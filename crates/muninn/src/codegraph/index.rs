//! Crawl-side orchestration: turn a scanned inventory into a stored graph.
//!
//! This slice always re-parses every supported file. Content hashes are
//! recorded so a later slice can skip unchanged files, but that also needs the
//! raw call sites persisted — resolution is project-wide, so an incremental
//! pass cannot re-resolve using only the files it re-read.

use std::path::Path;

use crate::huginn::graph::parsers::Lang;
use crate::huginn::scanner::FileEntry;

use super::extract::{extract_file, is_supported};
use super::resolve::{resolve, FileFacts};
use super::store::{CodeGraphStore, FileRecord, Result, SymbolRecord};
use super::{qualified_name, FileStatus};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexOutcome {
    pub files_indexed: usize,
    pub files_partial: usize,
    pub files_skipped: usize,
    pub files_errored: usize,
    pub symbols: usize,
    pub edges: usize,
    pub unresolved_calls: usize,
}

/// Stable, deterministic 64-bit FNV-1a over raw bytes.
fn content_hash(bytes: &[u8]) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x100_0000_01b3;
    let mut h = FNV_OFFSET;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:016x}")
}

pub fn index_project(
    store: &CodeGraphStore,
    project: &str,
    root: &Path,
    files: &[FileEntry],
) -> Result<IndexOutcome> {
    store.begin_project(project, root, true)?;

    let mut outcome = IndexOutcome::default();
    let mut facts: Vec<FileFacts> = Vec::new();

    for file in files {
        let rel = file.relative_path.clone();
        let lang = Lang::from_extension(&file.extension);

        let Some(lang) = lang.filter(|l| is_supported(*l)) else {
            store.replace_file(
                project,
                FileRecord {
                    path: &rel,
                    lang: None,
                    content_hash: "",
                    status: FileStatus::SkippedLang,
                    detail: None,
                },
                &[],
            )?;
            outcome.files_skipped += 1;
            continue;
        };

        let bytes = match std::fs::read(&file.path) {
            Ok(b) => b,
            Err(e) => {
                store.replace_file(
                    project,
                    FileRecord {
                        path: &rel,
                        lang: Some(lang_name(lang)),
                        content_hash: "",
                        status: FileStatus::Error,
                        detail: Some(&e.to_string()),
                    },
                    &[],
                )?;
                outcome.files_errored += 1;
                continue;
            }
        };
        let hash = content_hash(&bytes);
        let source = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                store.replace_file(
                    project,
                    FileRecord {
                        path: &rel,
                        lang: Some(lang_name(lang)),
                        content_hash: &hash,
                        status: FileStatus::Error,
                        detail: Some("not valid UTF-8"),
                    },
                    &[],
                )?;
                outcome.files_errored += 1;
                continue;
            }
        };

        let extract = extract_file(&source, lang, &rel);
        let status = if extract.had_errors {
            outcome.files_partial += 1;
            FileStatus::Partial
        } else {
            outcome.files_indexed += 1;
            FileStatus::Indexed
        };
        let detail = extract
            .had_errors
            .then(|| "tree-sitter reported syntax errors; symbols may be incomplete".to_string());

        let records: Vec<SymbolRecord> = extract
            .symbols
            .iter()
            .map(|s| SymbolRecord {
                name: s.name.clone(),
                qualified_name: qualified_name(&rel, s.container.as_deref(), &s.name),
                label: s.label.as_str(),
                file_path: rel.clone(),
                start_line: s.start_line,
                end_line: s.end_line,
                signature: s.signature.clone(),
                exported: s.exported,
                metrics: s.metrics,
            })
            .collect();

        outcome.symbols += records.len();
        store.replace_file(
            project,
            FileRecord {
                path: &rel,
                lang: Some(lang_name(lang)),
                content_hash: &hash,
                status,
                detail: detail.as_deref(),
            },
            &records,
        )?;
        facts.push(FileFacts { path: rel, extract });
    }

    let resolved = resolve(&facts, root);
    store.rebuild_edges(project, &resolved.edges)?;
    for (path, count) in &resolved.unresolved {
        store.set_unresolved(project, path, *count)?;
        outcome.unresolved_calls += count;
    }

    // Report what was stored, not what was resolved: two call sites on the
    // same line to the same definition collapse into one edge, and a count
    // that disagrees with the graph is a count nobody can check.
    outcome.edges = store.coverage(project)?.edges;

    let summary = render_summary(store, project, &outcome)?;
    store.set_summary(project, &summary)?;

    Ok(outcome)
}

