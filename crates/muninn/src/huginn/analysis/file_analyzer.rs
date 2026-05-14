use std::fs;
use std::path::Path;

use crate::huginn::analyzer::AnalysisDepth;
use crate::huginn::graph::{import_parser::parse_file, parsers::Lang, DependencyGraph};
use crate::huginn::scanner::FileEntry;

use super::techdebt;
use super::{
    AnalysisBody, DeepAnalysis, FileAnalysisResult, LightAnalysis, MediumAnalysis, TraitImpl,
};

const LIGHT_LINE_LIMIT: usize = 50;

pub fn analyze_file(
    file: &FileEntry,
    depth: AnalysisDepth,
    graph: &DependencyGraph,
) -> FileAnalysisResult {
    // Markdown short-circuit: produce a structured Deep body from heading
    // hierarchy / paragraph / bullet analysis instead of the generic code
    // path which yields a near-empty result for `.md` files.
    if matches!(depth, AnalysisDepth::Deep | AnalysisDepth::Medium)
        && (file.extension == "md" || file.extension == "mdx")
    {
        let content = fs::read_to_string(&file.path).unwrap_or_default();
        let md = super::markdown_analyzer::analyze(&content, &file.relative_path);
        let deep = super::markdown_analyzer::into_deep(md);
        return FileAnalysisResult::from_file(file, depth, AnalysisBody::Deep(deep));
    }

    // SQL short-circuit: attach schema analysis (tables, alters, indexes,
    // migration metadata) so the memory-entry generator can emit one entry
    // per table plus an overview entry.
    if !matches!(depth, AnalysisDepth::Skip) && file.extension == "sql" {
        let content = fs::read_to_string(&file.path).unwrap_or_default();
        let sql = super::sql_analyzer::analyze(&content, &file.relative_path);
        let mut result = FileAnalysisResult::from_file(
            file,
            depth,
            AnalysisBody::Light(LightAnalysis {
                purpose: sql_purpose(&sql, &file.relative_path),
                relationships: Vec::new(),
            }),
        );
        result.sql_analysis = Some(sql);
        return result;
    }

    let body = match depth {
        AnalysisDepth::Deep => AnalysisBody::Deep(analyze_deep(file, graph)),
        AnalysisDepth::Medium => AnalysisBody::Medium(analyze_medium(file, graph)),
        AnalysisDepth::Light => AnalysisBody::Light(analyze_light(file, graph)),
        AnalysisDepth::Skip => AnalysisBody::Skip,
    };
    let mut result = FileAnalysisResult::from_file(file, depth, body);
    // Trait-impl pairs are cheap to extract and needed even for
    // Medium/Light-classified files (adapters often score Medium).
    if file.extension == "rs" && !matches!(result.depth, AnalysisDepth::Skip) {
        if let Ok(content) = fs::read_to_string(&file.path) {
            result.rust_trait_impls = extract_rust_trait_impls(&content);
        }
    }
    result
}

fn analyze_deep(file: &FileEntry, graph: &DependencyGraph) -> DeepAnalysis {
    let content = fs::read_to_string(&file.path).unwrap_or_default();

    let lang = Lang::from_extension(&file.extension);
    let (key_imports, key_exports) = if let Some(lang) = lang {
        let parsed = parse_file(&content, lang, &file.relative_path);
        let is_test = is_test_file(&file.relative_path);
        let exports: Vec<String> = if is_test {
            vec![]
        } else {
            parsed.exports.iter().map(|e| e.name.clone()).collect()
        };
        let imports: Vec<String> = parsed.imports.iter().map(|i| i.source.clone()).collect();
        (imports, exports)
    } else {
        (vec![], vec![])
    };

    let purpose = infer_purpose(&file.relative_path, &content, &key_exports);
    let responsibilities = extract_responsibilities(&content);
    let patterns = detect_design_patterns(&content, &file.relative_path);
    let business_rules = extract_business_rules(&content);
    let gotchas = extract_gotchas(&content);
    let security_notes = extract_security_notes(&content);
    let relationships = build_relationships(&file.relative_path, graph);
    let tech_debt_markers = techdebt::extract(&content, &file.relative_path);

    DeepAnalysis {
        purpose,
        responsibilities,
        key_exports,
        key_imports,
        patterns,
        business_rules,
        gotchas,
        security_notes,
        relationships,
        tech_debt_markers,
    }
}

fn sql_purpose(sql: &super::sql_analyzer::SqlAnalysis, relative_path: &str) -> String {
    let seq = sql
        .migration_sequence
        .as_deref()
        .map(|s| format!("Migration {s}: "))
        .unwrap_or_default();
    let name = sql
        .migration_name
        .as_deref()
        .unwrap_or_else(|| relative_path.rsplit('/').next().unwrap_or(relative_path));
    let tables = sql.tables.len();
    let alters = sql.alters.len();
    let indexes = sql.indexes.len();
    format!("{seq}{name} — {tables} table(s), {alters} alter(s), {indexes} index(es)")
}

