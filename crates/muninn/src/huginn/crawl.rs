use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::librarian::MemoryLibrarian;
use crate::storage::StorageResult;
use crate::types::{EntryType, MemoryEntryInput, MemorySource};

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

pub struct CrawlOrchestrator<'a> {
    librarian: &'a Arc<MemoryLibrarian>,
    project_id: String,
    mode: CrawlMode,
    focus: Option<String>,
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
        }
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
            scan.files
                .retain(|f| f.relative_path.starts_with(focus.trim_start_matches('/')));
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

        // Phase 2.6: Retire entries for files that no longer exist
        let entries_deprecated = self
            .deprecate_removed(&prior_state, root, &scan.files)
            .await;

        // Phase 3: Score importance over full graph
        let scores = ImportanceScorer::score_all(&graph);

        // An incremental crawl with an empty change set has nothing to write.
        // Falling through would recompute the patterns and the architecture
        // summary from an all-skipped inventory and supersede the full crawl's
        // versions, which share their topic keys.
        if incremental && files_changed == 0 {
            let state = git::build_state(root, &self.project_id, &scan.files);
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

        // Phase 5: Cross-file patterns. Full crawls only — a pattern entry
        // names every file that matched, so regenerating one from the changed
        // subset would supersede the complete version with a shorter list.
        let patterns = if incremental {
            Vec::new()
        } else {
            pattern_extractor::extract(&analyses)
        };
        tracing::info!(patterns = patterns.len(), "patterns extracted");

        // Phase 6: Aggregate tech debt across all files (not just deep)
        let mut all_techdebt: Vec<TechDebtMarker> = Vec::new();
        for a in &analyses {
            if let AnalysisBody::Deep(d) = &a.body {
                all_techdebt.extend(d.tech_debt_markers.iter().cloned());
            }
        }
        // For medium/light files we didn't scan; pull techdebt by scanning head
        for a in &analyses {
            if matches!(a.body, AnalysisBody::Medium(_) | AnalysisBody::Light(_)) {
                let abs = root.join(&a.file_path);
                if let Ok(content) = fs::read_to_string(&abs) {
                    all_techdebt.extend(techdebt::extract(&content, &a.file_path));
                }
            }
        }
        let techdebt_count = all_techdebt.len();

        // Phase 7: Architecture summary. Full crawls only, for the same
        // supersession reason as the patterns above — it counts and classifies
        // the whole inventory, which an incremental crawl has not analyzed.
        let summary = if incremental {
            None
        } else {
            Some(arch_summarizer::summarize(
                &self.project_id,
                &analyses,
                &patterns,
                &all_techdebt,
            ))
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

        // Phase 9: Persist crawl state (always — captures snapshot of HEAD + hashes)
        let state = git::build_state(root, &self.project_id, &scan.files);
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
        })
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

        if deprecated > 0 {
            tracing::info!(deprecated, "retired entries for removed files");
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
            ..Default::default()
        };
        self.librarian.propose(input).await?;
        Ok(())
    }
}

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
