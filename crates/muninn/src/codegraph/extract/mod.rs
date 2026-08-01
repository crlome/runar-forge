//! Definition and call-site extraction.
//!
//! Adding a language is meant to be data, not code: a [`LangSpec`] carries the
//! tree-sitter queries and the handful of node-type names a generic walk needs,
//! and everything below consumes them uniformly.

mod spec_go;
mod spec_rust;
mod spec_ts;

use std::collections::HashMap;
use std::sync::OnceLock;

use tree_sitter::{Node, Query, QueryCursor, StreamingIterator, Tree};

use crate::huginn::graph::parsers::{make_parser, Lang};

use super::metrics::measure;
use super::{
    qualified_name, EdgeKind, FileExtract, RawCall, RawImport, RawRelation, RawSymbol, SymbolLabel,
};

/// Per-language extraction rules.
///
/// Capture-name convention, relied on by the walker:
/// - `@def.<label>` marks the *name* node of a definition; `<label>` parses via
///   [`SymbolLabel::from_capture`].
/// - `@def.body` optionally marks the node to measure for complexity.
/// - `@def.params` optionally marks the parameter list.
/// - `@def.receiver` optionally names the container directly, for languages
///   where a method is not written inside the type it belongs to. Go declares
///   `func (s *Server) Handle()` at file scope, so walking ancestors finds
///   nothing; the receiver type is a sibling field of the declaration.
/// - `@call.name` marks a callee name; `@call.qualifier` its receiver.
/// - `@import.source` marks a module path; `@import.name` a bound local name.
/// - `@rel.subject` / `@rel.object` mark the two ends of an inherit/implement.
pub struct LangSpec {
    pub lang: Lang,
    pub defs: &'static str,
    pub calls: &'static str,
    pub imports: &'static str,
    pub relations: &'static str,
    /// Node kinds that introduce a named container for the symbols inside them.
    pub container_kinds: &'static [&'static str],
    /// Field name holding a container's own name, tried in order.
    pub container_name_fields: &'static [&'static str],
    /// Node kinds counted as a branch for cyclomatic complexity.
    pub branch_kinds: &'static [&'static str],
    /// Node kinds counted as a loop.
    pub loop_kinds: &'static [&'static str],
    /// Whether a `relations` match should be read as Implements rather than
    /// Inherits.
    pub relation_kind: EdgeKind,
    /// Whether an initial capital is what makes a name visible outside its
    /// module. True for Go, where there is no `pub` or `export` keyword to
    /// look for and the identifier itself carries the visibility.
    pub capitalized_names_are_exported: bool,
}

pub fn spec_for(lang: Lang) -> &'static LangSpec {
    match lang {
        Lang::Rust => spec_rust::SPEC,
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => spec_ts::spec_for(lang),
        Lang::Go => spec_go::SPEC,
        // Python lands in the next slice; until then it is recorded as an
        // explicit coverage gap rather than silently producing nothing.
        Lang::Python => spec_ts::spec_for(Lang::TypeScript),
    }
}

/// Languages this build can actually extract from. Anything else is reported
/// as `skipped_lang` so the gap is counted.
pub fn is_supported(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Rust | Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Go
    )
}

struct Compiled {
    defs: Option<Query>,
    calls: Option<Query>,
    imports: Option<Query>,
    relations: Option<Query>,
}

fn compiled(lang: Lang) -> &'static Compiled {
    static CACHE: OnceLock<HashMap<Lang, Compiled>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let mut m = HashMap::new();
        for lang in [
            Lang::Rust,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Python,
            Lang::Go,
        ] {
            let spec = spec_for(lang);
            let language = lang.language();
            m.insert(
                lang,
                Compiled {
                    defs: Query::new(&language, spec.defs).ok(),
                    calls: Query::new(&language, spec.calls).ok(),
                    imports: Query::new(&language, spec.imports).ok(),
                    relations: Query::new(&language, spec.relations).ok(),
                },
            );
        }
        m
    });
    cache.get(&lang).expect("every Lang is compiled above")
}

