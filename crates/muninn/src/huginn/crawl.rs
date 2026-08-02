use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::librarian::MemoryLibrarian;
use crate::storage::StorageResult;
use crate::types::{EntryType, ListFilters, MemoryEntry, MemoryEntryInput, MemorySource};

use super::analysis::{
    arch_summarizer, file_analyzer, memory_entry_generator, pattern_extractor, techdebt,
    AnalysisBody, FileAnalysisResult, TechDebtMarker,
};
use super::analyzer::AnalysisDepth;
use super::git::{self, CrawlState};
use super::graph::{graph_builder::GraphBuilder, importance_scorer::ImportanceScorer};
use super::scanner::{scan_project, FileEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlMode {
    Full,
    Incremental,
    Auto,
}

impl CrawlMode {
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "incremental" => CrawlMode::Incremental,
            "full" => CrawlMode::Full,
            _ => CrawlMode::Auto,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CrawlResult {
    pub project_id: String,
    pub mode: CrawlMode,
    pub total_files: usize,
    pub analyzed_deep: usize,
    pub analyzed_medium: usize,
    pub analyzed_light: usize,
    pub skipped: usize,
    pub entries_saved: usize,
    pub entries_deprecated: usize,
    pub patterns_found: usize,
    pub techdebt_markers: usize,
    pub effective_mode: CrawlMode,
    pub files_changed: usize,
    /// False when this crawl deliberately left the cross-file patterns and the
    /// architecture summary as an earlier crawl built them. Only an unfocused
    /// full crawl regenerates them, so this is what the CLI note about them
    /// should be gated on — `effective_mode` alone misses the focused case.
    pub whole_project_entries_refreshed: bool,
    /// Present only when the crawl was asked to build the code graph.
    pub codegraph: Option<crate::codegraph::index::IndexOutcome>,
}

fn crawl_state_title(project_id: &str) -> String {
    format!("Crawl state: {project_id}")
}

/// Topic key of the crawl-state entry for a project.
pub fn crawl_state_key_for(project_id: &str) -> String {
    format!("scout:crawl-state:{project_id}")
}

/// Best-effort project root for a file being re-crawled on its own, used when
/// no crawl state records the root. Entry topic keys are built from
/// project-relative paths, so guessing wrong here means writing a duplicate
/// instead of superseding.
pub fn infer_project_root(file: &Path) -> Option<PathBuf> {
    const MARKERS: [&str; 5] = [
        ".git",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
    ];
    let mut cur = file.parent()?;
    loop {
        if MARKERS.iter().any(|m| cur.join(m).exists()) {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// Whether a scanned path lies under a focus filter.
///
/// A raw prefix test both over-matches (`src` catching `srcgen/`) and silently
/// matches nothing when the caller's separator is not the platform's — scanned
/// paths carry the native one.
fn under_focus(relative_path: &str, focus: &str) -> bool {
    let sep = std::path::MAIN_SEPARATOR;
    let normalise = |s: &str| s.replace(['/', '\\'], &sep.to_string());
    let focus = normalise(focus.trim_matches(['/', '\\']));
    if focus.is_empty() {
        return true;
    }
    let path = normalise(relative_path);
    path == focus || path.starts_with(&format!("{focus}{sep}"))
}

pub struct CrawlOrchestrator<'a> {
    librarian: &'a Arc<MemoryLibrarian>,
    project_id: String,
    mode: CrawlMode,
    focus: Option<String>,
    deep: bool,
}

impl<'a> CrawlOrchestrator<'a> {
    pub fn new(
        librarian: &'a Arc<MemoryLibrarian>,
        project_id: impl Into<String>,
        mode: CrawlMode,
        focus: Option<String>,
    ) -> Self {
        Self {
            librarian,
            project_id: project_id.into(),
            mode,
            focus,
            deep: false,
        }
    }

    /// Also build the symbol-level code graph. Opt-in while it beds in, and
    /// independent of the memory-entry pipeline: a codegraph failure is logged
    /// and never fails the crawl.
    pub fn with_deep(mut self, deep: bool) -> Self {
        self.deep = deep;
        self
    }

    pub async fn run(&self, root: &Path) -> StorageResult<CrawlResult> {
        tracing::info!(
            project = %self.project_id,
            root = %root.display(),
            mode = ?self.mode,
            "crawl starting"
        );

        // Phase 0: Resolve effective mode + load prior state if needed
        let prior_state = self.load_state().await?;
        let effective_mode = match self.mode {
            CrawlMode::Full => CrawlMode::Full,
            CrawlMode::Incremental => CrawlMode::Incremental,
            CrawlMode::Auto => {
                if prior_state.is_some() {
                    CrawlMode::Incremental
                } else {
                    CrawlMode::Full
                }
            }
        };
        tracing::info!(?effective_mode, "mode resolved");

        // Phase 1: Scan
        let mut scan = scan_project(root);
        if let Some(focus) = &self.focus {
            scan.files.retain(|f| under_focus(&f.relative_path, focus));
        }
        let file_count = scan.files.len();
        tracing::info!(files = file_count, "scan complete");

        // Phase 2: Build dependency graph (always over the full inventory so
        // import edges are accurate even when we only re-analyze a subset)
        // Framework-aware discovery: ranks entry points by confidence so
        // mono-repos with multiple frameworks rank correctly.
        let entry_candidates = crate::huginn::discovery::discover(&scan.files);
        for ep in &entry_candidates {
            tracing::info!(
                path = %ep.relative_path,
                framework = ep.framework.as_str(),
                confidence = ep.confidence,
                "entry-point candidate"
            );
        }
        let entry_points: Vec<String> = entry_candidates
            .iter()
            .map(|ep| ep.relative_path.clone())
            .collect();

        let graph = GraphBuilder::build(&scan.files, root, &entry_points);

        // Phase 2.5: Determine which files need re-analysis (incremental only)
        let analysis_set: Option<HashSet<String>> =
            if matches!(effective_mode, CrawlMode::Incremental) {
                if let Some(state) = &prior_state {
                    let changed = git::get_changed_files(root, state, &scan.files);
                    let affected = git::expand_to_affected(&changed, &graph);
                    tracing::info!(
                        has_git = changed.has_git,
                        changed_total = changed.added.len()
                            + changed.modified.len()
                            + changed.deleted.len()
                            + changed.renamed.len(),
                        affected = affected.len(),
                        "incremental change set"
                    );
                    Some(affected.into_iter().collect())
                } else {
                    None
                }
            } else {
                None
            };
        let files_changed = analysis_set.as_ref().map(|s| s.len()).unwrap_or(file_count);
        let incremental = matches!(effective_mode, CrawlMode::Incremental);
        let focused = self.focus.is_some();
        // Cross-file patterns and the architecture summary are whole-project
        // artifacts living under stable topic keys. An incremental crawl
        // narrows `scan.files` to the change set and a focus filter narrows it
        // to a subtree; either way phase 4 sees a fragment, and regenerating
        // from a fragment supersedes the complete version with a partial one.
        // The focused case is pre-existing rather than introduced by the
        // incremental work, but it is the same defect.
        let whole_project = !incremental && !focused;

        // Phase 2.6: Retire entries for files that no longer exist
        let mut entries_deprecated = self
            .deprecate_removed(&prior_state, root, &scan.files)
            .await;

        // Phase 2.7: Symbol-level code graph. Runs ahead of the no-change
        // shortcut below: the memory entries and the graph are indexed
        // independently, so a first `--deep` run must not be skipped just
        // because the entries are already current.
        let codegraph = if self.deep {
            self.index_codegraph(root, &scan.files, !incremental)
        } else {
            None
        };

        // Phase 3: Score importance over full graph
        let scores = ImportanceScorer::score_all(&graph);

        // An incremental crawl with an empty change set has nothing to write.
        // Falling through would recompute the patterns and the architecture
        // summary from an all-skipped inventory and supersede the full crawl's
        // versions, which share their topic keys.
        if incremental && files_changed == 0 {
            let state = self.build_persisted_state(root, &scan.files, &prior_state);
            if self.save_state(&state).await.is_err() {
                tracing::warn!("failed to persist crawl state");
            }
            tracing::info!("no changes since last crawl");
            return Ok(CrawlResult {
                project_id: self.project_id.clone(),
                mode: self.mode,
                total_files: file_count,
                analyzed_deep: 0,
                analyzed_medium: 0,
                analyzed_light: 0,
                skipped: file_count,
                entries_saved: 0,
                entries_deprecated,
                patterns_found: 0,
                techdebt_markers: 0,
                effective_mode,
                files_changed: 0,
                whole_project_entries_refreshed: whole_project,
                codegraph,
            });
        }

        // Phase 4: Analyze (full inventory in Full mode; affected subset in
        // Incremental mode).
        let mut analyses: Vec<FileAnalysisResult> = Vec::with_capacity(scan.files.len());
        for file in &scan.files {
            let in_set = analysis_set
                .as_ref()
                .is_none_or(|s| s.contains(&file.relative_path));
            let depth = if in_set {
                scores
                    .get(&file.relative_path)
                    .map(|s| s.analysis_depth)
                    .unwrap_or(AnalysisDepth::Skip)
            } else {
                AnalysisDepth::Skip
            };
            analyses.push(file_analyzer::analyze_file(file, depth, &graph));
        }

        let (deep, medium, light, skipped) = tally(&analyses);
        tracing::info!(deep, medium, light, skipped, "analysis complete");

        // Phase 5: Cross-file patterns. Unfocused full crawls only — a pattern
        // entry names every file that matched, so regenerating one from a
        // subset (the incremental change set, or a focus subtree) would
        // supersede the complete version with a shorter list.
        let patterns = if whole_project {
            pattern_extractor::extract(&analyses)
        } else {
            Vec::new()
        };
        tracing::info!(patterns = patterns.len(), "patterns extracted");

        // Phase 6: Aggregate tech debt across all files (not just deep).
        // `scanned_for_debt` records the files whose text this pass actually
        // ran the marker extractor over. That, and not "was in the scan", is
        // what licenses phase 8.5 to retire a marker-less file's entry: a file
        // we never read proves nothing about whether its TODOs are still there.
        let mut all_techdebt: Vec<TechDebtMarker> = Vec::new();
        let mut scanned_for_debt: HashSet<&str> = HashSet::new();
        for a in &analyses {
            match &a.body {
                AnalysisBody::Deep(d) => {
                    // Markdown at Deep/Medium depth takes the heading-structure
                    // path, which never fills `tech_debt_markers`. An empty list
                    // there means "not looked for", not "none left".
                    //
                    // The readability check is the same distinction: the deep
                    // analyzer swallows a read failure and analyses an empty
                    // string, which yields no markers and would otherwise look
                    // exactly like a file whose TODOs were removed.
                    if !is_markdown(&a.file_path) && root.join(&a.file_path).is_file() {
                        scanned_for_debt.insert(a.file_path.as_str());
                    }
                    all_techdebt.extend(d.tech_debt_markers.iter().cloned());
                }
                // Medium/light files were not read by the analyzer; pull
                // techdebt by scanning them here.
                AnalysisBody::Medium(_) | AnalysisBody::Light(_) => {
                    let abs = root.join(&a.file_path);
                    if let Ok(content) = fs::read_to_string(&abs) {
                        scanned_for_debt.insert(a.file_path.as_str());
                        all_techdebt.extend(techdebt::extract(&content, &a.file_path));
                    }
                }
                AnalysisBody::Skip => {}
            }
        }
        let techdebt_count = all_techdebt.len();

        // Phase 7: Architecture summary. Unfocused full crawls only, for the
        // same supersession reason as the patterns above — it counts and
        // classifies the whole inventory, which neither an incremental crawl
        // nor a focused one has analyzed.
        let summary = if whole_project {
            Some(arch_summarizer::summarize(
                &self.project_id,
                &analyses,
                &patterns,
                &all_techdebt,
            ))
        } else {
            None
        };

        // Phase 8: Save entries — per-file + patterns + techdebt + summary
        let mut saved = 0usize;

        for a in &analyses {
            for input in memory_entry_generator::file_entries(a, &self.project_id) {
                if self.librarian.propose(input).await.is_ok() {
                    saved += 1;
                }
            }
        }

        for p in &patterns {
            let input = memory_entry_generator::pattern_entry(p, &self.project_id);
            if self.librarian.propose(input).await.is_ok() {
                saved += 1;
            }
        }

        for input in memory_entry_generator::techdebt_entries(&all_techdebt, &self.project_id) {
            if self.librarian.propose(input).await.is_ok() {
                saved += 1;
            }
        }

        // Architecture summary entry — the stable topic_key makes each crawl
        // supersede the previous summary instead of accumulating copies.
        if let Some(summary) = &summary {
            let arch_input = MemoryEntryInput {
                title: format!("Architecture summary: {}", self.project_id),
                content: summary.formatted.clone(),
                entry_type: EntryType::Architecture,
                source: Some(MemorySource::Scout),
                tags: vec![
                    self.project_id.clone(),
                    "scout".into(),
                    "architecture-summary".into(),
                ],
                project_id: Some(self.project_id.clone()),
                topic_key: Some(format!("scout:arch:{}", self.project_id)),
                ..Default::default()
            };
            if self.librarian.propose(arch_input).await.is_ok() {
                saved += 1;
            }
        }

        // Phase 8.5: Retire the tech-debt entries of files this pass read and
        // found clean. Runs in every mode — the candidate set is bounded by
        // what was actually read, so an incremental crawl considers its change
        // set and a focused crawl its subtree.
        entries_deprecated += self
            .deprecate_clean_techdebt(&prior_state, root, &scanned_for_debt, &all_techdebt)
            .await;

        // Known gap: deleting a `.sql` file orphans its `scout:table:` entries.
        // `deprecate_removed` knows the path that vanished but not the table
        // names it declared — that mapping lives only in the entry's rendered
        // prose, never in a tag or in the crawl state — so it has no key to
        // look up. Reconciling by set difference was tried and removed: a crawl
        // cannot examine a `.sql` file at all (`.sql` is not a `Lang`, so it
        // never becomes a graph node, never gets an importance score, and
        // always analyzes at Skip), so the "complete set of keys this crawl
        // wrote" it would diff against is always empty, and the pass retires
        // every per-table entry in the project. Only `huginn_recrawl_file`
        // creates these, so nothing would put them back. The fix is to record
        // the declaring path on the entry, not to guess at it here.

        // Phase 9: Persist crawl state (always — captures snapshot of HEAD + hashes)
        let state = self.build_persisted_state(root, &scan.files, &prior_state);
        if self.save_state(&state).await.is_err() {
            tracing::warn!("failed to persist crawl state");
        }

        tracing::info!(saved, "crawl complete");

        Ok(CrawlResult {
            project_id: self.project_id.clone(),
            mode: self.mode,
            total_files: file_count,
            analyzed_deep: deep,
            analyzed_medium: medium,
            analyzed_light: light,
            skipped,
            entries_saved: saved,
            entries_deprecated,
            patterns_found: patterns.len(),
            techdebt_markers: techdebt_count,
            effective_mode,
            files_changed,
            whole_project_entries_refreshed: whole_project,
            codegraph,
        })
    }

    fn index_codegraph(
        &self,
        root: &Path,
        files: &[FileEntry],
        full: bool,
    ) -> Option<crate::codegraph::index::IndexOutcome> {
        use crate::codegraph::{index, store::CodeGraphStore};

        // Indexing replaces the project's whole graph, and a focused crawl has
        // deliberately narrowed the inventory, so running it here would delete
        // every definition outside the focus and then report full coverage of
        // what remains.
        if self.focus.is_some() {
            tracing::warn!("focused crawl: skipping the code graph, which is whole-project");
            return None;
        }

        // A background refresh may be writing this project's graph right now.
        // Wait for it rather than interleave: both rebuild the whole edge
        // table, and the refresh is short. Proceeding after the wait is
        // deliberate — an explicit crawl silently skipping the graph would be
        // worse than two writers against a WAL database with a busy timeout.
        let _lock = crate::codegraph::refresh::RefreshLock::acquire_bounded(
            &self.project_id,
            crate::codegraph::refresh::crawl_lock_wait(),
        );
        if _lock.is_none() {
            tracing::warn!(
                project = %self.project_id,
                "a graph refresh still holds the lock; indexing anyway"
            );
        }

        let store = match CodeGraphStore::open_default() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "codegraph unavailable; skipping deep index");
                return None;
            }
        };
        match index::index_project(&store, &self.project_id, root, files, full, None) {
            Ok(outcome) => {
                tracing::info!(
                    symbols = outcome.symbols,
                    edges = outcome.edges,
                    unresolved = outcome.unresolved_calls,
                    "codegraph indexed"
                );
                Some(outcome)
            }
            Err(e) => {
                tracing::warn!(error = %e, "codegraph indexing failed");
                None
            }
        }
    }

    /// Every live entry of `entry_type` in this project whose topic key starts
    /// with `prefix`, read in pages. One paged scan rather than a topic-key
    /// lookup per candidate file: on a full crawl that would be one round trip
    /// per source file, thousands of them, to retire a handful of entries.
    ///
    /// Returns nothing at all on a read error rather than the pages that did
    /// arrive — callers decide what to retire by set difference, and a list
    /// that silently stops early is indistinguishable from a complete one.
    async fn live_entries_under(&self, entry_type: EntryType, prefix: &str) -> Vec<MemoryEntry> {
        const PAGE: usize = 500;
        let mut out = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = match self
                .librarian
                .list(ListFilters {
                    entry_type: Some(entry_type),
                    project_id: Some(self.project_id.clone()),
                    limit: Some(PAGE),
                    offset: Some(offset),
                    ..Default::default()
                })
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(%prefix, error = %e, "listing entries for retirement failed");
                    return Vec::new();
                }
            };
            let fetched = page.len();
            for entry in page {
                let under_prefix = entry
                    .topic_key
                    .as_deref()
                    .is_some_and(|k| k.starts_with(prefix));
                if under_prefix {
                    out.push(entry);
                }
            }
            if fetched < PAGE {
                return out;
            }
            offset += PAGE;
        }
    }

    /// Live entries of `entry_type` carrying any of `tags`.
    ///
    /// Paged the same way as `live_entries_under` and filtered in Rust for the
    /// same reason: tags are stored as a JSON array in one column, so there is
    /// no index to ask. Callers must therefore keep the candidate set small and
    /// call this only when something actually needs retiring.
    async fn live_entries_tagged(
        &self,
        entry_type: EntryType,
        tags: &HashSet<String>,
    ) -> Vec<MemoryEntry> {
        const PAGE: usize = 500;
        let mut out = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = match self
                .librarian
                .list(ListFilters {
                    entry_type: Some(entry_type),
                    project_id: Some(self.project_id.clone()),
                    limit: Some(PAGE),
                    offset: Some(offset),
                    ..Default::default()
                })
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "listing tagged entries for retirement failed");
                    return Vec::new();
                }
            };
            let fetched = page.len();
            for entry in page {
                if entry.tags.iter().any(|t| tags.contains(t)) {
                    out.push(entry);
                }
            }
            if fetched < PAGE {
                return out;
            }
            offset += PAGE;
        }
    }

    /// The crawl state to persist, for either of the two exits that write one.
    ///
    /// A focused crawl scanned only its subtree, so the inventory it built
    /// describes a fragment of the project. Persisting that fragment as-is
    /// drops every other file from `file_hashes`, and that map is what
    /// `deprecate_removed` diffs against to notice deletions — so a file
    /// deleted outside the focus would never be seen as deleted by any later
    /// crawl, and its entries would stay live forever. Carrying the prior
    /// hashes forward keeps the record whole; `or_insert` and not `insert`
    /// because for files this crawl did scan, its own hash is the fresh one.
    fn build_persisted_state(
        &self,
        root: &Path,
        files: &[FileEntry],
        prior_state: &Option<CrawlState>,
    ) -> CrawlState {
        let mut state = git::build_state(root, &self.project_id, files);
        if self.focus.is_some() {
            if let Some(prior) = prior_state {
                for (path, hash) in &prior.file_hashes {
                    state
                        .file_hashes
                        .entry(path.clone())
                        .or_insert_with(|| hash.clone());
                }
            }
        }
        state
    }

    /// Retire the tech-debt entries of files this crawl read and found clean.
    /// `memory_entry_generator::techdebt_entries` only emits for files that
    /// HAVE markers, so without this a file whose last TODO was deleted keeps
    /// a live `scout:techdebt:` entry claiming debt that is gone.
    ///
    /// Candidates are the files whose text this pass ran the extractor over,
    /// minus the ones that produced markers — a set proportional to what
    /// changed, intersected against one paged read of the entries that exist.
    /// That containment is also the focus guard: a focused crawl never reads a
    /// file outside its subtree, so it can never retire one.
    async fn deprecate_clean_techdebt(
        &self,
        prior_state: &Option<CrawlState>,
        root: &Path,
        scanned: &HashSet<&str>,
        markers: &[TechDebtMarker],
    ) -> usize {
        // Topic keys are built from root-relative paths, so a crawl of the same
        // project from a different directory would retire the entry belonging to
        // a different file that still has its markers. `deprecate_removed`
        // refuses the same case.
        if let Some(prior) = prior_state {
            let prior_root = Path::new(&prior.project_root);
            if prior_root != root {
                tracing::warn!(
                    prior = %prior_root.display(),
                    current = %root.display(),
                    "crawl root changed; skipping tech-debt cleanup"
                );
                return 0;
            }
        }
        let with_markers: HashSet<&str> = markers.iter().map(|m| m.file_path.as_str()).collect();
        let clean: HashSet<&str> = scanned.difference(&with_markers).copied().collect();
        if clean.is_empty() {
            return 0;
        }

        let prefix = format!("scout:techdebt:{}:", self.project_id);
        let live = self.live_entries_under(EntryType::TechDebt, &prefix).await;
        let mut deprecated = 0usize;
        for entry in live {
            let is_clean = entry
                .topic_key
                .as_deref()
                .and_then(|k| k.strip_prefix(prefix.as_str()))
                .is_some_and(|path| clean.contains(path));
            if is_clean && self.librarian.deprecate(entry.id).await.is_ok() {
                deprecated += 1;
            }
        }

        if deprecated > 0 {
            tracing::info!(
                deprecated,
                "retired tech debt for files with no markers left"
            );
        }
        deprecated
    }

    /// Soft-delete the per-file entries of files that were present at the
    /// previous crawl and are gone now. Skipped while a focus filter is
    /// active: the scan set is deliberately narrow then, so every file outside
    /// the focus would look deleted.
    async fn deprecate_removed(
        &self,
        prior_state: &Option<CrawlState>,
        root: &Path,
        files: &[FileEntry],
    ) -> usize {
        if self.focus.is_some() {
            return 0;
        }
        let state = match prior_state {
            Some(s) => s,
            None => return 0,
        };

        // Recorded paths are relative to the root they were crawled from, so
        // comparing them against a scan of a different directory would make
        // every one of them look deleted. Phase 9 rewrites the state, so the
        // next crawl from this root retires normally.
        let prior_root = Path::new(&state.project_root);
        if prior_root != root {
            tracing::warn!(
                prior = %prior_root.display(),
                current = %root.display(),
                "crawl root changed; skipping removed-file cleanup"
            );
            return 0;
        }

        let current: HashSet<&str> = files.iter().map(|f| f.relative_path.as_str()).collect();
        let mut deprecated = 0usize;
        let mut vanished_sql: HashSet<String> = HashSet::new();

        for path in state.file_hashes.keys() {
            if current.contains(path.as_str()) {
                continue;
            }
            // Absent from the scan is not the same as gone from disk. The
            // scanner also drops symlinks and files that grew past its size
            // cap, and retiring those would discard knowledge about a file
            // that is still there.
            if root.join(path).exists() {
                continue;
            }
            if is_sql(path) {
                vanished_sql.insert(memory_entry_generator::sql_file_tag(path));
            }
            for topic_key in [
                format!("scout:file:{}:{}", self.project_id, path),
                format!("scout:techdebt:{}:{}", self.project_id, path),
            ] {
                match self
                    .librarian
                    .get_by_topic_key(Some(&self.project_id), &topic_key)
                    .await
                {
                    Ok(Some(entry)) => {
                        if self.librarian.deprecate(entry.id).await.is_ok() {
                            deprecated += 1;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(%topic_key, error = %e, "orphan lookup failed"),
                }
            }
        }

        deprecated += self.deprecate_sql_derived(&vanished_sql).await;

        if deprecated > 0 {
            tracing::info!(deprecated, "retired entries for removed files");
        }
        deprecated
    }

    /// Retire the table and alter entries a deleted `.sql` file declared.
    ///
    /// The loop above retires by exact topic key, which those entries do not
    /// have: a table entry is keyed by table name so the same table keeps one
    /// entry across migrations, and an alter entry has no key at all. Both
    /// carry a `file:` tag naming the file they came from, which is the only
    /// machine-readable link back — hence a scan rather than a lookup.
    ///
    /// The candidate set is the tags of `.sql` files that were recorded, are
    /// absent from this scan, and are gone from disk. That is a fact about
    /// specific files, not an inference from a set believed to be complete —
    /// the mistake that made the previous attempt at this delete every
    /// per-table entry in the project. An empty set does nothing at all.
    async fn deprecate_sql_derived(&self, vanished: &HashSet<String>) -> usize {
        if vanished.is_empty() {
            return 0;
        }
        let mut deprecated = 0usize;
        for entry_type in [EntryType::Architecture, EntryType::Context] {
            for entry in self.live_entries_tagged(entry_type, vanished).await {
                if self.librarian.deprecate(entry.id).await.is_ok() {
                    deprecated += 1;
                }
            }
        }
        if deprecated > 0 {
            tracing::info!(deprecated, "retired entries of deleted .sql files");
        }
        deprecated
    }

    async fn load_state(&self) -> StorageResult<Option<CrawlState>> {
        let entry = self
            .librarian
            .get_by_topic_key(Some(&self.project_id), &self.crawl_state_key())
            .await?;
        Ok(entry.and_then(|e| git::deserialize_state(&e.content)))
    }

    fn crawl_state_key(&self) -> String {
        crawl_state_key_for(&self.project_id)
    }

    async fn save_state(&self, state: &CrawlState) -> StorageResult<()> {
        let input = MemoryEntryInput {
            title: crawl_state_title(&self.project_id),
            content: git::serialize_state(state),
            entry_type: EntryType::Context,
            source: Some(MemorySource::Scout),
            tags: vec![
                self.project_id.clone(),
                "scout".into(),
                "crawl-state".into(),
            ],
            project_id: Some(self.project_id.clone()),
            topic_key: Some(self.crawl_state_key()),
            // This content is machine-readable, not prose. `load_state` runs
            // it through `git::deserialize_state`, which is
            // `serde_json::from_str(json).ok()` — so a truncated blob does
            // not error, it yields `None`, which reads as "no previous
            // state" and silently downgrades every later crawl to a full
            // one. The blob is dominated by `file_hashes` (99.8% of 127,680
            // chars on a 2,082-file project), so it exceeds the content
            // limit on any sizeable project.
            exact_content: true,
            ..Default::default()
        };
        self.librarian.propose(input).await?;
        Ok(())
    }
}

/// Exactly the extensions `file_analyzer` routes through the markdown
/// short-circuit — the reason a Deep body for one of these carries no
/// tech-debt markers.
fn is_markdown(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".md") || lower.ends_with(".mdx")
}

