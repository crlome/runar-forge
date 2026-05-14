use std::path::{Path, PathBuf};

use tree_sitter::{Query, QueryCursor, StreamingIterator};

use super::parsers::{make_parser, Lang};
use super::{ParsedExport, ParsedFile, ParsedImport};

/// Parse imports + exports from a file's source text using tree-sitter queries.
pub fn parse_file(source: &str, lang: Lang, file_path: &str) -> ParsedFile {
    let mut parser = make_parser(lang);
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => {
            return ParsedFile {
                file_path: file_path.to_string(),
                imports: vec![],
                exports: vec![],
                parse_errors: vec!["parse failed".into()],
            };
        }
    };

    let (import_query_src, export_query_src) = queries_for(lang);
    let language = lang.language();

    let mut imports = Vec::new();
    if let Ok(query) = Query::new(&language, import_query_src) {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let raw = cap
                    .node
                    .utf8_text(source.as_bytes())
                    .unwrap_or("")
                    .trim_matches(|c: char| c == '"' || c == '\'' || c == '`');
                if raw.is_empty() {
                    continue;
                }
                // For Rust, the captured argument is the full use-path
                // (e.g. `crate::types::Foo`, `std::fs`, `{a, b}`). Extract
                // the first segment.
                let text = if matches!(lang, Lang::Rust) {
                    first_rust_segment(raw)
                } else {
                    raw.to_string()
                };
                if text.is_empty() {
                    continue;
                }
                imports.push(to_parsed_import(&text, lang));
            }
        }
    }

    let mut exports = Vec::new();
    if let Some(export_src) = export_query_src {
        if let Ok(query) = Query::new(&language, export_src) {
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
            while let Some(m) = matches.next() {
                for cap in m.captures {
                    let name = cap
                        .node
                        .utf8_text(source.as_bytes())
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        continue;
                    }
                    exports.push(ParsedExport {
                        name,
                        is_default: false,
                        is_type_only: false,
                    });
                }
            }
        }
    }

    ParsedFile {
        file_path: file_path.to_string(),
        imports,
        exports,
        parse_errors: vec![],
    }
}

/// Extract the leftmost segment of a Rust use-path.
/// `crate::types::Foo` → `crate`, `std::fs` → `std`, `{a, b}` → "".
fn first_rust_segment(raw: &str) -> String {
    let trimmed = raw.trim().trim_start_matches('{');
    let first = trimmed.split("::").next().unwrap_or("").trim();
    // skip use-tree group prefixes like `{a, b}` with no leading segment
    if first.is_empty() || first.starts_with('{') {
        return String::new();
    }
    first.to_string()
}

fn to_parsed_import(source: &str, lang: Lang) -> ParsedImport {
    let is_relative = match lang {
        Lang::Rust => {
            source == "crate"
                || source == "super"
                || source == "self"
                || source.starts_with("crate::")
                || source.starts_with("super::")
                || source.starts_with("self::")
        }
        Lang::Go => source.starts_with('.') || source.starts_with('/'),
        _ => source.starts_with('.'),
    };
    ParsedImport {
        raw: source.to_string(),
        source: source.to_string(),
        is_relative,
        is_external: !is_relative,
        resolved_path: None,
        imported_names: vec![],
        is_default: false,
        is_side_effect: false,
        is_type_only: false,
    }
}