/// Parse one file into definitions, call sites, imports and relations.
pub fn extract_file(source: &str, lang: Lang, file_path: &str) -> FileExtract {
    let spec = spec_for(lang);
    let queries = compiled(lang);
    let mut parser = make_parser(lang);
    let tree: Tree = match parser.parse(source, None) {
        Some(t) => t,
        None => {
            return FileExtract {
                had_errors: true,
                ..Default::default()
            }
        }
    };
    let bytes = source.as_bytes();
    let root = tree.root_node();

    let symbols = collect_symbols(&root, bytes, spec, queries, file_path);
    let calls = collect_calls(&root, bytes, spec, queries, file_path, &symbols);
    let imports = collect_imports(&root, bytes, queries);
    let relations = collect_relations(&root, bytes, spec, queries);

    FileExtract {
        symbols,
        calls,
        imports,
        relations,
        had_errors: root.has_error(),
    }
}

fn text<'a>(node: Node<'_>, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

/// First line of a declaration, trimmed and length-capped.
///
/// Everything after the first `=` is dropped: for a const or static the
/// declaration line carries its initializer, and that is exactly where a
/// hardcoded credential lives. No value from the source reaches storage.
fn signature_of(node: Node<'_>, bytes: &[u8]) -> String {
    let raw = text(node, bytes);
    let first = raw.lines().next().unwrap_or("");
    let head = match first.find('=') {
        Some(i) => &first[..i],
        None => first,
    };
    crate::text::truncate_ellipsis(head.trim(), 200)
}

/// Walk up to the nearest container node and read its name.
///
/// A container is never its own container: `trait Greeter` reaches `trait_item`
/// through that node's own name field, and treating it as enclosing would name
/// the trait `Greeter.Greeter`.
fn container_of(node: Node<'_>, bytes: &[u8], spec: &LangSpec) -> Option<String> {
    let mut cur = node.parent();
    'outer: while let Some(n) = cur {
        if spec.container_kinds.contains(&n.kind()) {
            for field in spec.container_name_fields {
                if let Some(name_node) = n.child_by_field_name(field) {
                    if name_node.id() == node.id() {
                        cur = n.parent();
                        continue 'outer;
                    }
                    let name = base_type_name(text(name_node, bytes));
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
        }
        cur = n.parent();
    }
    None
}

/// Strip type arguments so `impl<T> Foo<T>` contributes the same container name
/// as the `Foo` definition itself; otherwise nothing would ever match it.
fn base_type_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let base = trimmed.split('<').next().unwrap_or(trimmed).trim();
    base.rsplit("::").next().unwrap_or(base).trim().to_string()
}

fn collect_symbols(
    root: &Node<'_>,
    bytes: &[u8],
    spec: &LangSpec,
    queries: &Compiled,
    file_path: &str,
) -> Vec<RawSymbol> {
    let query = match &queries.defs {
        Some(q) => q,
        None => return Vec::new(),
    };
    let mut out: Vec<RawSymbol> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, bytes);

    while let Some(m) = matches.next() {
        let mut name_node: Option<Node<'_>> = None;
        let mut label: Option<SymbolLabel> = None;
        let mut body: Option<Node<'_>> = None;
        let mut params: Option<Node<'_>> = None;
        let mut receiver: Option<Node<'_>> = None;

        for cap in m.captures {
            let cap_name = &query.capture_names()[cap.index as usize];
            match cap_name.strip_prefix("def.") {
                Some("body") => body = Some(cap.node),
                Some("params") => params = Some(cap.node),
                Some("receiver") => receiver = Some(cap.node),
                Some(suffix) => {
                    if let Some(l) = SymbolLabel::from_capture(suffix) {
                        name_node = Some(cap.node);
                        label = Some(l);
                    }
                }
                None => {}
            }
        }

        let (name_node, label) = match (name_node, label) {
            (Some(n), Some(l)) => (n, l),
            _ => continue,
        };
        let name = text(name_node, bytes).trim().to_string();
        if name.is_empty() {
            continue;
        }

        // A method is a function that happens to sit inside a container; the
        // query does not have to know the difference. Where the language writes
        // the container beside the declaration instead of around it, an
        // explicit receiver capture supplies it and the ancestor walk — which
        // would find the file and stop — is not consulted.
        let container = receiver
            .map(|n| base_type_name(text(n, bytes)))
            .filter(|c| !c.is_empty())
            .or_else(|| container_of(name_node, bytes, spec));
        let label = match (label, container.is_some()) {
            (SymbolLabel::Function, true) => SymbolLabel::Method,
            (l, _) => l,
        };

        let decl = name_node.parent().unwrap_or(name_node);
        let metrics = measure(body, params, spec);

        let exported = is_exported(decl, bytes, &name, spec);
        out.push(RawSymbol {
            name,
            label,
            container,
            start_line: decl.start_position().row as u32 + 1,
            end_line: decl.end_position().row as u32 + 1,
            signature: signature_of(decl, bytes),
            exported,
            metrics,
        });
    }

    // Two queries can match the same declaration; identity is the qualified
    // name, so collapse on it and keep the richer label. `dedup_by` keeps the
    // first of each run, so richness has to sort ahead of line order — before
    // it did, whichever pattern happened to match first won.
    out.sort_by(|a, b| {
        qualified_name(file_path, a.container.as_deref(), &a.name)
            .cmp(&qualified_name(file_path, b.container.as_deref(), &b.name))
            .then(a.label.richness().cmp(&b.label.richness()))
            .then(a.start_line.cmp(&b.start_line))
    });
    out.dedup_by(|a, b| {
        qualified_name(file_path, a.container.as_deref(), &a.name)
            == qualified_name(file_path, b.container.as_deref(), &b.name)
    });
    out
}

