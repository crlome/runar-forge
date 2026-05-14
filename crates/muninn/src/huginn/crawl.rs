use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::librarian::MemoryLibrarian;
use crate::storage::StorageResult;
use crate::types::{EntryType, ListFilters, MemoryEntryInput, MemorySource};

use super::analysis::{
    arch_summarizer, file_analyzer, memory_entry_generator, pattern_extractor, techdebt,
    AnalysisBody, FileAnalysisResult, TechDebtMarker,
};
use super::analyzer::AnalysisDepth;
use super::git::{self, CrawlState};
use super::graph::{graph_builder::GraphBuilder, importance_scorer::ImportanceScorer};
use super::scanner::scan_project;

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
    pub patterns_found: usize,
    pub techdebt_markers: usize,
    pub effective_mode: CrawlMode,
    pub files_changed: usize,
}

fn crawl_state_title(project_id: &str) -> String {
    format!("Crawl state: {project_id}")
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

        // Phase 3: Score importance over full graph
        let scores = ImportanceScorer::score_all(&graph);

        // Phase 4: Analyze (full inventory in Full mode; affected subset in
        // Incremental mode). Patterns + arch summary still see ALL files via
        // the full graph + a stub analysis (depth=Light) so they remain accurate.
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

        // Phase 5: Cross-file patterns
        let patterns = pattern_extractor::extract(&analyses);
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

        // Phase 7: Architecture summary
        let summary =
            arch_summarizer::summarize(&self.project_id, &analyses, &patterns, &all_techdebt);

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

        // Architecture summary entry (overwrites previous via dedup on title)
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
            ..Default::default()
        };
        if self.librarian.propose(arch_input).await.is_ok() {
            saved += 1;
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
            patterns_found: patterns.len(),
            techdebt_markers: techdebt_count,
            effective_mode,
            files_changed,
        })
    }

    async fn load_state(&self) -> StorageResult<Option<CrawlState>> {
        let entries = self
            .librarian
            .list(ListFilters {
                entry_type: Some(EntryType::Context),
                project_id: Some(self.project_id.clone()),
                namespace: Some(self.project_id.clone()),
                limit: Some(50),
                ..Default::default()
            })
            .await?;
        let title = crawl_state_title(&self.project_id);
        let entry = entries.into_iter().find(|e| e.title == title);
        Ok(entry.and_then(|e| git::deserialize_state(&e.content)))
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
            topic_key: Some(format!("scout:crawl-state:{}", self.project_id)),
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
}
