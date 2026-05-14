use std::collections::HashMap;

use crate::huginn::analyzer::AnalysisDepth;

use super::{AnalysisBody, CrossFilePattern, FileAnalysisResult, TechDebtMarker};

#[derive(Debug, Clone, Default)]
pub struct ConfirmedStack {
    pub runtime: String,
    pub frameworks: Vec<String>,
    pub database: Vec<String>,
    pub testing: Vec<String>,
    pub build_tools: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ArchitectureSummary {
    pub project: String,
    pub total_files: usize,
    pub deep: usize,
    pub medium: usize,
    pub light: usize,
    pub skipped: usize,
    pub confirmed_stack: ConfirmedStack,
    pub top_languages: Vec<(String, usize)>,
    pub entry_points: Vec<String>,
    pub architecture_pattern: String,
    pub techdebt_counts: HashMap<String, usize>,
    pub formatted: String,
}

pub fn summarize(
    project: &str,
    analyses: &[FileAnalysisResult],
    patterns: &[CrossFilePattern],
    techdebt: &[TechDebtMarker],
) -> ArchitectureSummary {
    let mut deep = 0;
    let mut medium = 0;
    let mut light = 0;
    let mut skipped = 0;
    let mut lang_counts: HashMap<String, usize> = HashMap::new();
    let mut all_imports: Vec<String> = Vec::new();
    let mut entry_points = Vec::new();

    for a in analyses {
        match a.depth {
            AnalysisDepth::Deep => deep += 1,
            AnalysisDepth::Medium => medium += 1,
            AnalysisDepth::Light => light += 1,
            AnalysisDepth::Skip => skipped += 1,
        }
        if !a.extension.is_empty() {
            *lang_counts.entry(a.extension.clone()).or_default() += 1;
        }
        match &a.body {
            AnalysisBody::Deep(d) => all_imports.extend(d.key_imports.iter().cloned()),
            AnalysisBody::Medium(m) => all_imports.extend(m.import_sources.iter().cloned()),
            _ => {}
        }
        if is_entry_point_path(&a.file_path) {
            entry_points.push(a.file_path.clone());
        }
    }

    let mut langs: Vec<(String, usize)> = lang_counts.into_iter().collect();
    langs.sort_by(|a, b| b.1.cmp(&a.1));
    let top_languages = langs.into_iter().take(5).collect::<Vec<_>>();

    let confirmed_stack = detect_stack(&all_imports, &top_languages);
    let architecture_pattern = infer_architecture_pattern(patterns, &entry_points);

    let mut techdebt_counts: HashMap<String, usize> = HashMap::new();
    for m in techdebt {
        *techdebt_counts
            .entry(m.marker_type.as_str().to_string())
            .or_default() += 1;
    }

    let total_files = analyses.len();
    let formatted = format_summary(
        project,
        total_files,
        deep,
        medium,
        light,
        skipped,
        &top_languages,
        &confirmed_stack,
        &entry_points,
        &architecture_pattern,
        patterns,
        &techdebt_counts,
    );

    ArchitectureSummary {
        project: project.into(),
        total_files,
        deep,
        medium,
        light,
        skipped,
        confirmed_stack,
        top_languages,
        entry_points,
        architecture_pattern,
        techdebt_counts,
        formatted,
    }
}

fn is_entry_point_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    matches!(
        name.as_str(),
        "main.rs"
            | "main.ts"
            | "main.py"
            | "main.go"
            | "lib.rs"
            | "app.ts"
            | "server.ts"
            | "cli.ts"
            | "index.ts"
            | "manage.py"
            | "wsgi.py"
            | "asgi.py"
    )
}