/// Visibility is a syntactic property in every language handled here.
///
/// The JS/TS test is the declaration's own parent chain up to the first
/// statement boundary — scanning preceding siblings instead would mark every
/// declaration after the file's first `export` as exported, and walking all
/// ancestors would do the same to anything nested inside an exported function.
///
/// Go states visibility in the name itself, so there is no keyword to find and
/// the name is the whole test.
fn is_exported(decl: Node<'_>, bytes: &[u8], name: &str, spec: &LangSpec) -> bool {
    if spec.capitalized_names_are_exported {
        return name.chars().next().is_some_and(char::is_uppercase);
    }
    let head = text(decl, bytes).lines().next().unwrap_or("");
    let head = head.split('=').next().unwrap_or(head);
    if head.contains("pub ") || head.contains("pub(") {
        return true;
    }
    let mut cur = decl.parent();
    while let Some(n) = cur {
        match n.kind() {
            "export_statement" => return true,
            // A declaration reaches its export through declaration wrappers
            // only; anything else means we have left the statement.
            "lexical_declaration"
            | "variable_declaration"
            | "variable_declarator"
            | "declaration" => cur = n.parent(),
            _ => return false,
        }
    }
    false
}

fn collect_calls(
    root: &Node<'_>,
    bytes: &[u8],
    spec: &LangSpec,
    queries: &Compiled,
    file_path: &str,
    symbols: &[RawSymbol],
) -> Vec<RawCall> {
    let query = match &queries.calls {
        Some(q) => q,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, bytes);

    while let Some(m) = matches.next() {
        let mut callee: Option<Node<'_>> = None;
        let mut qualifier: Option<String> = None;
        for cap in m.captures {
            match query.capture_names()[cap.index as usize] {
                "call.name" => callee = Some(cap.node),
                "call.qualifier" => {
                    let q = text(cap.node, bytes).trim();
                    if !q.is_empty() {
                        qualifier = Some(q.to_string());
                    }
                }
                _ => {}
            }
        }
        let callee_node = match callee {
            Some(n) => n,
            None => continue,
        };
        let name = text(callee_node, bytes).trim().to_string();
        if name.is_empty() {
            continue;
        }
        let line = callee_node.start_position().row as u32 + 1;
        out.push(RawCall {
            caller: enclosing_symbol(callee_node, bytes, spec, file_path, symbols),
            callee: name,
            qualifier,
            line,
        });
    }
    out
}

/// The definition a node sits inside, as a qualified name. Falls back to
/// `None` for a call at file scope.
fn enclosing_symbol(
    node: Node<'_>,
    bytes: &[u8],
    spec: &LangSpec,
    file_path: &str,
    symbols: &[RawSymbol],
) -> Option<String> {
    let line = node.start_position().row as u32 + 1;
    // The innermost definition whose line range covers this node.
    symbols
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .min_by_key(|s| s.end_line - s.start_line)
        .map(|s| qualified_name(file_path, s.container.as_deref(), &s.name))
        .or_else(|| container_of(node, bytes, spec).map(|c| qualified_name(file_path, None, &c)))
}

