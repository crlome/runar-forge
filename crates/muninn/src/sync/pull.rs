//! `runar sync pull` — incremental remote → local replication.
//!
//! Phase 5.6.3. Refuses if `sync_state.last_pulled_updated_at` is NULL
//! (forces user to run `runar sync bootstrap` first — prevents an
//! accidental full-table scan disguised as a delta).

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};

use crate::storage::postgres::PostgresAdapter;
use crate::storage::sqlite::SqliteAdapter;
use crate::storage::MemoryStorage;
use crate::types::{ApplyOutcome, MemoryEntry};

/// Clock-skew margin between source and observer. Anything within this
/// window of `now()` is excluded so an inflight write isn't half-pulled.
pub const CLOCK_SKEW_SECS: i64 = 2;

async fn resolve_backends() -> Result<(Arc<dyn MemoryStorage>, Arc<dyn MemoryStorage>)> {
    let local_kind = std::env::var("RUNAR_STORAGE_LOCAL")
        .context("RUNAR_STORAGE_LOCAL not set — hybrid sync requires both backends")?;
    let remote_url = std::env::var("RUNAR_STORAGE_REMOTE")
        .context("RUNAR_STORAGE_REMOTE not set — hybrid sync requires both backends")?;
    let namespace = std::env::var("RUNAR_MEMORY_NAMESPACE").unwrap_or_else(|_| "default".into());

    let local: Arc<dyn MemoryStorage> = match local_kind.as_str() {
        "sqlite" => {
            let path = std::env::var("RUNAR_SQLITE_PATH")
                .unwrap_or_else(|_| "~/.runar-forge/memory.db".into());
            Arc::new(SqliteAdapter::new(&path, &namespace)?)
        }
        "postgresql" | "postgres" => {
            let url = std::env::var("RUNAR_DB_URL").context(
                "RUNAR_STORAGE_LOCAL=postgresql requires RUNAR_DB_URL for the local instance",
            )?;
            Arc::new(PostgresAdapter::new(&url, &namespace)?)
        }
        other => bail!("unknown RUNAR_STORAGE_LOCAL: {other}"),
    };
    local.initialize().await?;

    let remote: Arc<dyn MemoryStorage> = Arc::new(PostgresAdapter::new(&remote_url, &namespace)?);
    remote.initialize().await?;

    Ok((local, remote))
}

#[derive(Debug, Default)]
pub struct PullSummary {
    pub fetched: usize,
    pub applied_inserts: usize,
    pub applied_updates: usize,
    pub skipped: usize,
    pub conflicts: usize,
    pub final_cursor: Option<DateTime<Utc>>,
}

pub async fn cmd_pull(
    limit: usize,
    dry_run: bool,
    since_override: Option<DateTime<Utc>>,
) -> Result<()> {
    let (local, remote) = resolve_backends().await?;

    let mut state = local.read_sync_state().await?;
    if state.initialized_at.is_none() {
        bail!("sync_state not initialized — run `runar sync init` first");
    }

    let cursor = match (since_override, state.last_pulled_updated_at) {
        (Some(s), _) => Some(s),
        (None, Some(c)) => Some(c),
        (None, None) => bail!(
            "pull cursor is NULL — run `runar sync bootstrap` first to seed the local DB \
             (pull does NOT do full-table scans)"
        ),
    };

    let mut summary = PullSummary::default();
    let mut current_cursor = cursor;
    let mut total_fetched = 0usize;
    let max_iter = 100; // safety cap on background loops

    for _ in 0..max_iter {
        let batch: Vec<MemoryEntry> = remote
            .list_changed_since(current_cursor, CLOCK_SKEW_SECS, limit, None)
            .await
            .map_err(|e| anyhow!("remote list_changed_since: {e}"))?;
        if batch.is_empty() {
            break;
        }
        total_fetched += batch.len();
        let last_updated = batch.last().map(|e| e.updated_at);
        summary.fetched = total_fetched;

        if dry_run {
            println!(
                "[dry] would apply {} row(s); cursor advance to {:?}",
                batch.len(),
                last_updated
            );
            // Don't advance cursor in dry-run.
            break;
        }

        for entry in batch.iter() {
            match local.apply_remote_entry(entry.clone()).await {
                Ok(ApplyOutcome::Inserted) => summary.applied_inserts += 1,
                Ok(ApplyOutcome::UpdatedLww) => summary.applied_updates += 1,
                Ok(ApplyOutcome::SkippedNewerLocal) => summary.skipped += 1,
                Ok(ApplyOutcome::ConflictRecorded) => summary.conflicts += 1,
                Err(e) => {
                    tracing::warn!(error = %e, entry_id = %entry.id, "apply_remote_entry failed");
                }
            }
        }

        // Advance cursor only after the entire batch applied. Bumping
        // mid-batch on error would silently lose rows.
        current_cursor = last_updated;
        summary.final_cursor = current_cursor;

        if batch.len() < limit {
            break;
        }
    }

    if !dry_run {
        state.last_pulled_updated_at = current_cursor;
        state.last_pull_at = Some(Utc::now());
        if let Err(e) = local.write_sync_state(&state).await {
            tracing::warn!(error = %e, "write_sync_state failed");
        }
    }

    println!(
        "pull complete: fetched={} inserts={} updates={} skipped={} conflicts={} cursor={:?}",
        summary.fetched,
        summary.applied_inserts,
        summary.applied_updates,
        summary.skipped,
        summary.conflicts,
        summary.final_cursor
    );
    Ok(())
}