/// Narrows the tagged-entry scan to crawls where a `.sql` file actually
/// vanished. Only `.sql` files produce `file:`-tagged entries, so a crawl that
/// lost none cannot have orphaned one — this skips work, and no entry's fate
/// depends on it. The extension is lowercased because the scanner reports it
/// as written.
fn is_sql(path: &str) -> bool {
    path.to_lowercase().ends_with(".sql")
}

/// Matches `memory_entry_generator::file_entries`, which decides whether to fan
/// a file out into per-table entries on the lowercased suffix rather than on
fn tally(analyses: &[FileAnalysisResult]) -> (usize, usize, usize, usize) {
    let mut deep = 0;
    let mut medium = 0;
    let mut light = 0;
    let mut skipped = 0;
    for a in analyses {
        match a.depth {
            AnalysisDepth::Deep => deep += 1,
            AnalysisDepth::Medium => medium += 1,
            AnalysisDepth::Light => light += 1,
            AnalysisDepth::Skip => skipped += 1,
        }
    }
    (deep, medium, light, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::DisabledEmbeddingProvider;
    use crate::storage::sqlite::SqliteAdapter;
    use crate::storage::MemoryStorage;
    use std::fs;

    async fn test_librarian() -> Arc<MemoryLibrarian> {
        let storage = Arc::new(SqliteAdapter::in_memory("test").unwrap());
        storage.initialize().await.unwrap();
        let embedding = Arc::new(DisabledEmbeddingProvider);
        Arc::new(MemoryLibrarian::new(storage, embedding, "test", None))
    }

    /// A large project's crawl state exceeds the content limit — 99.8% of it
    /// is `file_hashes` — and it must survive the round trip whole.
    ///
    /// `load_state` parses through `git::deserialize_state`, which is
    /// `serde_json::from_str(json).ok()`. Truncation therefore does not
    /// error: it yields `None`, which reads as "no previous state", so every
    /// later crawl silently downgrades to a full one and incremental crawling
    /// is dead with nothing in the logs. Deleting `exact_content: true` from
    /// `save_state` must fail here.
    #[tokio::test]
    async fn a_large_crawl_state_round_trips_and_stays_parseable() {
        let librarian = test_librarian().await;
        let orch = CrawlOrchestrator::new(&librarian, "bigproj", CrawlMode::Full, None);

        let mut state = CrawlState {
            project_root: "/tmp/bigproj".into(),
            project_id: "bigproj".into(),
            ..Default::default()
        };
        // Enough files that the serialized blob clears the limit several
        // times over, as it does on any real project of size.
        for i in 0..4_000 {
            state
                .file_hashes
                .insert(format!("src/module_{i}/handler.rs"), format!("{i:016x}"));
        }
        let serialized_len = git::serialize_state(&state).chars().count();
        assert!(
            serialized_len > crate::librarian::content_limit(),
            "fixture must exceed the limit to be meaningful ({serialized_len} chars)"
        );

        orch.save_state(&state).await.unwrap();

        let loaded = orch
            .load_state()
            .await
            .unwrap()
            .expect("crawl state must still parse after a save/load round trip");
        assert_eq!(
            loaded.file_hashes.len(),
            4_000,
            "every file hash must survive; a truncated blob yields None and \
             silently turns every later crawl into a full one"
        );
        assert_eq!(loaded.project_id, "bigproj");
    }

    #[tokio::test]
    async fn crawls_fixture_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/auth")).unwrap();
        fs::write(
            root.join("src/main.ts"),
            "import { AuthService } from './auth/auth.service'\n\
             // TODO: wire middleware\n\
             export function bootstrap() { return new AuthService() }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/auth/auth.service.ts"),
            "import jwt from 'jsonwebtoken'\n\
             import bcrypt from 'bcrypt'\n\
             export const MAX_LOGIN_ATTEMPTS = 5\n\
             export class AuthService {\n\
               async login(u: string, p: string) {\n\
                 const h = await bcrypt.hash(p, 10)\n\
                 return jwt.sign({ u }, 's')\n\
               }\n\
             }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/auth/auth.guard.ts"),
            "// FIXME: wire canActivate\nexport class AuthGuard {\n  canActivate(ctx: any) { return true }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/auth/auth.middleware.ts"),
            "export function authMiddleware(req: any, res: any, next: any) { next() }\n",
        )
        .unwrap();

        let librarian = test_librarian().await;
        let orchestrator = CrawlOrchestrator::new(&librarian, "fixture", CrawlMode::Full, None);
        let result = orchestrator.run(root).await.unwrap();

        assert_eq!(result.total_files, 4);
        assert!(result.entries_saved > 0);
        assert!(
            result.techdebt_markers >= 2,
            "expected TODO + FIXME markers, got {}",
            result.techdebt_markers
        );
        // auth + data-access patterns both triggered by signals present in fixture
        assert!(result.patterns_found >= 1, "expected at least one pattern");

        // Verify entries landed in the "fixture" namespace (project_id)
        let stats = librarian.get_stats(Some("fixture")).await.unwrap();
        assert!(stats.total_entries > 0);
    }

    #[tokio::test]
    async fn auto_mode_uses_full_then_incremental() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.ts"), "export const a = 1\n").unwrap();
        fs::write(root.join("src/b.ts"), "export const b = 2\n").unwrap();

        let librarian = test_librarian().await;

        // First run: Auto → Full (no prior state)
        let r1 = CrawlOrchestrator::new(&librarian, "incp", CrawlMode::Auto, None)
            .run(root)
            .await
            .unwrap();
        assert_eq!(r1.effective_mode, CrawlMode::Full);
        assert_eq!(r1.files_changed, 2);

        // Second run with no changes: Auto → Incremental, files_changed = 0
        let r2 = CrawlOrchestrator::new(&librarian, "incp", CrawlMode::Auto, None)
            .run(root)
            .await
            .unwrap();
        assert_eq!(r2.effective_mode, CrawlMode::Incremental);
        assert_eq!(r2.files_changed, 0);

        // Modify one file, run incremental: only that file (and dependents) re-analyzed
        fs::write(root.join("src/a.ts"), "export const a = 999\n").unwrap();
        let r3 = CrawlOrchestrator::new(&librarian, "incp", CrawlMode::Incremental, None)
            .run(root)
            .await
            .unwrap();
        assert_eq!(r3.effective_mode, CrawlMode::Incremental);
        assert_eq!(
            r3.files_changed, 1,
            "only the modified file should be in the change set"
        );
    }

    /// Not a `#[tokio::test]`: the codegraph path is a process-wide env var, so
    /// it has to be set and restored around a runtime we own. Overriding it is
    /// what keeps a regression in the focus guard from writing to the real
    /// `~/.runar-forge/codegraph.db`.
    #[test]
    fn a_focused_deep_crawl_leaves_the_code_graph_alone() {
        use crate::codegraph::store::CodeGraphStore;

        let _guard = crate::test_support::HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("cg.db");
        let prev = std::env::var("RUNAR_CODEGRAPH_PATH").ok();
        std::env::set_var("RUNAR_CODEGRAPH_PATH", &graph_path);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let root = tmp.path().join("proj");
            fs::create_dir_all(root.join("src")).unwrap();
            fs::create_dir_all(root.join("lib")).unwrap();
            fs::write(
                root.join("src/a.ts"),
                "export function alpha() { return 1 }\n",
            )
            .unwrap();
            fs::write(
                root.join("lib/b.ts"),
                "export function beta() { return 2 }\n",
            )
            .unwrap();

            let librarian = test_librarian().await;
            let full = CrawlOrchestrator::new(&librarian, "deepfocus", CrawlMode::Full, None)
                .with_deep(true)
                .run(&root)
                .await
                .unwrap();
            let indexed = full.codegraph.expect("a deep crawl reports its index");
            assert!(indexed.symbols >= 2, "got {indexed:?}");

            let focused = CrawlOrchestrator::new(
                &librarian,
                "deepfocus",
                CrawlMode::Full,
                Some("src".to_string()),
            )
            .with_deep(true)
            .run(&root)
            .await
            .unwrap();
            assert!(
                focused.codegraph.is_none(),
                "a focused crawl must not touch the whole-project graph"
            );

            let store = CodeGraphStore::open(&graph_path).unwrap();
            let cov = store.coverage("deepfocus").unwrap();
            assert_eq!(
                cov.symbols, indexed.symbols,
                "the focused crawl replaced the graph with its subset"
            );
        });

        match prev {
            Some(v) => std::env::set_var("RUNAR_CODEGRAPH_PATH", v),
            None => std::env::remove_var("RUNAR_CODEGRAPH_PATH"),
        }
    }

    #[tokio::test]
    async fn incremental_crawl_preserves_architecture_summary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/main.ts"),
            "import { a } from './a'\nexport function boot() { return a }\n",
        )
        .unwrap();
        fs::write(root.join("src/a.ts"), "export const a = 1\n").unwrap();

        let librarian = test_librarian().await;
        let arch_key = "scout:arch:archp";

        CrawlOrchestrator::new(&librarian, "archp", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();
        let after_full = librarian
            .get_by_topic_key(Some("archp"), arch_key)
            .await
            .unwrap()
            .expect("a full crawl writes an architecture summary");

        // A no-op incremental must leave it completely alone.
        let r2 = CrawlOrchestrator::new(&librarian, "archp", CrawlMode::Auto, None)
            .run(root)
            .await
            .unwrap();
        assert_eq!(r2.effective_mode, CrawlMode::Incremental);
        assert_eq!(r2.files_changed, 0);
        let after_noop = librarian
            .get_by_topic_key(Some("archp"), arch_key)
            .await
            .unwrap()
            .expect("summary must survive a no-op incremental");
        assert_eq!(
            after_noop.id, after_full.id,
            "no-op incremental superseded the architecture summary"
        );
        assert_eq!(after_noop.content, after_full.content);

        // An incremental that does have work must not replace the whole-project
        // summary with one derived from the changed subset either.
        fs::write(root.join("src/a.ts"), "export const a = 42\n").unwrap();
        let r3 = CrawlOrchestrator::new(&librarian, "archp", CrawlMode::Incremental, None)
            .run(root)
            .await
            .unwrap();
        assert!(r3.files_changed > 0);
        assert_eq!(r3.patterns_found, 0, "incremental must not emit patterns");
        let after_change = librarian
            .get_by_topic_key(Some("archp"), arch_key)
            .await
            .unwrap()
            .expect("summary must survive an incremental with changes");
        assert_eq!(after_change.id, after_full.id);
        assert_eq!(after_change.content, after_full.content);
    }

    #[tokio::test]
    async fn removed_files_have_their_entries_retired() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.ts"), "export const main = 1\n").unwrap();
        fs::write(root.join("src/gone.ts"), "export const gone = 1\n").unwrap();

        let librarian = test_librarian().await;

        // First crawl records src/gone.ts in the crawl state.
        CrawlOrchestrator::new(&librarian, "rmp", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();

        let file_key = seed_file_entry(&librarian, "rmp", "src/gone.ts").await;
        let file_key = file_key.as_str();
        assert!(librarian
            .get_by_topic_key(Some("rmp"), file_key)
            .await
            .unwrap()
            .is_some());

        fs::remove_file(root.join("src/gone.ts")).unwrap();
        let r = CrawlOrchestrator::new(&librarian, "rmp", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();

        assert!(
            r.entries_deprecated >= 1,
            "expected the removed file's entry to be retired"
        );
        assert!(
            librarian
                .get_by_topic_key(Some("rmp"), file_key)
                .await
                .unwrap()
                .is_none(),
            "entry for a deleted file is still live"
        );
    }

    #[tokio::test]
    async fn focused_crawl_does_not_retire_entries_outside_the_focus() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("src/a.ts"), "export const a = 1\n").unwrap();
        fs::write(root.join("lib/b.ts"), "export const b = 2\n").unwrap();

        let librarian = test_librarian().await;
        CrawlOrchestrator::new(&librarian, "focusp", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();

        let r = CrawlOrchestrator::new(
            &librarian,
            "focusp",
            CrawlMode::Full,
            Some("src".to_string()),
        )
        .run(root)
        .await
        .unwrap();

        assert_eq!(
            r.entries_deprecated, 0,
            "a focused crawl must not retire entries for files it did not scan"
        );
    }

    #[test]
    fn infer_project_root_finds_the_nearest_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("crates/muninn/src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(root.join("crates/muninn/Cargo.toml"), "[package]\n").unwrap();
        let file = root.join("crates/muninn/src/main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        assert_eq!(
            infer_project_root(&file).unwrap(),
            root.join("crates/muninn"),
            "the innermost manifest wins"
        );

        // Nothing to anchor on: no root rather than a wrong one, so the caller
        // can refuse instead of writing an entry under a mismatched key.
        let bare = tempfile::tempdir().unwrap();
        fs::write(bare.path().join("loose.rs"), "\n").unwrap();
        assert!(infer_project_root(&bare.path().join("loose.rs")).is_none());
    }

    /// `FileEntry::relative_path` carries the platform separator, and topic
    /// keys are compared as strings, so a fixture path has to be built the
    /// same way the scanner builds one.
    fn native(rel: &str) -> String {
        rel.split('/')
            .collect::<Vec<_>>()
            .join(std::path::MAIN_SEPARATOR_STR)
    }

    /// Seed a per-file entry so retirement assertions do not depend on where
    /// the importance scorer happens to place a fixture.
    async fn seed_file_entry(librarian: &Arc<MemoryLibrarian>, project: &str, rel: &str) -> String {
        let rel = native(rel);
        let key = format!("scout:file:{project}:{rel}");
        librarian
            .propose(MemoryEntryInput {
                title: rel.clone(),
                content: format!("placeholder for {rel}"),
                entry_type: EntryType::Context,
                source: Some(MemorySource::Scout),
                project_id: Some(project.into()),
                topic_key: Some(key.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        key
    }

    /// Every live pattern entry keyed by topic key, so "the focused crawl left
    /// them alone" can be asserted without knowing which detectors fired.
    async fn pattern_snapshot(
        librarian: &Arc<MemoryLibrarian>,
        project: &str,
    ) -> std::collections::BTreeMap<String, (uuid::Uuid, String)> {
        librarian
            .list(ListFilters {
                entry_type: Some(EntryType::Pattern),
                project_id: Some(project.to_string()),
                limit: Some(200),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.topic_key.unwrap_or_default(), (e.id, e.content)))
            .collect()
    }

    #[tokio::test]
    async fn focused_full_crawl_leaves_patterns_and_the_summary_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/auth")).unwrap();
        fs::create_dir_all(root.join("lib/auth")).unwrap();
        fs::write(
            root.join("src/main.ts"),
            "import { AuthService } from './auth/auth.service'\n\
             export function bootstrap() { return new AuthService() }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/auth/auth.service.ts"),
            "import jwt from 'jsonwebtoken'\n\
             export class AuthService {\n  sign() { return jwt.sign({}, 's') }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/auth/auth.guard.ts"),
            "export class AuthGuard {\n  canActivate(ctx: any) { return true }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/auth/auth.middleware.ts"),
            "export function authMiddleware(req: any, res: any, next: any) { next() }\n",
        )
        .unwrap();
        // Outside the focus on purpose: dropping this file shortens the auth
        // pattern's file list and changes the summary's counts, which is what
        // makes a regenerated whole-project entry observably partial. Three
        // auth files remain inside `src`, so the detector still clears its
        // confidence floor and would rewrite the entry if it were allowed to.
        fs::write(
            root.join("lib/auth/auth.strategy.service.ts"),
            "export class JwtStrategy {\n  validate(p: any) { return p }\n}\n",
        )
        .unwrap();

        let librarian = test_librarian().await;
        let arch_key = "scout:arch:focusarch";

        let full = CrawlOrchestrator::new(&librarian, "focusarch", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();
        assert!(full.whole_project_entries_refreshed);
        assert!(
            full.patterns_found >= 1,
            "fixture must produce a cross-file pattern to protect"
        );
        let after_full = librarian
            .get_by_topic_key(Some("focusarch"), arch_key)
            .await
            .unwrap()
            .expect("a full crawl writes an architecture summary");
        let patterns_after_full = pattern_snapshot(&librarian, "focusarch").await;
        assert!(!patterns_after_full.is_empty());

        let focused = CrawlOrchestrator::new(
            &librarian,
            "focusarch",
            CrawlMode::Full,
            Some("src".to_string()),
        )
        .run(root)
        .await
        .unwrap();

        assert!(
            !focused.whole_project_entries_refreshed,
            "a focused crawl must report that it left the whole-project entries alone"
        );
        assert_eq!(
            focused.patterns_found, 0,
            "a focused crawl must not emit cross-file patterns"
        );
        let after_focus = librarian
            .get_by_topic_key(Some("focusarch"), arch_key)
            .await
            .unwrap()
            .expect("summary must survive a focused crawl");
        assert_eq!(
            after_focus.id, after_full.id,
            "a focused crawl superseded the whole-project architecture summary"
        );
        assert_eq!(after_focus.content, after_full.content);
        assert_eq!(
            pattern_snapshot(&librarian, "focusarch").await,
            patterns_after_full,
            "a focused crawl rewrote the cross-file patterns from a fragment"
        );
    }

    #[tokio::test]
    async fn a_file_that_lost_its_last_marker_stops_claiming_tech_debt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/auth")).unwrap();
        fs::write(
            root.join("src/main.ts"),
            "import { AuthService } from './auth/auth.service'\n\
             // TODO: wire middleware\n\
             export function bootstrap() { return new AuthService() }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/auth/auth.service.ts"),
            "export class AuthService {\n  login() { return true }\n}\n",
        )
        .unwrap();

        let librarian = test_librarian().await;
        let key = format!("scout:techdebt:debtp:{}", native("src/main.ts"));

        let first = CrawlOrchestrator::new(&librarian, "debtp", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();
        assert!(first.techdebt_markers >= 1, "fixture must produce a marker");
        assert!(
            librarian
                .get_by_topic_key(Some("debtp"), &key)
                .await
                .unwrap()
                .is_some(),
            "the TODO must produce a tech-debt entry for the next crawl to retire"
        );

        // The file stays; only its last marker goes.
        fs::write(
            root.join("src/main.ts"),
            "import { AuthService } from './auth/auth.service'\n\
             export function bootstrap() { return new AuthService() }\n",
        )
        .unwrap();
        let second = CrawlOrchestrator::new(&librarian, "debtp", CrawlMode::Incremental, None)
            .run(root)
            .await
            .unwrap();

        assert!(second.files_changed > 0);
        assert!(
            second.entries_deprecated >= 1,
            "the marker-less file's tech-debt entry should have been retired"
        );
        assert!(
            librarian
                .get_by_topic_key(Some("debtp"), &key)
                .await
                .unwrap()
                .is_none(),
            "entry still claims debt the file no longer has"
        );
    }

    #[tokio::test]
    async fn a_focused_crawl_leaves_tech_debt_outside_its_focus_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(
            root.join("src/main.ts"),
            "export function boot() { return 1 }\n",
        )
        .unwrap();
        fs::write(
            root.join("lib/legacy.service.ts"),
            "// FIXME: rewrite this\nexport class Legacy {}\n",
        )
        .unwrap();

        let librarian = test_librarian().await;
        let key = format!(
            "scout:techdebt:outfocus:{}",
            native("lib/legacy.service.ts")
        );

        CrawlOrchestrator::new(&librarian, "outfocus", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();
        assert!(
            librarian
                .get_by_topic_key(Some("outfocus"), &key)
                .await
                .unwrap()
                .is_some(),
            "fixture must produce a tech-debt entry outside the focus"
        );

        // The marker goes, but a crawl focused on `src` never reads that file,
        // so it has no grounds to conclude anything about its debt.
        fs::write(
            root.join("lib/legacy.service.ts"),
            "export class Legacy {}\n",
        )
        .unwrap();
        let focused = CrawlOrchestrator::new(
            &librarian,
            "outfocus",
            CrawlMode::Full,
            Some("src".to_string()),
        )
        .run(root)
        .await
        .unwrap();
        assert_eq!(
            focused.entries_deprecated, 0,
            "a focused crawl retired an entry for a file it never read"
        );
        assert!(
            librarian
                .get_by_topic_key(Some("outfocus"), &key)
                .await
                .unwrap()
                .is_some(),
            "focused crawl retired tech debt outside its focus"
        );

        // An unfocused crawl does read it, and only then retires it.
        let full = CrawlOrchestrator::new(&librarian, "outfocus", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();
        assert!(full.entries_deprecated >= 1);
        assert!(
            librarian
                .get_by_topic_key(Some("outfocus"), &key)
                .await
                .unwrap()
                .is_none(),
            "entry still claims debt the file no longer has"
        );
    }

    #[tokio::test]
    async fn crawling_the_same_project_from_another_root_retires_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("backend/src")).unwrap();
        fs::create_dir_all(root.join("frontend/src")).unwrap();
        fs::write(root.join("backend/src/app.ts"), "export const app = 1\n").unwrap();
        fs::write(root.join("frontend/src/ui.ts"), "export const ui = 1\n").unwrap();

        let librarian = test_librarian().await;
        CrawlOrchestrator::new(&librarian, "mono", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();
        let key = seed_file_entry(&librarian, "mono", "backend/src/app.ts").await;

        // Same project id, root one level down. Every recorded path is now
        // unreachable, but none of those files were deleted.
        let r = CrawlOrchestrator::new(&librarian, "mono", CrawlMode::Auto, None)
            .run(&root.join("backend"))
            .await
            .unwrap();

        assert_eq!(
            r.entries_deprecated, 0,
            "a crawl rooted elsewhere must not retire the previous root's entries"
        );
        assert!(
            librarian
                .get_by_topic_key(Some("mono"), &key)
                .await
                .unwrap()
                .is_some(),
            "entries were retired after a root change"
        );
    }

    /// The state a focused crawl persists is the inventory every later crawl
    /// diffs against to find deletions. If it only lists the focus subtree,
    /// a file deleted elsewhere never registers as deleted and its entries
    /// stay live forever.
    ///
    /// Goes through the no-change incremental exit deliberately: that is the
    /// ordinary outcome of focusing twice in a row, it returns before phase 9,
    /// and it writes its own state.
    #[tokio::test]
    async fn a_focused_crawl_keeps_the_whole_inventory_in_the_crawl_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(root.join("src/a.ts"), "export const a = 1\n").unwrap();
        fs::write(root.join("other/b.ts"), "export const b = 1\n").unwrap();

        let librarian = test_librarian().await;
        CrawlOrchestrator::new(&librarian, "focusstate", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();

        let orchestrator = CrawlOrchestrator::new(
            &librarian,
            "focusstate",
            CrawlMode::Auto,
            Some("src".to_string()),
        );
        orchestrator.run(root).await.unwrap();

        let state = orchestrator.load_state().await.unwrap().unwrap();
        assert!(
            state.file_hashes.contains_key(&native("other/b.ts")),
            "a focused crawl dropped the rest of the project from the inventory: {:?}",
            state.file_hashes.keys().collect::<Vec<_>>()
        );
        assert!(
            state.file_hashes.contains_key(&native("src/a.ts")),
            "the focused subtree is missing from its own crawl state"
        );

        // The point of keeping it: a deletion outside the focus is still seen.
        fs::remove_file(root.join("other/b.ts")).unwrap();
        let key = seed_file_entry(&librarian, "focusstate", "other/b.ts").await;
        CrawlOrchestrator::new(&librarian, "focusstate", CrawlMode::Auto, None)
            .run(root)
            .await
            .unwrap();
        assert!(
            librarian
                .get_by_topic_key(Some("focusstate"), &key)
                .await
                .unwrap()
                .is_none(),
            "a file deleted while a focused crawl ran is never retired"
        );
    }

    /// Recreate what `huginn_recrawl_file` does to a `.sql` file: analyze it at
    /// Deep depth and propose the entries. A crawl cannot produce these — `.sql`
    /// is not a `Lang`, so it always analyzes at `Skip` — which is exactly why
    /// they need retiring by a route other than the crawl's own bookkeeping.
    async fn recrawl_sql_file(
        librarian: &Arc<MemoryLibrarian>,
        project: &str,
        root: &Path,
        rel: &str,
    ) {
        let abs = root.join(rel);
        let metadata = fs::metadata(&abs).unwrap();
        let entry = FileEntry {
            relative_path: rel.to_string(),
            path: abs.clone(),
            size: metadata.len(),
            line_count: fs::read_to_string(&abs).unwrap().lines().count(),
            last_modified: metadata.modified().ok(),
            extension: "sql".into(),
        };
        let analysis = file_analyzer::analyze_file(
            &entry,
            AnalysisDepth::Deep,
            &crate::huginn::graph::DependencyGraph::default(),
        );
        for input in memory_entry_generator::file_entries(&analysis, project) {
            librarian.propose(input).await.unwrap();
        }
    }

    /// A deleted `.sql` file must take its per-table and alter entries with it.
    /// Neither can be reached by topic key — a table entry is keyed by table
    /// name so it survives across migrations, and an alter entry has no key at
    /// all — so both are found through the `file:` tag naming their source.
    #[tokio::test]
    async fn deleting_a_sql_file_retires_the_entries_it_declared() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("migrations")).unwrap();
        fs::write(
            root.join("migrations/001_init.sql"),
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);\n\
             ALTER TABLE users ADD COLUMN nickname TEXT;\n",
        )
        .unwrap();
        fs::write(
            root.join("migrations/002_keep.sql"),
            "CREATE TABLE orders (id INTEGER PRIMARY KEY);\n",
        )
        .unwrap();

        let librarian = test_librarian().await;
        CrawlOrchestrator::new(&librarian, "sqlret", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();
        recrawl_sql_file(&librarian, "sqlret", root, "migrations/001_init.sql").await;
        recrawl_sql_file(&librarian, "sqlret", root, "migrations/002_keep.sql").await;

        let users = librarian
            .get_by_topic_key(Some("sqlret"), "scout:table:sqlret:users")
            .await
            .unwrap();
        assert!(users.is_some(), "the fixture never produced a table entry");

        fs::remove_file(root.join("migrations/001_init.sql")).unwrap();
        CrawlOrchestrator::new(&librarian, "sqlret", CrawlMode::Auto, None)
            .run(root)
            .await
            .unwrap();

        assert!(
            librarian
                .get_by_topic_key(Some("sqlret"), "scout:table:sqlret:users")
                .await
                .unwrap()
                .is_none(),
            "the table entry outlived the only file that declared it"
        );
        assert!(
            librarian
                .get_by_topic_key(Some("sqlret"), "scout:table:sqlret:orders")
                .await
                .unwrap()
                .is_some(),
            "retired a table whose declaring file is still on disk"
        );

        // The alter entry has no topic key, so it is checked by title.
        let live = librarian
            .list(ListFilters {
                project_id: Some("sqlret".into()),
                limit: Some(500),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !live.iter().any(|e| e.title.starts_with("ALTER users")),
            "the alter entry of a deleted file stayed live"
        );
    }

    /// Deleting an unrelated file must not touch a `.sql`-derived entry. The
    /// `is_sql` gate makes this cheap rather than correct — the tag scan only
    /// ever matches the tag of a file that actually vanished — so this stays
    /// green with the gate disabled, which is the intended split: no entry's
    /// fate depends on classifying an extension.
    #[tokio::test]
    async fn deleting_a_non_sql_file_leaves_table_entries_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("schema.sql"),
            "CREATE TABLE users (id INTEGER);\n",
        )
        .unwrap();
        fs::write(root.join("src/a.ts"), "export const a = 1\n").unwrap();

        let librarian = test_librarian().await;
        CrawlOrchestrator::new(&librarian, "sqlkeep", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();
        recrawl_sql_file(&librarian, "sqlkeep", root, "schema.sql").await;

        fs::remove_file(root.join("src/a.ts")).unwrap();
        CrawlOrchestrator::new(&librarian, "sqlkeep", CrawlMode::Auto, None)
            .run(root)
            .await
            .unwrap();

        assert!(
            librarian
                .get_by_topic_key(Some("sqlkeep"), "scout:table:sqlkeep:users")
                .await
                .unwrap()
                .is_some(),
            "a table entry was retired because an unrelated file was deleted"
        );
    }

    #[tokio::test]
    async fn files_the_scanner_skips_are_not_treated_as_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.ts"), "export const a = 1\n").unwrap();
        fs::write(root.join("src/big.ts"), "export const big = 1\n").unwrap();

        let librarian = test_librarian().await;
        CrawlOrchestrator::new(&librarian, "skipp", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();
        let key = seed_file_entry(&librarian, "skipp", "src/big.ts").await;

        // Grow it past the scanner's 1 MiB cap: still on disk, no longer scanned.
        fs::write(root.join("src/big.ts"), "x".repeat(1_048_577)).unwrap();
        let r = CrawlOrchestrator::new(&librarian, "skipp", CrawlMode::Full, None)
            .run(root)
            .await
            .unwrap();

        assert_eq!(
            r.entries_deprecated, 0,
            "a file that outgrew the scanner's size cap still exists and must keep its entry"
        );
        assert!(librarian
            .get_by_topic_key(Some("skipp"), &key)
            .await
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn files_reachable_only_through_a_symlink_are_not_retired() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let outside = dir.path().join("outside");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("src/a.ts"), "export const a = 1\n").unwrap();
        fs::write(root.join("docs/b.ts"), "export const b = 1\n").unwrap();

        // Crawl while docs/ is a real directory, so docs/b.ts lands in the state.
        let librarian = test_librarian().await;
        CrawlOrchestrator::new(&librarian, "symp", CrawlMode::Full, None)
            .run(&root)
            .await
            .unwrap();
        let key = seed_file_entry(&librarian, "symp", "docs/b.ts").await;

        // Turn docs/ into a symlink. That is the state an upgrade produces: the
        // path is recorded from when it was walked, and the scanner no longer
        // emits it, but it still resolves on disk.
        fs::rename(root.join("docs"), outside.join("docs")).unwrap();
        std::os::unix::fs::symlink(outside.join("docs"), root.join("docs")).unwrap();
        assert!(root.join("docs/b.ts").exists());

        let r = CrawlOrchestrator::new(&librarian, "symp", CrawlMode::Full, None)
            .run(&root)
            .await
            .unwrap();

        assert_eq!(
            r.entries_deprecated, 0,
            "symlinked-but-present files must not be retired"
        );
        assert!(librarian
            .get_by_topic_key(Some("symp"), &key)
            .await
            .unwrap()
            .is_some());
    }
}