fn collect_imports(root: &Node<'_>, bytes: &[u8], queries: &Compiled) -> Vec<RawImport> {
    let query = match &queries.imports {
        Some(q) => q,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, bytes);

    while let Some(m) = matches.next() {
        let mut source: Option<String> = None;
        let mut names: Vec<String> = Vec::new();
        for cap in m.captures {
            let raw = text(cap.node, bytes)
                .trim()
                .trim_matches(|c: char| c == '"' || c == '\'' || c == '`');
            if raw.is_empty() {
                continue;
            }
            match query.capture_names()[cap.index as usize] {
                "import.source" => source = Some(raw.to_string()),
                "import.name" => names.push(raw.to_string()),
                _ => {}
            }
        }
        let Some(source) = source else { continue };
        if names.is_empty() {
            // A whole-module import still binds the trailing segment.
            if let Some(last) = source.rsplit(['/', ':']).find(|s| !s.is_empty()) {
                names.push(last.to_string());
            }
        }
        for local_name in names {
            out.push(RawImport {
                local_name,
                source: source.clone(),
            });
        }
    }
    out
}

fn collect_relations(
    root: &Node<'_>,
    bytes: &[u8],
    spec: &LangSpec,
    queries: &Compiled,
) -> Vec<RawRelation> {
    let query = match &queries.relations {
        Some(q) => q,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, bytes);

    while let Some(m) = matches.next() {
        let mut subject = None;
        let mut object = None;
        for cap in m.captures {
            let value = text(cap.node, bytes).trim().to_string();
            if value.is_empty() {
                continue;
            }
            match query.capture_names()[cap.index as usize] {
                "rel.subject" => subject = Some(value),
                "rel.object" => object = Some(value),
                _ => {}
            }
        }
        if let (Some(subject), Some(object)) = (subject, object) {
            // Definitions are named without type arguments, so `impl<T> Foo<T>`
            // has to be normalised the same way or its relation never matches.
            out.push(RawRelation {
                subject: base_type_name(&subject),
                object: base_type_name(&object),
                kind: spec.relation_kind,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_never_carries_a_literal_value() {
        // The declaration line of a const holds its initializer, which is
        // exactly where a hardcoded credential sits.
        let src = concat!(
            "pub const API_KEY: &str = \"sk-live-super-secret-value\";\n",
            "static TOKEN: &str = \"ghp_anothersecret\";\n",
            "pub fn ordinary(a: u32) -> u32 { a }\n"
        );
        let out = extract_file(src, Lang::Rust, "src/keys.rs");
        assert!(!out.symbols.is_empty());
        for sym in &out.symbols {
            assert!(
                !sym.signature.contains("sk-live") && !sym.signature.contains("ghp_"),
                "signature leaked a literal: {:?}",
                sym.signature
            );
        }
        let api = out.symbols.iter().find(|s| s.name == "API_KEY").unwrap();
        assert!(api.signature.contains("API_KEY"), "got {:?}", api.signature);
        assert!(api.exported, "pub const should read as exported");
    }

    #[test]
    fn only_the_exported_declaration_is_marked_exported() {
        // Scanning preceding siblings would mark everything after the file's
        // first `export` as exported.
        let src = concat!(
            "export function first() { return 1 }\n",
            "function second() { return 2 }\n",
            "export function third() { return 3 }\n"
        );
        let out = extract_file(src, Lang::TypeScript, "src/m.ts");
        let by = |n: &str| out.symbols.iter().find(|s| s.name == n).unwrap().exported;
        assert!(by("first"));
        assert!(!by("second"), "a later declaration inherited the export");
        assert!(by("third"));
    }

    /// A query that fails to compile silently yields nothing at runtime, so
    /// grammar drift has to fail here instead.
    ///
    /// Driven off `is_supported` rather than a hand-written list, because a
    /// hand-written list is exactly how a newly added language gets shipped
    /// without ever being checked.
    ///
    /// Only supported languages are asserted. An unsupported one borrows
    /// another language's spec, and that spec does *not* compile against the
    /// borrowed grammar — Python's does not — but nothing extracts from it
    /// either: `is_supported` gates the call, and the file is counted as
    /// `skipped_lang`. Requiring it to compile would assert something the
    /// system never relies on.
    #[test]
    fn every_spec_query_compiles() {
        let all = [
            Lang::Rust,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Python,
            Lang::Go,
        ];
        assert!(
            all.iter().copied().filter(|l| is_supported(*l)).count() >= 5,
            "a supported language dropped out of the compilation check"
        );
        for lang in all.into_iter().filter(|l| is_supported(*l)) {
            let spec = spec_for(lang);
            let language = lang.language();
            for (what, src) in [
                ("defs", spec.defs),
                ("calls", spec.calls),
                ("imports", spec.imports),
                ("relations", spec.relations),
            ] {
                Query::new(&language, src)
                    .unwrap_or_else(|e| panic!("{lang:?} {what} query failed to compile: {e}"));
            }
        }
    }
}