/// Returns (import_query, optional export_query) for the given language.
fn queries_for(lang: Lang) -> (&'static str, Option<&'static str>) {
    match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => (
            // imports: ES static import + re-export + dynamic import() + require()
            r#"
            (import_statement source: (string (string_fragment) @src))
            (export_statement source: (string (string_fragment) @src))
            (call_expression
              function: (identifier) @fn
              arguments: (arguments (string (string_fragment) @src))
              (#eq? @fn "require"))
            (call_expression
              function: (import)
              arguments: (arguments (string (string_fragment) @src)))
            "#,
            Some(
                r#"
                (export_statement
                  declaration: (function_declaration name: (identifier) @name))
                (export_statement
                  declaration: (class_declaration name: (type_identifier) @name))
                (export_statement
                  declaration: (lexical_declaration
                    (variable_declarator name: (identifier) @name)))
                (export_specifier name: (identifier) @name)
                "#,
            ),
        ),
        Lang::Python => (
            r#"
            (import_statement name: (dotted_name) @src)
            (import_from_statement module_name: (dotted_name) @src)
            (import_from_statement module_name: (relative_import) @src)
            "#,
            Some(
                r#"
                (function_definition name: (identifier) @name)
                (class_definition name: (identifier) @name)
                "#,
            ),
        ),
        Lang::Rust => (
            // capture the full argument; post-process to get the first segment
            r#"
            (use_declaration argument: (_) @src)
            "#,
            None,
        ),
        Lang::Go => (
            r#"
            (import_spec path: (interpreted_string_literal (interpreted_string_literal_content) @src))
            "#,
            None,
        ),
    }
}

/// Resolve a relative import string to a project-relative path, if it resolves
/// to a real file under `project_root`. Returns None for unresolved/external.
pub fn resolve_import(
    import_source: &str,
    from_absolute_path: &Path,
    project_root: &Path,
    lang: Lang,
) -> Option<String> {
    if !is_relative_source(import_source, lang) {
        return None;
    }

    let from_dir = from_absolute_path.parent()?;
    let candidate = from_dir.join(import_source);

    let extensions: &[&str] = match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            &["", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"]
        }
        Lang::Python => &["", ".py"],
        Lang::Rust => &["", ".rs"],
        Lang::Go => &["", ".go"],
    };

    for ext in extensions {
        let p: PathBuf = if ext.is_empty() {
            candidate.clone()
        } else {
            candidate.with_extension(ext.trim_start_matches('.'))
        };
        if p.is_file() {
            return relativize(&p, project_root);
        }
    }

    // try as directory with index.* (TS/JS barrel)
    if candidate.is_dir() {
        for idx in [
            "index.ts",
            "index.tsx",
            "index.js",
            "index.jsx",
            "mod.rs",
            "__init__.py",
        ] {
            let p = candidate.join(idx);
            if p.is_file() {
                return relativize(&p, project_root);
            }
        }
    }

    None
}

fn is_relative_source(source: &str, lang: Lang) -> bool {
    match lang {
        Lang::Rust => {
            source == "crate"
                || source == "super"
                || source == "self"
                || source.starts_with("crate::")
                || source.starts_with("super::")
                || source.starts_with("self::")
        }
        Lang::Go => source.starts_with('.') || source.starts_with('/'),
        _ => source.starts_with('.'),
    }
}

fn relativize(p: &Path, root: &Path) -> Option<String> {
    p.strip_prefix(root)
        .ok()
        .map(|rel| rel.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_imports() {
        let src = r#"
import { a } from './a'
import b from 'b'
const x = require('x')
const y = import('./dyn')
"#;
        let parsed = parse_file(src, Lang::TypeScript, "f.ts");
        let sources: Vec<&str> = parsed.imports.iter().map(|i| i.source.as_str()).collect();
        assert!(sources.contains(&"./a"));
        assert!(sources.contains(&"b"));
        assert!(sources.contains(&"x"));
        assert!(sources.contains(&"./dyn"));
    }

    #[test]
    fn python_imports() {
        let src = "from flask import Flask\nimport os\nfrom .local import thing\n";
        let parsed = parse_file(src, Lang::Python, "f.py");
        let sources: Vec<&str> = parsed.imports.iter().map(|i| i.source.as_str()).collect();
        assert!(sources.contains(&"flask"));
        assert!(sources.contains(&"os"));
    }

    #[test]
    fn rust_imports() {
        let src = "use std::fs;\nuse crate::types;\nuse super::scanner;\n";
        let parsed = parse_file(src, Lang::Rust, "f.rs");
        let sources: Vec<&str> = parsed.imports.iter().map(|i| i.source.as_str()).collect();
        assert!(sources.contains(&"std"));
        assert!(sources.contains(&"crate"));
        assert!(sources.contains(&"super"));
        let crate_import = parsed.imports.iter().find(|i| i.source == "crate").unwrap();
        assert!(crate_import.is_relative);
    }

    #[test]
    fn go_imports() {
        let src = "package main\nimport (\n  \"fmt\"\n  \"net/http\"\n)\n";
        let parsed = parse_file(src, Lang::Go, "f.go");
        let sources: Vec<&str> = parsed.imports.iter().map(|i| i.source.as_str()).collect();
        assert!(sources.contains(&"fmt"));
        assert!(sources.contains(&"net/http"));
    }

    #[test]
    fn ts_reexports() {
        let src = "export { foo } from './foo'\nexport { bar } from './bar'\n";
        let parsed = parse_file(src, Lang::TypeScript, "index.ts");
        let sources: Vec<&str> = parsed.imports.iter().map(|i| i.source.as_str()).collect();
        assert!(sources.contains(&"./foo"));
        assert!(sources.contains(&"./bar"));
        assert!(parsed.exports.len() >= 2);
    }

    #[test]
    fn ts_exports() {
        let src = "export function foo() {}\nexport class Bar {}\nexport const baz = 1\n";
        let parsed = parse_file(src, Lang::TypeScript, "f.ts");
        let names: Vec<&str> = parsed.exports.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"Bar"));
        assert!(names.contains(&"baz"));
    }
}
