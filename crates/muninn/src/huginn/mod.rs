pub mod analysis;
pub mod analyzer;
pub mod benchmark;
pub mod crawl;
pub mod discovery;
pub mod git;
pub mod graph;
#[cfg(test)]
mod parser_spike;
pub mod scanner;

use std::path::Path;
use std::sync::Arc;

use crate::librarian::MemoryLibrarian;
use crate::storage::StorageResult;

pub use crawl::{CrawlMode, CrawlOrchestrator, CrawlResult};

/// Backward-compatible entry point — runs a full auto crawl with no focus.
pub async fn crawl_project(
    root: &Path,
    project_id: &str,
    librarian: &Arc<MemoryLibrarian>,
) -> StorageResult<CrawlResult> {
    crawl_project_with_mode(root, project_id, librarian, CrawlMode::Auto).await
}

pub async fn crawl_project_with_mode(
    root: &Path,
    project_id: &str,
    librarian: &Arc<MemoryLibrarian>,
    mode: CrawlMode,
) -> StorageResult<CrawlResult> {
    CrawlOrchestrator::new(librarian, project_id, mode, None)
        .run(root)
        .await
}
