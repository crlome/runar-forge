//! Framework-aware entry-point discovery.
//!
//! Replaces the plain filename match in `crawl.rs` with a set of detectors
//! that also inspect file path context and (when ambiguous) a small slice
//! of content. Each detector emits a confidence score so mono-repos with
//! multiple frameworks (e.g. Next.js web + Axum API) rank correctly.

use std::collections::HashSet;
use std::fs;

use super::scanner::FileEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Framework {
    NextJs,
    NestJs,
    Express,
    Django,
    Flask,
    Axum,
    Actix,
    Gin,
    Echo,
    RustBinary,
    RustLibrary,
    PlainScript,
}

impl Framework {
    pub fn as_str(&self) -> &'static str {
        match self {
            Framework::NextJs => "next.js",
            Framework::NestJs => "nestjs",
            Framework::Express => "express",
            Framework::Django => "django",
            Framework::Flask => "flask",
            Framework::Axum => "axum",
            Framework::Actix => "actix-web",
            Framework::Gin => "gin",
            Framework::Echo => "echo",
            Framework::RustBinary => "rust-binary",
            Framework::RustLibrary => "rust-library",
            Framework::PlainScript => "script",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EntryPoint {
    pub relative_path: String,
    pub framework: Framework,
    pub confidence: f64,
}

/// Produce a ranked list of entry points for a project. Higher confidence
/// first; deduplicated by relative_path (keeps the highest-confidence hit).
pub fn discover(files: &[FileEntry]) -> Vec<EntryPoint> {
    let mut out: Vec<EntryPoint> = Vec::new();
    for f in files {
        for ep in detect_for_file(f) {
            out.push(ep);
        }
    }

    // Sort by confidence desc, then dedup by path keeping the first (highest).
    out.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.relative_path.cmp(&b.relative_path))
    });
    let mut seen: HashSet<String> = HashSet::new();
    out.retain(|ep| seen.insert(ep.relative_path.clone()));
    out
}

/// Convenience predicate kept for callers that still just want yes/no.
pub fn is_entry_point(f: &FileEntry) -> bool {
    !detect_for_file(f).is_empty()
}

fn detect_for_file(f: &FileEntry) -> Vec<EntryPoint> {
    let rel = f.relative_path.replace('\\', "/").to_lowercase();
    let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
    let mut hits: Vec<EntryPoint> = Vec::new();

    // ── Next.js ──────────────────────────────────────────────────────
    // Pages router: `pages/_app.*`, `pages/index.*`
    // App router:   `app/layout.*`, `app/page.*`
    if matches!(
        name.as_str(),
        "_app.ts"
            | "_app.tsx"
            | "_app.js"
            | "_app.jsx"
            | "index.ts"
            | "index.tsx"
            | "index.js"
            | "index.jsx"
    ) && (rel.contains("/pages/") || rel.starts_with("pages/"))
    {
        hits.push(ep(&f.relative_path, Framework::NextJs, 0.95));
    }
    if matches!(
        name.as_str(),
        "layout.ts"
            | "layout.tsx"
            | "layout.js"
            | "layout.jsx"
            | "page.ts"
            | "page.tsx"
            | "page.js"
            | "page.jsx"
    ) && (rel.contains("/app/") || rel.starts_with("app/"))
    {
        hits.push(ep(&f.relative_path, Framework::NextJs, 0.90));
    }

    // ── Django ───────────────────────────────────────────────────────
    if matches!(name.as_str(), "manage.py" | "wsgi.py" | "asgi.py") {
        hits.push(ep(&f.relative_path, Framework::Django, 0.95));
    }

    // ── NestJS (main.ts with NestFactory) ────────────────────────────
    // ── Express  (server.ts / app.ts with express()) ─────────────────
    // ── Flask   (app.py with Flask(__name__)) ────────────────────────
    // ── Axum/Actix (main.rs with axum::Router / actix_web::HttpServer)
    // ── Gin/Echo  (main.go with gin.Default() / echo.New()) ──────────
    if matches!(name.as_str(), "main.ts" | "main.js") && sniff(f, &["nestfactory", "@nestjs/core"])
    {
        hits.push(ep(&f.relative_path, Framework::NestJs, 0.95));
    }
    if matches!(
        name.as_str(),
        "server.ts" | "server.js" | "app.ts" | "app.js"
    ) && sniff(f, &["express()", "require('express')", "from \"express\""])
    {
        hits.push(ep(&f.relative_path, Framework::Express, 0.90));
    }
    if name == "app.py" && sniff(f, &["flask(__name__)", "from flask import"]) {
        hits.push(ep(&f.relative_path, Framework::Flask, 0.90));
    }
    if name == "main.rs" {
        if sniff(f, &["axum::router", "axum::serve", "axum::routing"]) {
            hits.push(ep(&f.relative_path, Framework::Axum, 0.93));
        }
        if sniff(f, &["actix_web::httpserver", "actix_web::web"]) {
            hits.push(ep(&f.relative_path, Framework::Actix, 0.93));
        }
    }
    if name == "main.go" {
        if sniff(f, &["gin.default()", "gin.new("]) {
            hits.push(ep(&f.relative_path, Framework::Gin, 0.93));
        }
        if sniff(f, &["echo.new("]) {
            hits.push(ep(&f.relative_path, Framework::Echo, 0.93));
        }
    }

    // ── Generic fallbacks (filename only, low confidence) ────────────
    // Only emit if no framework hit fired for this file.
    if hits.is_empty() {
        let fallback = match name.as_str() {
            "main.rs" => Some((Framework::RustBinary, 0.70)),
            "lib.rs" => Some((Framework::RustLibrary, 0.70)),
            "main.ts" | "app.ts" | "server.ts" | "cli.ts" | "index.ts" => {
                Some((Framework::PlainScript, 0.55))
            }
            "main.py" => Some((Framework::PlainScript, 0.55)),
            "main.go" => Some((Framework::PlainScript, 0.55)),
            _ => None,
        };
        if let Some((fw, conf)) = fallback {
            hits.push(ep(&f.relative_path, fw, conf));
        }
    }

    hits
}