/// Match `impl [<generics>] Trait[<args>] for Type[<args>]` across a Rust
/// source file. Keeps only the trait and type base names (strips generic
/// parameters) so downstream grouping works by identity.
///
/// Deliberately text-based rather than tree-sitter-based: the analyzer
/// already has the raw source in hand, and a regex pass is cheap. This is
/// sufficient for the storage-adapter / trait-polymorphism detector; more
/// precise AST-level analysis can replace it when needed.
fn extract_rust_trait_impls(src: &str) -> Vec<TraitImpl> {
    use regex::Regex;
    use std::sync::OnceLock;

    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // `impl` (opt. generics) Trait (opt. generics) `for` Type (opt. generics)
        Regex::new(
            r"(?m)^\s*impl(?:\s*<[^>]*>)?\s+([A-Za-z_][\w:]*)(?:\s*<[^>]*>)?\s+for\s+([A-Za-z_][\w:]*)",
        )
        .expect("valid regex")
    });

    let mut out = Vec::new();
    for caps in re.captures_iter(src) {
        let trait_name = caps
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let type_name = caps
            .get(2)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if trait_name.is_empty() || type_name.is_empty() {
            continue;
        }
        out.push(TraitImpl {
            trait_name,
            type_name,
        });
    }
    out
}

fn analyze_medium(file: &FileEntry, graph: &DependencyGraph) -> MediumAnalysis {
    let content = fs::read_to_string(&file.path).unwrap_or_default();
    let lang = Lang::from_extension(&file.extension);
    let (import_sources, key_exports) = if let Some(lang) = lang {
        let parsed = parse_file(&content, lang, &file.relative_path);
        let is_test = is_test_file(&file.relative_path);
        let exports: Vec<String> = if is_test {
            vec![]
        } else {
            parsed.exports.iter().map(|e| e.name.clone()).collect()
        };
        (
            parsed.imports.iter().map(|i| i.source.clone()).collect(),
            exports,
        )
    } else {
        (vec![], vec![])
    };
    MediumAnalysis {
        purpose: infer_purpose(&file.relative_path, &content, &key_exports),
        key_exports,
        import_sources,
        relationships: build_relationships(&file.relative_path, graph),
    }
}

fn analyze_light(file: &FileEntry, graph: &DependencyGraph) -> LightAnalysis {
    let content = read_head(&file.path, LIGHT_LINE_LIMIT);
    LightAnalysis {
        purpose: infer_purpose(&file.relative_path, &content, &[]),
        relationships: build_relationships(&file.relative_path, graph),
    }
}

fn read_head(path: &Path, max_lines: usize) -> String {
    let content = fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_test_file(path: &str) -> bool {
    path.contains(".spec.") || path.contains(".test.")
}

fn infer_purpose(file_path: &str, content: &str, exports: &[String]) -> String {
    // File-level doc comment (@description)
    if let Some(head) = content.get(..500.min(content.len())) {
        if let Some(idx) = head.find("@description") {
            let line = head[idx + "@description".len()..]
                .lines()
                .next()
                .unwrap_or("")
                .trim();
            if !line.is_empty() {
                return line.to_string();
            }
        }
    }

    // First non-shebang line comment
    for line in content.lines().take(10) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("#!") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("//") {
            let body = rest.trim_start_matches('/').trim();
            if body.len() > 10 {
                return body.to_string();
            }
        }
        break;
    }

    // File-type + first export
    let basename = Path::new(file_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if let Some(first) = exports.first() {
        return format!("module providing {first}");
    }
    basename
}

fn extract_responsibilities(content: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        // export function/async function name
        if let Some(rest) = trimmed.strip_prefix("export ") {
            let rest = rest
                .trim_start_matches("async ")
                .trim_start_matches("default ");
            for kw in ["function ", "class ", "const "] {
                if let Some(after) = rest.strip_prefix(kw) {
                    if let Some(n) = take_ident(after) {
                        if !is_boilerplate(&n) {
                            names.push(method_to_resp(&n));
                        }
                    }
                    break;
                }
            }
        }
        // fn / pub fn name (Rust)
        if let Some(rest) = trimmed
            .strip_prefix("pub fn ")
            .or_else(|| trimmed.strip_prefix("fn "))
        {
            if let Some(n) = take_ident(rest) {
                if !is_boilerplate(&n) {
                    names.push(method_to_resp(&n));
                }
            }
        }
        // def name (Python)
        if let Some(rest) = trimmed.strip_prefix("def ") {
            if let Some(n) = take_ident(rest) {
                if !is_boilerplate(&n) && !n.starts_with('_') {
                    names.push(method_to_resp(&n));
                }
            }
        }
        if names.len() >= 15 {
            break;
        }
    }
    names
}

