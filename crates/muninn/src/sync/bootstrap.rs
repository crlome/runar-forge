//! `runar sync bootstrap` — first-time full pull from remote into a
//! (typically empty) local DB.
//!
//! Phase 5.6.3. Distinct from `pull` because it does NOT require a
//! pre-set cursor and explicitly accepts the cost of a full-table
//! copy. Idempotent: applies LWW + verified resolver per row.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;

use crate::storage::postgres::PostgresAdapter;
use crate::storage::sqlite::SqliteAdapter;
use crate::storage::MemoryStorage;
use crate::sync::pull::CLOCK_SKEW_SECS;
use crate::types::{ApplyOutcome, MemoryEntry};

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

pub async fn cmd_bootstrap(
    project_filter: Option<String>,
    dry_run: bool,
    page_size: usize,
    yes_i_know: bool,
) -> Result<()> {
    let (local, remote) = resolve_backends().await?;

    let mut state = local.read_sync_state().await?;
    if state.initialized_at.is_none() {
        bail!("sync_state not initialized — run `runar sync init` first");
    }

    // Guard: if local already has rows AND user did not pass
    // --yes-i-know, refuse to surprise them. Bootstrap will run the
    // resolver per row but is still a "weird" operation on a populated
    // local.
    let local_stats = local
        .get_stats(
            std::env::var("RUNAR_MEMORY_NAMESPACE")
                .as_deref()
                .unwrap_or("default"),
        )
        .await?;
    if local_stats.total_entries > 0 && !yes_i_know && !dry_run {
        bail!(
            "local DB has {} entries — bootstrap on a populated DB requires --yes-i-know \
             (LWW + verified resolver still protects newer/verified local rows, but the \
             flag is required to confirm intent)",
            local_stats.total_entries
        );
    }

    println!(
        "bootstrap: pulling all rows from remote (page_size={page_size}{}{})",
        if dry_run { ", DRY RUN" } else { "" },
        match &project_filter {
            Some(p) => format!(", project={p}"),
            None => String::new(),
        }
    );

    let mut cursor: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut total_fetched = 0usize;
    let mut total_inserts = 0usize;
    let mut total_updates = 0usize;
    let mut total_skips = 0usize;
    let mut total_conflicts = 0usize;
    let project_ref = project_filter.as_deref();

    loop {
        let batch: Vec<MemoryEntry> = remote
            .list_changed_since(cursor, CLOCK_SKEW_SECS, page_size, project_ref)
            .await
            .map_err(|e| anyhow!("remote list_changed_since: {e}"))?;
        if batch.is_empty() {
            break;
        }
        total_fetched += batch.len();
        let last_updated = batch.last().map(|e| e.updated_at);

        if dry_run {
            println!("  [dry] would apply {} rows", batch.len());
        } else {
            for entry in batch.iter() {
                match local.apply_remote_entry(entry.clone()).await {
                    Ok(ApplyOutcome::Inserted) => total_inserts += 1,
                    Ok(ApplyOutcome::UpdatedLww) => total_updates += 1,
                    Ok(ApplyOutcome::SkippedNewerLocal) => total_skips += 1,
                    Ok(ApplyOutcome::ConflictRecorded) => total_conflicts += 1,
                    Err(e) => {
                        tracing::warn!(error = %e, entry_id = %entry.id, "apply_remote_entry failed");
                    }
                }
            }
        }

        // Progress.
        println!(
            "  ... fetched={total_fetched} inserts={total_inserts} updates={total_updates} skips={total_skips}"
        );

        cursor = last_updated;
        if batch.len() < page_size {
            break;
        }
    }

    if !dry_run {
        // Set cursor to (now - 2s) so the next `pull` only picks up
        // rows changed AFTER bootstrap completed. Don't use the last
        // batch's updated_at — that would miss any rows that landed
        // during the bootstrap itself.
        let safe_cursor = Utc::now() - chrono::Duration::seconds(CLOCK_SKEW_SECS);
        state.last_pulled_updated_at = Some(safe_cursor);
        state.last_pull_at = Some(Utc::now());
        if let Err(e) = local.write_sync_state(&state).await {
            tracing::warn!(error = %e, "write_sync_state failed");
        }
    }

    println!(
        "bootstrap complete: fetched={total_fetched} inserts={total_inserts} updates={total_updates} skips={total_skips} conflicts={total_conflicts}"
    );
    Ok(())
}