fn ep(path: &str, framework: Framework, confidence: f64) -> EntryPoint {
    EntryPoint {
        relative_path: path.to_string(),
        framework,
        confidence,
    }
}

/// Read up to the first 8 KiB of a file and check whether any needle is
/// present (case-insensitive). Returns false on IO error — better to miss a
/// framework hit than crash the crawl.
fn sniff(f: &FileEntry, needles: &[&str]) -> bool {
    let Ok(mut content) = fs::read_to_string(&f.path) else {
        return false;
    };
    if content.len() > 8192 {
        content.truncate(8192);
    }
    let lower = content.to_lowercase();
    needles.iter().any(|n| lower.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn mk_file(rel: &str, disk_path: PathBuf) -> FileEntry {
        FileEntry {
            path: disk_path,
            relative_path: rel.into(),
            size: 0,
            line_count: 0,
            last_modified: Some(SystemTime::now()),
            extension: rel.rsplit('.').next().unwrap_or("").to_string(),
        }
    }

    #[test]
    fn detects_django_manage_py() {
        let f = mk_file("manage.py", PathBuf::from("/nowhere/manage.py"));
        let hits = detect_for_file(&f);
        assert_eq!(hits[0].framework, Framework::Django);
        assert!(hits[0].confidence >= 0.9);
    }

    #[test]
    fn detects_nextjs_pages_router() {
        let f = mk_file("src/pages/_app.tsx", PathBuf::from("/nowhere/_app.tsx"));
        let hits = detect_for_file(&f);
        assert_eq!(hits[0].framework, Framework::NextJs);
    }

    #[test]
    fn detects_nextjs_app_router() {
        let f = mk_file("app/layout.tsx", PathBuf::from("/nowhere/layout.tsx"));
        let hits = detect_for_file(&f);
        assert_eq!(hits[0].framework, Framework::NextJs);
    }

    #[test]
    fn rust_main_without_framework_falls_back_to_binary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("main.rs");
        std::fs::write(&p, "fn main() { println!(\"hi\"); }").unwrap();
        let f = mk_file("src/main.rs", p);
        let hits = detect_for_file(&f);
        assert_eq!(hits[0].framework, Framework::RustBinary);
        assert!(hits[0].confidence < 0.8); // low-confidence fallback
    }

    #[test]
    fn rust_main_with_axum_detects_framework() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("main.rs");
        std::fs::write(
            &p,
            "use axum::Router;\nfn main() { let app = Router::new(); }",
        )
        .unwrap();
        let f = mk_file("src/main.rs", p);
        let hits = detect_for_file(&f);
        assert_eq!(hits[0].framework, Framework::Axum);
        assert!(hits[0].confidence > 0.9);
    }

    #[test]
    fn discover_ranks_higher_confidence_first() {
        let dir = tempfile::tempdir().unwrap();
        let rs = dir.path().join("m.rs");
        std::fs::write(&rs, "use axum::Router;\nfn main(){}").unwrap();
        let py = dir.path().join("manage.py");
        std::fs::write(&py, "").unwrap();

        let files = vec![
            mk_file("src/main.rs", rs),
            mk_file("manage.py", py),
            mk_file("src/lib.rs", PathBuf::from("/nowhere/lib.rs")),
        ];
        let ranked = discover(&files);
        // manage.py (0.95) should outrank axum main.rs (0.93) and lib.rs (0.70).
        assert_eq!(ranked[0].relative_path, "manage.py");
        assert_eq!(ranked.last().unwrap().relative_path, "src/lib.rs");
    }

    #[test]
    fn is_entry_point_matches_main_rs() {
        let f = mk_file("src/main.rs", PathBuf::from("/nowhere/main.rs"));
        assert!(is_entry_point(&f));
    }
}