fn take_ident(s: &str) -> Option<String> {
    let n: String = s
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if n.is_empty() {
        None
    } else {
        Some(n)
    }
}

fn is_boilerplate(name: &str) -> bool {
    matches!(
        name,
        "constructor" | "new" | "toString" | "toJSON" | "clone" | "default" | "from" | "into"
    )
}

fn method_to_resp(name: &str) -> String {
    // Convert camelCase/snake_case to words
    let mut s = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if c == '_' {
            s.push(' ');
            prev_lower = false;
        } else if c.is_uppercase() && prev_lower {
            s.push(' ');
            s.extend(c.to_lowercase());
            prev_lower = false;
        } else {
            s.extend(c.to_lowercase());
            prev_lower = c.is_lowercase();
        }
    }
    s
}

fn detect_design_patterns(content: &str, file_path: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    let lower = content.to_lowercase();

    if lower.contains("getinstance(") || lower.contains("private static instance") {
        patterns.push("Singleton".into());
    }
    if (lower.contains("factory") || lower.contains("create")) && lower.contains("new ") {
        patterns.push("Factory".into());
    }
    if lower.contains("@injectable") || lower.contains("@inject") {
        patterns.push("Dependency Injection".into());
    }
    if lower.contains("subscribe") || lower.contains("eventemitter") || lower.contains(".on(") {
        patterns.push("Observer/Event Emitter".into());
    }
    if lower.contains(".pipe(") || lower.contains(".transform(") {
        patterns.push("Pipeline/Transform".into());
    }
    if lower.contains("strategy") && lower.contains("implements") {
        patterns.push("Strategy".into());
    }
    if (lower.contains(".use(") || lower.contains("middleware"))
        && (lower.contains("req") || lower.contains("request"))
    {
        patterns.push("Middleware".into());
    }
    if lower.contains("canactivate") || lower.contains("@guard") {
        patterns.push("Guard".into());
    }
    let fp_lower = file_path.to_lowercase();
    if (lower.contains("repository") || lower.contains(".find") || lower.contains(".save("))
        && (fp_lower.contains("entity")
            || fp_lower.contains("model")
            || fp_lower.contains("schema"))
    {
        patterns.push("Repository".into());
    }

    patterns
}

fn extract_business_rules(content: &str) -> Vec<String> {
    let mut rules = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let lower = line.to_lowercase();
        let is_comment = line.trim_start().starts_with("//")
            || line.trim_start().starts_with('#')
            || line.trim_start().starts_with("/*")
            || line.trim_start().starts_with('*');
        if is_comment {
            for kw in [
                "rule",
                "policy",
                "requirement",
                " must ",
                " always ",
                " never ",
                "enforce",
                "constraint",
            ] {
                if lower.contains(kw) {
                    let cleaned = line
                        .trim_start()
                        .trim_start_matches(|c: char| {
                            c == '/' || c == '*' || c == '#' || c.is_whitespace()
                        })
                        .trim()
                        .to_string();
                    if cleaned.len() > 5 {
                        rules.push(format!("Line {}: {}", i + 1, cleaned));
                    }
                    break;
                }
            }
        }

        // SCREAMING_SNAKE constants with prefixes
        let trimmed = line.trim();
        for pfx in [
            "const ",
            "let ",
            "val ",
            "static ",
            "public static final ",
            "pub const ",
        ] {
            if let Some(rest) = trimmed.strip_prefix(pfx) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_uppercase() || *c == '_')
                    .collect();
                if name.len() > 3
                    && (name.starts_with("MAX_")
                        || name.starts_with("MIN_")
                        || name.starts_with("DEFAULT_")
                        || name.starts_with("LIMIT_")
                        || name.starts_with("THRESHOLD_"))
                {
                    rules.push(format!("{} — constant", name));
                }
                break;
            }
        }

        if rules.len() >= 20 {
            break;
        }
    }
    rules
}

fn extract_gotchas(content: &str) -> Vec<String> {
    let mut gotchas = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let lower = line.to_lowercase();
        if lower.contains("// workaround")
            || lower.contains("// hack")
            || lower.contains("// fixme")
            || lower.contains("// warning")
            || lower.contains("// caution")
        {
            let cleaned = line.trim_start().trim_start_matches('/').trim().to_string();
            gotchas.push(format!("Line {}: {}", i + 1, cleaned));
        }
        if lower.contains("@ts-ignore") || lower.contains("@ts-expect-error") {
            gotchas.push(format!("Line {}: TS suppression", i + 1));
        }
        if lower.contains("eslint-disable") {
            gotchas.push(format!("Line {}: ESLint disabled", i + 1));
        }
        if gotchas.len() >= 10 {
            break;
        }
    }
    gotchas
}

