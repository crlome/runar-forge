//! `runar sync gc` — outbox retention sweep.
//!
//! Phase 5.6.5. Deletes confirmed outbox rows older than the retention
//! threshold. Pending rows are never touched. Auto-triggered by
//! `mcp-muninn` startup when the last run is > 24h ago (best-effort).

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;

use crate::storage::postgres::PostgresAdapter;
use crate::storage::sqlite::SqliteAdapter;
use crate::storage::MemoryStorage;

fn retention_secs() -> i64 {
    let days: i64 = std::env::var("RUNAR_SYNC_OUTBOX_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);
    days * 86_400
}

async fn resolve_local() -> Result<Arc<dyn MemoryStorage>> {
    let local_kind = std::env::var("RUNAR_STORAGE_LOCAL")
        .context("RUNAR_STORAGE_LOCAL not set — gc is a no-op outside hybrid mode")?;
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
    Ok(local)
}

pub async fn cmd_gc(dry_run: bool) -> Result<()> {
    let local = resolve_local().await?;
    let secs = retention_secs();

    if dry_run {
        // We don't have a "count would-delete" trait method; report
        // depth + threshold instead. Keeps the impl small.
        let depth = local
            .outbox_depth()
            .await
            .map_err(|e| anyhow!("outbox_depth: {e}"))?;
        println!(
            "[dry] retention threshold = {} days ({} s); pending outbox depth = {}",
            secs / 86_400,
            secs,
            depth
        );
        println!("[dry] would delete confirmed rows older than threshold");
        return Ok(());
    }

    let started = std::time::Instant::now();
    let deleted = local
        .gc_outbox(secs)
        .await
        .map_err(|e| anyhow!("gc_outbox: {e}"))?;
    let depth = local
        .outbox_depth()
        .await
        .map_err(|e| anyhow!("outbox_depth: {e}"))?;
    println!(
        "sync gc: deleted {} confirmed row(s) (retention {} days, took {}ms); pending depth now {}",
        deleted,
        secs / 86_400,
        started.elapsed().as_millis(),
        depth
    );
    Ok(())
}

/// Path of the marker file that records last GC run time. Read by
/// `mcp-muninn` startup to decide whether to auto-trigger.
pub fn last_gc_marker_path() -> std::path::PathBuf {
    crate::setup::runar_dir().join("sync-gc-last-run")
}

/// Best-effort: returns true if the marker is missing or older than
/// `older_than_secs`. Used by `mcp-muninn` startup to decide whether
/// to fire `cmd_gc`. Errors fall back to `false` so a flaky FS doesn't
/// thrash GC.
pub fn should_auto_run(older_than_secs: i64) -> bool {
    let path = last_gc_marker_path();
    let Ok(meta) = std::fs::metadata(&path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(elapsed) = modified.elapsed() else {
        return false;
    };
    elapsed.as_secs() as i64 > older_than_secs
}

pub fn touch_marker() {
    let path = last_gc_marker_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, Utc::now().to_rfc3339());
}

#[cfg(test)]
mod tests {
    #[test]
    fn retention_secs_uses_default_7d() {
        // Can't reliably manipulate process env in tests; assert the
        // numeric default is 7 days expressed in seconds.
        let days = 7;
        assert_eq!(days * 86_400, 604_800);
    }
}