fn detect_stack(imports: &[String], top_langs: &[(String, usize)]) -> ConfirmedStack {
    let mut frameworks = Vec::new();
    let mut database = Vec::new();
    let mut testing = Vec::new();
    let mut build_tools = Vec::new();

    for imp in imports {
        let l = imp.to_lowercase();
        if l.contains("express") {
            push_unique(&mut frameworks, "Express");
        }
        if l.contains("fastify") {
            push_unique(&mut frameworks, "Fastify");
        }
        if l.contains("@nestjs") {
            push_unique(&mut frameworks, "NestJS");
        }
        if l.contains("next") {
            push_unique(&mut frameworks, "Next.js");
        }
        if l.contains("react") {
            push_unique(&mut frameworks, "React");
        }
        if l.contains("axum") {
            push_unique(&mut frameworks, "Axum");
        }
        if l.contains("actix") {
            push_unique(&mut frameworks, "Actix");
        }
        if l.contains("flask") {
            push_unique(&mut frameworks, "Flask");
        }
        if l.contains("django") {
            push_unique(&mut frameworks, "Django");
        }
        if l.contains("fastapi") {
            push_unique(&mut frameworks, "FastAPI");
        }

        if l.contains("prisma") {
            push_unique(&mut database, "Prisma");
        }
        if l.contains("typeorm") {
            push_unique(&mut database, "TypeORM");
        }
        if l.contains("drizzle") {
            push_unique(&mut database, "Drizzle");
        }
        if l.contains("mongoose") {
            push_unique(&mut database, "Mongoose");
        }
        if l.contains("rusqlite") {
            push_unique(&mut database, "rusqlite (SQLite)");
        }
        if l.contains("tokio-postgres") {
            push_unique(&mut database, "tokio-postgres");
        }
        if l.contains("sqlx") {
            push_unique(&mut database, "sqlx");
        }
        if l.contains("pgvector") {
            push_unique(&mut database, "pgvector");
        }

        if l.contains("vitest") {
            push_unique(&mut testing, "Vitest");
        }
        if l.contains("jest") {
            push_unique(&mut testing, "Jest");
        }
        if l.contains("pytest") {
            push_unique(&mut testing, "pytest");
        }

        if l.contains("tsup") {
            push_unique(&mut build_tools, "tsup");
        }
        if l.contains("vite") {
            push_unique(&mut build_tools, "Vite");
        }
        if l.contains("webpack") {
            push_unique(&mut build_tools, "Webpack");
        }
        if l.contains("esbuild") {
            push_unique(&mut build_tools, "esbuild");
        }
    }

    let runtime = match top_langs.first().map(|(e, _)| e.as_str()) {
        Some("rs") => "Rust".into(),
        Some("ts") | Some("tsx") | Some("js") | Some("jsx") => "Node.js / TypeScript".into(),
        Some("py") => "Python".into(),
        Some("go") => "Go".into(),
        _ => "Unknown".into(),
    };

    ConfirmedStack {
        runtime,
        frameworks,
        database,
        testing,
        build_tools,
    }
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

fn infer_architecture_pattern(patterns: &[CrossFilePattern], entry_points: &[String]) -> String {
    let has = |n: &str| patterns.iter().any(|p| p.name == n);
    if has("dependency-injection") && has("middleware-chain") {
        "Modular MVC / framework-based (DI + middleware)".into()
    } else if has("data-access-pattern") && has("authentication-flow") {
        "Layered web service (auth + data access)".into()
    } else if entry_points
        .iter()
        .any(|e| e.ends_with("main.rs") || e.ends_with("lib.rs"))
    {
        "Rust CLI / library".into()
    } else {
        "Generic modular".into()
    }
}

#[allow(clippy::too_many_arguments)]
fn format_summary(
    project: &str,
    total: usize,
    deep: usize,
    medium: usize,
    light: usize,
    skipped: usize,
    langs: &[(String, usize)],
    stack: &ConfirmedStack,
    entry_points: &[String],
    arch: &str,
    patterns: &[CrossFilePattern],
    techdebt: &HashMap<String, usize>,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Architecture Summary: {project}\n"));
    s.push_str(&format!(
        "*Analyzed: {}*\n\n",
        chrono::Utc::now().to_rfc3339()
    ));
    s.push_str("## File Statistics\n");
    s.push_str(&format!("- Total files: {total}\n"));
    s.push_str(&format!(
        "- Deep: {deep}, Medium: {medium}, Light: {light}, Skipped: {skipped}\n\n"
    ));

    s.push_str("## Languages\n");
    for (ext, count) in langs {
        s.push_str(&format!("- {ext}: {count}\n"));
    }
    s.push('\n');

    s.push_str("## Confirmed Stack\n");
    s.push_str(&format!("- Runtime: {}\n", stack.runtime));
    if !stack.frameworks.is_empty() {
        s.push_str(&format!("- Frameworks: {}\n", stack.frameworks.join(", ")));
    }
    if !stack.database.is_empty() {
        s.push_str(&format!("- Database: {}\n", stack.database.join(", ")));
    }
    if !stack.testing.is_empty() {
        s.push_str(&format!("- Testing: {}\n", stack.testing.join(", ")));
    }
    if !stack.build_tools.is_empty() {
        s.push_str(&format!("- Build: {}\n", stack.build_tools.join(", ")));
    }
    s.push('\n');

    s.push_str("## Entry Points\n");
    if entry_points.is_empty() {
        s.push_str("- (none detected)\n");
    } else {
        for p in entry_points {
            s.push_str(&format!("- {p}\n"));
        }
    }
    s.push('\n');

    s.push_str(&format!("## Architecture Pattern\n{arch}\n\n"));

    s.push_str("## Cross-file Patterns\n");
    if patterns.is_empty() {
        s.push_str("- (none with sufficient confidence)\n");
    } else {
        for p in patterns {
            s.push_str(&format!(
                "- **{}** ({:.0}% confidence): {}\n",
                p.name,
                p.confidence * 100.0,
                p.description
            ));
        }
    }
    s.push('\n');

    s.push_str("## Tech Debt Markers\n");
    if techdebt.is_empty() {
        s.push_str("- (none found)\n");
    } else {
        for (t, n) in techdebt {
            s.push_str(&format!("- {t}: {n}\n"));
        }
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huginn::analysis::{AnalysisBody, DeepAnalysis, FileAnalysisResult};
    use crate::huginn::analyzer::AnalysisDepth;

    fn mk(path: &str, ext: &str, imports: Vec<String>) -> FileAnalysisResult {
        FileAnalysisResult {
            file_path: path.into(),
            extension: ext.into(),
            line_count: 100,
            depth: AnalysisDepth::Deep,
            body: AnalysisBody::Deep(DeepAnalysis {
                purpose: "mod".into(),
                key_imports: imports,
                ..Default::default()
            }),
            rust_trait_impls: Vec::new(),
            sql_analysis: None,
        }
    }

    #[test]
    fn detects_rust_stack() {
        let analyses = vec![
            mk("src/main.rs", "rs", vec!["tokio".into(), "rusqlite".into()]),
            mk(
                "src/storage.rs",
                "rs",
                vec!["tokio-postgres".into(), "pgvector".into()],
            ),
        ];
        let summary = summarize("test", &analyses, &[], &[]);
        assert_eq!(summary.confirmed_stack.runtime, "Rust");
        assert!(summary
            .confirmed_stack
            .database
            .iter()
            .any(|d| d.contains("rusqlite")));
        assert!(summary
            .confirmed_stack
            .database
            .iter()
            .any(|d| d.contains("tokio-postgres")));
    }

    #[test]
    fn entry_points_detected() {
        let analyses = vec![mk("src/main.rs", "rs", vec![])];
        let summary = summarize("test", &analyses, &[], &[]);
        assert_eq!(summary.entry_points, vec!["src/main.rs".to_string()]);
    }
}
