//! Symbol-level code graph.
//!
//! Derived, regenerable, per-machine data: it lives in its own SQLite file
//! rather than in `memory_entries`, so a large project's symbols never compete
//! with curated memories for a slot in recall, and the storage trait does not
//! grow a second implementation for a backend this never needs.

pub mod extract;
pub mod format;
pub mod index;
pub mod metrics;
pub mod resolve;
pub mod store;

/// What kind of definition a symbol is. Stored as a string so the query layer
/// can boost by it without a join.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolLabel {
    Function,
    Method,
    Class,
    Interface,
    Trait,
    Struct,
    Enum,
    Type,
    Const,
    Module,
    /// A data member of a struct, class or interface. Never callable — see
    /// `is_callable` in `resolve.rs`.
    Field,
}

impl SymbolLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolLabel::Function => "Function",
            SymbolLabel::Method => "Method",
            SymbolLabel::Class => "Class",
            SymbolLabel::Interface => "Interface",
            SymbolLabel::Trait => "Trait",
            SymbolLabel::Struct => "Struct",
            SymbolLabel::Enum => "Enum",
            SymbolLabel::Type => "Type",
            SymbolLabel::Const => "Const",
            SymbolLabel::Module => "Module",
            SymbolLabel::Field => "Field",
        }
    }

    /// Parse the suffix of a tree-sitter capture name (`@def.function`).
    pub fn from_capture(suffix: &str) -> Option<Self> {
        Some(match suffix {
            "function" => SymbolLabel::Function,
            "method" => SymbolLabel::Method,
            "class" => SymbolLabel::Class,
            "interface" => SymbolLabel::Interface,
            "trait" => SymbolLabel::Trait,
            "struct" => SymbolLabel::Struct,
            "enum" => SymbolLabel::Enum,
            "type" => SymbolLabel::Type,
            "const" => SymbolLabel::Const,
            "module" => SymbolLabel::Module,
            "field" => SymbolLabel::Field,
            _ => return None,
        })
    }

    /// Round-trip the stored string. Unknown labels read as `Function`, which
    /// is the safest wrong answer: it is callable and owns nothing.
    pub fn from_stored(s: &str) -> Self {
        match s {
            "Method" => SymbolLabel::Method,
            "Class" => SymbolLabel::Class,
            "Interface" => SymbolLabel::Interface,
            "Trait" => SymbolLabel::Trait,
            "Struct" => SymbolLabel::Struct,
            "Enum" => SymbolLabel::Enum,
            "Type" => SymbolLabel::Type,
            "Const" => SymbolLabel::Const,
            "Module" => SymbolLabel::Module,
            "Field" => SymbolLabel::Field,
            _ => SymbolLabel::Function,
        }
    }

    /// Whether this label can own methods, which is what makes it a container
    /// for qualified names and for the suffix resolution tier.
    pub fn is_container(self) -> bool {
        matches!(
            self,
            SymbolLabel::Class
                | SymbolLabel::Interface
                | SymbolLabel::Trait
                | SymbolLabel::Struct
                | SymbolLabel::Enum
        )
    }

    /// Ordering for collapsing two matches on the same qualified name, lower
    /// being richer. A declaration can match more than one pattern — a TS class
    /// field bound to an arrow function is both a field and a callable — and
    /// the callable reading carries a body, parameters and metrics, so it is
    /// the one worth keeping.
    pub fn richness(self) -> u8 {
        match self {
            SymbolLabel::Method | SymbolLabel::Function => 0,
            SymbolLabel::Class
            | SymbolLabel::Interface
            | SymbolLabel::Trait
            | SymbolLabel::Struct
            | SymbolLabel::Enum => 1,
            SymbolLabel::Type | SymbolLabel::Module => 2,
            SymbolLabel::Field => 3,
            SymbolLabel::Const => 4,
        }
    }
}

/// Cheap per-function complexity signals, all from one walk of the body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SymbolMetrics {
    pub complexity: u32,
    pub cognitive: u32,
    pub loop_depth: u32,
    pub param_count: u32,
}

/// A definition as extracted, before it is assigned an id or resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSymbol {
    pub name: String,
    pub label: SymbolLabel,
    /// Enclosing type — the `Foo` in `Foo::bar` / `Foo.bar`.
    pub container: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// Declaration line only. Bodies and literals are never stored, which
    /// keeps secrets out of this database by construction.
    pub signature: String,
    pub exported: bool,
    pub metrics: SymbolMetrics,
}

/// A call site. The callee is unresolved at this stage — resolution happens
/// once, project-wide, after every file has been parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct RawCall {
    /// Qualified name of the enclosing definition, or `None` at file scope.
    pub caller: Option<String>,
    pub callee: String,
    /// Receiver or module path written before the callee, if any.
    pub qualifier: Option<String>,
    pub line: u32,
}

/// One name brought into a file's scope.
#[derive(Debug, Clone, PartialEq)]
pub struct RawImport {
    /// The name as bound locally, which is what a call site will reference.
    pub local_name: String,
    /// Module path exactly as written.
    pub source: String,
}

/// An inheritance or trait-implementation relation between two named types.
#[derive(Debug, Clone, PartialEq)]
pub struct RawRelation {
    pub subject: String,
    pub object: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    Calls,
    Inherits,
    Implements,
}