fn lang_name(lang: Lang) -> &'static str {
    match lang {
        Lang::TypeScript => "typescript",
        Lang::Tsx => "tsx",
        Lang::JavaScript => "javascript",
        Lang::Python => "python",
        Lang::Rust => "rust",
        Lang::Go => "go",
    }
}

/// The block the SessionStart hook injects verbatim. Rendered once at crawl
/// time so the hook path is a single indexed row read.
fn render_summary(store: &CodeGraphStore, project: &str, outcome: &IndexOutcome) -> Result<String> {
    let cov = store.coverage(project)?;
    let hubs = store.top_hubs(project, 8)?;

    let mut out = String::new();
    out.push_str(&format!(
        "{} symbols and {} call edges across {} files.",
        outcome.symbols, outcome.edges, cov.files_total
    ));
    let parsed = cov.indexed + cov.partial;
    if let Some(pct) = (parsed * 100).checked_div(cov.files_total) {
        out.push_str(&format!(" Parsed {parsed}/{} ({pct}%).", cov.files_total));
    }
    if !cov.skipped_by_ext.is_empty() {
        let listed: Vec<String> = cov
            .skipped_by_ext
            .iter()
            .take(3)
            .map(|(ext, n)| format!("{ext} ({n})"))
            .collect();
        out.push_str(&format!(" Not parsed: {}.", listed.join(", ")));
    }
    if !hubs.is_empty() {
        let names: Vec<String> = hubs
            .iter()
            .take(8)
            .map(|(qn, _, fan)| format!("{qn} ({fan})"))
            .collect();
        out.push_str(&format!("\nMost-called: {}.", names.join(", ")));
    }
    Ok(crate::text::truncate_ellipsis(&out, 600))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn entry(root: &Path, rel: &str, content: &str) -> FileEntry {
        let path = root.join(rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        FileEntry {
            path,
            relative_path: rel.to_string(),
            size: content.len() as u64,
            line_count: content.lines().count(),
            last_modified: Some(SystemTime::now()),
            extension: rel.rsplit('.').next().unwrap_or("").to_string(),
        }
    }

    #[test]
    fn unsupported_languages_are_counted_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = vec![
            entry(root, "src/a.rs", "pub fn alpha() {}\n"),
            entry(root, "README.md", "# hi\n"),
            entry(root, "Main.java", "class Main {}\n"),
        ];

        let store = CodeGraphStore::in_memory().unwrap();
        let out = index_project(&store, "p", root, &files).unwrap();

        assert_eq!(out.files_skipped, 2, "md and java are not parseable here");
        let cov = store.coverage("p").unwrap();
        assert_eq!(cov.files_total, 3, "every scanned file gets a coverage row");
        assert_eq!(cov.skipped_lang, 2);
        assert!(
            cov.skipped_by_ext.iter().any(|(e, _)| e == ".java"),
            "the gap should name the extension, got {:?}",
            cov.skipped_by_ext
        );
    }

    #[test]
    fn a_summary_is_stored_for_the_hook_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = vec![entry(root, "src/a.rs", "pub fn alpha() {}\n")];
        let store = CodeGraphStore::in_memory().unwrap();
        index_project(&store, "p", root, &files).unwrap();

        let summary = store.summary("p").unwrap().expect("summary is stored");
        assert!(summary.contains("symbols"), "got {summary}");
        assert!(summary.len() <= 600);
    }

    #[test]
    fn unreadable_files_are_recorded_as_errors_not_skipped_silently() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut f = entry(root, "src/bad.rs", "pub fn x() {}\n");
        f.path = root.join("src/does-not-exist.rs");
        let store = CodeGraphStore::in_memory().unwrap();
        let out = index_project(&store, "p", root, &[f]).unwrap();
        assert_eq!(out.files_errored, 1);
        assert_eq!(store.coverage("p").unwrap().errored, 1);
    }
}
