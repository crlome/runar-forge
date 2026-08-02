//! Crawl-side orchestration: turn a scanned inventory into a stored graph.
//!
//! An incremental pass re-parses only files whose content hash changed, then
//! re-resolves the WHOLE project — resolution is project-wide, so a call in an
//! untouched file can start or stop resolving because of an edit elsewhere.
//! That is affordable because each file's unresolved facts are stored, so a
//! re-resolve costs a few table scans rather than a parse per file.

use std::path::Path;

use crate::huginn::graph::parsers::Lang;
use crate::huginn::scanner::FileEntry;

use super::extract::{extract_file, is_supported};
use super::freshness::{self, Snapshot};
use super::resolve::{resolve, FileFacts};
use super::store::{CodeGraphStore, FileRecord, RawFacts, Result, SymbolRecord};
use super::{qualified_name, FileStatus};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexOutcome {
    pub files_indexed: usize,
    pub files_partial: usize,
    pub files_skipped: usize,
    pub files_errored: usize,
    /// Files whose content hash was unchanged, so they were not re-parsed.
    pub files_reused: usize,
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
    full: bool,
    observed: Option<Snapshot>,
) -> Result<IndexOutcome> {
    // What the tree looked like when this pass began. Taken before any work so
    // that an edit landing mid-index is outside the baseline and reads as
    // stale next time: the graph may have missed it, and claiming otherwise is
    // the one error this must never make. Callers that scanned the tree
    // themselves pass the snapshot they took before scanning, which closes the
    // same window over the scan.
    let observed = observed.unwrap_or_else(|| freshness::snapshot(root));

    // A full pass clears the project first; an incremental one keeps every row
    // it is not about to replace.
    store.begin_project(project, root, full)?;

    let known = if full {
        std::collections::HashMap::new()
    } else {
        store.file_hashes(project)?
    };
    let mut outcome = IndexOutcome::default();
    let mut facts: Vec<FileFacts> = Vec::new();
    let mut touched: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();

    for file in files {
        let rel = file.relative_path.clone();
        present.insert(rel.clone());
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
                RawFacts::default(),
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
                    RawFacts::default(),
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
                    RawFacts::default(),
                )?;
                outcome.files_errored += 1;
                continue;
            }
        };

        // Unchanged since the last pass: its stored facts are still valid and
        // are loaded in bulk below instead of being re-derived.
        if known.get(&rel).is_some_and(|h| *h == hash) {
            outcome.files_reused += 1;
            continue;
        }
        touched.insert(rel.clone());

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
            RawFacts {
                calls: &extract.calls,
                imports: &extract.imports,
                relations: &extract.relations,
            },
        )?;
        facts.push(FileFacts { path: rel, extract });
    }

    // Files gone from the inventory must not keep contributing edges. Only on
    // an incremental pass — a full one already cleared the project.
    if !full {
        for stale in known.keys().filter(|p| !present.contains(p.as_str())) {
            store.forget_file(project, stale)?;
        }
    }

    // Everything not re-parsed, read back rather than re-derived. This is the
    // whole point of persisting the raw facts.
    for (path, extract) in store.load_extracts(project, &touched)? {
        outcome.symbols += extract.symbols.len();
        facts.push(FileFacts { path, extract });
    }

    let resolved = resolve(&facts, root);
    store.rebuild_edges(project, &resolved.edges)?;
    for (path, count) in &resolved.unresolved {
        store.set_unresolved(project, path, *count)?;
        outcome.unresolved_calls += count;
    }

    // Report what was stored, not what was resolved: two call sites on the
    // same line to the same definition collapse into one edge, and a count
    // that disagrees with the graph is a count nobody can check. The same
    // applies to symbols, which now come partly from this pass and partly from
    // rows an earlier one wrote.
    let cov = store.coverage(project)?;
    outcome.edges = cov.edges;
    outcome.symbols = cov.symbols;

    let summary = render_summary(store, project, &outcome)?;
    store.set_summary(project, &summary)?;

    // Last, and only on the success path: a baseline recorded for a pass that
    // then failed would claim a currency the stored graph does not have.
    freshness::stamp(store, project, &observed)?;

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
        let out = index_project(&store, "p", root, &files, true, None).unwrap();

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

    /// The property that makes incremental indexing safe to default: the graph
    /// it produces must be indistinguishable from a full rebuild.
    #[test]
    fn an_incremental_pass_agrees_with_a_full_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = || {
            vec![
                entry(
                    root,
                    "src/a.rs",
                    "pub mod b;\nuse crate::b::helper;\npub fn alpha() { helper(); }\n",
                ),
                entry(root, "src/b.rs", "pub fn helper() {}\n"),
                entry(root, "src/c.rs", "pub fn gamma() {}\n"),
            ]
        };

        let full_store = CodeGraphStore::in_memory().unwrap();
        index_project(&full_store, "p", root, &files(), true, None).unwrap();
        let full_cov = full_store.coverage("p").unwrap();

        let inc_store = CodeGraphStore::in_memory().unwrap();
        index_project(&inc_store, "p", root, &files(), true, None).unwrap();
        // Edit one file; the other two must be reused, not re-parsed.
        let changed = vec![
            entry(
                root,
                "src/a.rs",
                "pub mod b;\nuse crate::b::helper;\npub fn alpha() { helper(); helper(); }\n",
            ),
            entry(root, "src/b.rs", "pub fn helper() {}\n"),
            entry(root, "src/c.rs", "pub fn gamma() {}\n"),
        ];
        let out = index_project(&inc_store, "p", root, &changed, false, None).unwrap();
        assert_eq!(out.files_reused, 2, "unchanged files must not be re-parsed");

        let inc_cov = inc_store.coverage("p").unwrap();
        assert_eq!(inc_cov.symbols, full_cov.symbols);
        assert_eq!(inc_cov.files_total, full_cov.files_total);
        // The cross-file edge lives in a file that was reused, so it only
        // survives because the stored facts were re-resolved.
        assert!(inc_cov.edges > 0, "the alpha -> helper edge was lost");
        assert_eq!(inc_cov.edges, full_cov.edges);
    }

    #[test]
    fn an_incremental_pass_forgets_a_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let a = entry(root, "src/a.rs", "pub fn alpha() {}\n");
        let b = entry(root, "src/b.rs", "pub fn beta() {}\n");

        let store = CodeGraphStore::in_memory().unwrap();
        index_project(&store, "p", root, &[a.clone(), b], true, None).unwrap();
        assert_eq!(store.coverage("p").unwrap().symbols, 2);

        index_project(&store, "p", root, &[a], false, None).unwrap();
        let cov = store.coverage("p").unwrap();
        assert_eq!(cov.symbols, 1, "the deleted file kept contributing symbols");
        assert_eq!(cov.files_total, 1);
    }

    #[test]
    fn a_summary_is_stored_for_the_hook_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = vec![entry(root, "src/a.rs", "pub fn alpha() {}\n")];
        let store = CodeGraphStore::in_memory().unwrap();
        index_project(&store, "p", root, &files, true, None).unwrap();

        let summary = store.summary("p").unwrap().expect("summary is stored");
        assert!(summary.contains("symbols"), "got {summary}");
        assert!(summary.len() <= 600);
    }

    /// Through the real producer: whatever else an index does, it must leave
    /// behind a record of the tree it read, or nothing downstream can tell a
    /// current graph from a month-old one.
    #[test]
    fn an_index_records_the_tree_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = vec![entry(root, "src/a.rs", "pub fn alpha() {}\n")];
        let store = CodeGraphStore::in_memory().unwrap();
        index_project(&store, "p", root, &files, true, None).unwrap();

        let recorded = freshness::baseline(&store, "p").expect("every index leaves a baseline");
        // A bare tempdir is not a repository, so the honest record is that it
        // could not be judged — never a hash that would later compare equal.
        assert_eq!(recorded.head, "no-git");
        assert!(!matches!(
            freshness::judge(Some(&recorded), &freshness::Snapshot::default()),
            freshness::Verdict::Fresh
        ));
    }

    /// The parameter exists so a caller that scanned the tree first can record
    /// what it saw *before* scanning, rather than what the tree became while
    /// the scan ran.
    #[test]
    fn a_caller_supplied_snapshot_is_the_one_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = vec![entry(root, "src/a.rs", "pub fn alpha() {}\n")];
        let store = CodeGraphStore::in_memory().unwrap();
        let observed = freshness::Snapshot {
            head: Some("cafebabe".into()),
            dirty: Some("0".into()),
        };
        index_project(&store, "p", root, &files, true, Some(observed.clone())).unwrap();

        let recorded = freshness::baseline(&store, "p").unwrap();
        assert_eq!(recorded.head, "cafebabe");
        assert_eq!(
            freshness::judge(Some(&recorded), &observed),
            freshness::Verdict::Fresh
        );
    }

    #[test]
    fn unreadable_files_are_recorded_as_errors_not_skipped_silently() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut f = entry(root, "src/bad.rs", "pub fn x() {}\n");
        f.path = root.join("src/does-not-exist.rs");
        let store = CodeGraphStore::in_memory().unwrap();
        let out = index_project(&store, "p", root, &[f], true, None).unwrap();
        assert_eq!(out.files_errored, 1);
        assert_eq!(store.coverage("p").unwrap().errored, 1);
    }
}