impl EdgeKind {
    pub fn from_stored(s: &str) -> Self {
        match s {
            "INHERITS" => EdgeKind::Inherits,
            "IMPLEMENTS" => EdgeKind::Implements,
            _ => EdgeKind::Calls,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "CALLS",
            EdgeKind::Inherits => "INHERITS",
            EdgeKind::Implements => "IMPLEMENTS",
        }
    }
}

/// Everything one file yields in a single parse.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileExtract {
    pub symbols: Vec<RawSymbol>,
    pub calls: Vec<RawCall>,
    pub imports: Vec<RawImport>,
    pub relations: Vec<RawRelation>,
    /// The tree contained ERROR nodes: what follows is partial, and the
    /// coverage table records it so absence of a symbol is never read as proof
    /// the symbol does not exist.
    pub had_errors: bool,
}

/// Why a file is or is not represented in the graph. Recorded for every file
/// the scanner yields, so unsupported languages are a counted gap rather than
/// a silent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Indexed,
    Partial,
    SkippedLang,
    Error,
}

impl FileStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FileStatus::Indexed => "indexed",
            FileStatus::Partial => "partial",
            FileStatus::SkippedLang => "skipped_lang",
            FileStatus::Error => "error",
        }
    }
}

/// How a call was matched to a definition. Every edge carries one, so a
/// consumer can tell a certain edge from a plausible one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The callee's local name resolves through the caller file's imports.
    ImportMap,
    /// Defined in the same file.
    SameModule,
    /// Exactly one definition project-wide carries that name.
    UniqueName,
    /// A qualified call whose method name exists on exactly one container.
    Suffix,
    /// The name is declared on one trait and otherwise defined only by types
    /// that implement it, so every candidate is the same method and the trait
    /// declaration is its identity.
    TraitMethod,
}

impl Resolution {
    pub fn confidence(self) -> f64 {
        match self {
            Resolution::ImportMap => 0.95,
            Resolution::SameModule => 0.90,
            Resolution::UniqueName => 0.75,
            Resolution::Suffix => 0.55,
            // Stronger than the suffix tier, which picks one of several
            // genuinely different methods, and weaker than a unique name, which
            // has no rival definition at all. Here the rivals exist but are
            // provably the same method under a different impl.
            Resolution::TraitMethod => 0.70,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::ImportMap => "import_map",
            Resolution::SameModule => "same_module",
            Resolution::UniqueName => "unique_name",
            Resolution::Suffix => "suffix",
            Resolution::TraitMethod => "trait_method",
        }
    }
}

/// Stable identity for a definition: `path:Container.name`, or `path:name` at
/// file scope. Language-agnostic on purpose — per-language canonical forms
/// would have to agree with the resolver, and this only has to be unique.
pub fn qualified_name(file_path: &str, container: Option<&str>, name: &str) -> String {
    match container {
        Some(c) if !c.is_empty() => format!("{file_path}:{c}.{name}"),
        _ => format!("{file_path}:{name}"),
    }
}

/// Split an identifier into search tokens: camelCase and snake_case parts plus
/// the original, so `parseFileCache` is found by "parse", "file" or "cache".
pub fn name_tokens(name: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for part in name.split(['_', '-', ':', '.']) {
        if part.is_empty() {
            continue;
        }
        let chars: Vec<char> = part.chars().collect();
        let mut current = String::new();
        for (i, &ch) in chars.iter().enumerate() {
            // Break before an uppercase that starts a new word: either it
            // follows a lowercase (`parseFile`) or it ends a run of uppercase
            // and is followed by one (`HTTPServer` → http + server).
            let starts_word = ch.is_uppercase()
                && !current.is_empty()
                && (!chars[i - 1].is_uppercase()
                    || chars.get(i + 1).is_some_and(|n| n.is_lowercase()));
            if starts_word {
                out.push(std::mem::take(&mut current));
            }
            current.extend(ch.to_lowercase());
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out.retain(|t| t.len() > 1);
    let lowered = name.to_lowercase();
    if !out.contains(&lowered) {
        out.push(lowered);
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_names_distinguish_methods_from_free_functions() {
        assert_eq!(qualified_name("src/a.rs", None, "run"), "src/a.rs:run");
        assert_eq!(
            qualified_name("src/a.rs", Some("Engine"), "run"),
            "src/a.rs:Engine.run"
        );
        assert_eq!(qualified_name("src/a.rs", Some(""), "run"), "src/a.rs:run");
    }

    #[test]
    fn name_tokens_split_both_conventions() {
        assert_eq!(
            name_tokens("parseFileCache"),
            "parse file cache parsefilecache"
        );
        assert_eq!(name_tokens("build_state"), "build state build_state");
        assert_eq!(name_tokens("HTTPServer"), "http server httpserver");
    }

    #[test]
    fn name_tokens_never_emit_single_letters() {
        // `a` would match nearly every query, so short fragments are dropped.
        assert_eq!(name_tokens("aB"), "ab");
    }

    #[test]
    fn resolution_confidence_is_ordered_by_certainty() {
        assert!(Resolution::ImportMap.confidence() > Resolution::SameModule.confidence());
        assert!(Resolution::SameModule.confidence() > Resolution::UniqueName.confidence());
        assert!(Resolution::UniqueName.confidence() > Resolution::Suffix.confidence());
    }
}