fn extract_security_notes(content: &str) -> Vec<String> {
    let mut notes = Vec::new();
    let lower = content.to_lowercase();
    if (lower.contains("jwt") || lower.contains("bearer") || lower.contains("token"))
        && (lower.contains("verify") || lower.contains("validate") || lower.contains("sign"))
    {
        notes.push("JWT/token handling detected".into());
    }
    if lower.contains("password") || lower.contains("credential") || lower.contains("secret") {
        notes.push("Sensitive data (password/credentials/secrets)".into());
    }
    if (lower.contains("bcrypt") || lower.contains("argon") || lower.contains("scrypt"))
        && lower.contains("password")
    {
        notes.push("Password hashing detected".into());
    }
    if lower.contains("sanitize") || lower.contains("escape") || lower.contains("xss") {
        notes.push("Input sanitization detected".into());
    }
    if lower.contains("cors") && (lower.contains("allow") || lower.contains("enable")) {
        notes.push("CORS configuration detected".into());
    }
    if lower.contains("@useguards") || lower.contains("canactivate") || lower.contains("authguard")
    {
        notes.push("Auth guard usage detected".into());
    }
    if lower.contains("helmet") || lower.contains("content-security") {
        notes.push("Security headers configuration".into());
    }
    if lower.contains("rate-limit") || lower.contains("ratelimit") || lower.contains("throttle") {
        notes.push("Rate limiting detected".into());
    }
    notes
}

fn build_relationships(file_path: &str, graph: &DependencyGraph) -> Vec<String> {
    let mut rels = Vec::new();
    if let Some(edges) = graph.edges.get(file_path) {
        for imp in edges {
            rels.push(format!("imports {imp}"));
        }
    }
    if let Some(rev) = graph.reverse_edges.get(file_path) {
        for imp in rev {
            rels.push(format!("imported by {imp}"));
        }
    }
    rels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsibilities_from_rust() {
        let src = "pub fn handle_request() {}\nfn private_helper() {}\npub fn send_email() {}\n";
        let r = extract_responsibilities(src);
        assert!(r.iter().any(|x| x.contains("handle request")));
        assert!(r.iter().any(|x| x.contains("send email")));
    }

    #[test]
    fn patterns_detected() {
        let src = "@Injectable()\nclass Foo { subscribe(event) {} }";
        let p = detect_design_patterns(src, "foo.service.ts");
        assert!(p.contains(&"Dependency Injection".to_string()));
        assert!(p.contains(&"Observer/Event Emitter".to_string()));
    }

    #[test]
    fn security_detected() {
        let src = "bcrypt.hash(password)\nconst token = jwt.sign(data)\njwt.verify(token)";
        let notes = extract_security_notes(src);
        assert!(notes.iter().any(|n| n.contains("Password hashing")));
        assert!(notes.iter().any(|n| n.contains("JWT")));
    }

    #[test]
    fn business_rules_from_constants() {
        let src = "pub const MAX_RETRIES: u32 = 3;\npub const DEFAULT_TIMEOUT_MS: u64 = 5000;\n";
        let rules = extract_business_rules(src);
        assert!(rules.iter().any(|r| r.contains("MAX_RETRIES")));
        assert!(rules.iter().any(|r| r.contains("DEFAULT_TIMEOUT_MS")));
    }

    #[test]
    fn extracts_rust_trait_impls() {
        let src = r#"
            use super::Thing;

            pub struct SqliteAdapter;
            impl MemoryStorage for SqliteAdapter {
                fn ping(&self) {}
            }

            impl<'a> Display for Thing<'a> {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
            }

            impl MemoryStorage for PostgresAdapter {}

            // impl InsideComment for Bar {}
        "#;
        let impls = extract_rust_trait_impls(src);
        // MemoryStorage for SqliteAdapter
        assert!(impls
            .iter()
            .any(|i| i.trait_name == "MemoryStorage" && i.type_name == "SqliteAdapter"));
        // Generic on impl side still parsed
        assert!(impls
            .iter()
            .any(|i| i.trait_name == "Display" && i.type_name == "Thing"));
        // Second MemoryStorage impl picked up
        assert!(impls
            .iter()
            .any(|i| i.trait_name == "MemoryStorage" && i.type_name == "PostgresAdapter"));
        // `impl TypeOnly { ... }` (no trait) must not match
        assert!(!impls.iter().any(|i| i.type_name == "TypeOnly"));
    }
}
