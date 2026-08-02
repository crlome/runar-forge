//! `runar sync status` — read-only summary of sync state.
//!
//! Phase 5.6.3. Reports outbox depth, last push/pull, cursor, and
//! recent conflicts. Schema-version + dim re-fetched live so a doctor
//! command isn't strictly necessary alongside this.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;

use crate::storage::postgres::PostgresAdapter;
use crate::storage::sqlite::SqliteAdapter;
use crate::storage::MemoryStorage;

async fn resolve_local() -> Result<Arc<dyn MemoryStorage>> {
    let local_kind = std::env::var("RUNAR_STORAGE_LOCAL")
        .context("RUNAR_STORAGE_LOCAL not set — hybrid sync requires both backends")?;
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

pub async fn cmd_status(json_out: bool) -> Result<()> {
    let local = resolve_local().await?;
    let state = local.read_sync_state().await?;
    let attempt_cap = crate::sync::push::max_attempts();
    let health = local
        .outbox_health(attempt_cap)
        .await
        .map_err(|e| anyhow!("outbox_health: {e}"))?;
    let depth = health.total() as usize;
    let conflicts = local
        .list_conflicts(10)
        .await
        .map_err(|e| anyhow!("list_conflicts: {e}"))?;

    let auto_enabled = std::env::var("RUNAR_SYNC_AUTO")
        .map(|v| matches!(v.as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(false);
    let hb = crate::sync::heartbeat::read();
    let loop_alive = hb.is_some()
        && !crate::sync::heartbeat::is_stale(
            (std::env::var("RUNAR_SYNC_MAX_BACKOFF_MS")
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(300_000)
                / 1000)
                * 2,
        );

    if json_out {
        let payload = json!({
            "initialized_at": state.initialized_at,
            "last_push_at": state.last_push_at,
            "last_pull_at": state.last_pull_at,
            "last_pulled_updated_at": state.last_pulled_updated_at,
            "local_dim": state.local_dim,
            "remote_dim": state.remote_dim,
            "local_schema_version": state.local_schema_version,
            "remote_schema_version": state.remote_schema_version,
            "outbox_depth": depth,
            "outbox_pending": health.pending,
            "outbox_in_flight": health.in_flight,
            "outbox_dead_lettered": health.dead_lettered,
            "outbox_oldest_unconfirmed": health.oldest_unconfirmed,
            "outbox_max_attempts_seen": health.max_attempts_seen,
            "outbox_wedged": health.is_wedged(),
            "recent_conflicts": conflicts.len(),
            "auto_enabled": auto_enabled,
            "loop_alive": loop_alive,
            "last_tick": hb.as_ref().map(|h| h.last_tick),
            "current_backoff_ms": hb.as_ref().map(|h| h.current_backoff_ms),
            "next_run_at": hb.as_ref().map(|h| h.next_run_at),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("runar sync status");
    println!();
    match state.initialized_at {
        Some(t) => println!("  initialized at:        {t}"),
        None => println!("  initialized at:        <never>  (run `runar sync init`)"),
    }
    println!(
        "  local schema:          {}",
        state.local_schema_version.as_deref().unwrap_or("?")
    );
    println!(
        "  remote schema:         {}",
        state.remote_schema_version.as_deref().unwrap_or("?")
    );
    println!(
        "  embedding dim:         local={} remote={}",
        state
            .local_dim
            .map(|d| d.to_string())
            .unwrap_or_else(|| "?".into()),
        state
            .remote_dim
            .map(|d| d.to_string())
            .unwrap_or_else(|| "?".into())
    );
    println!("  last push:             {:?}", state.last_push_at);
    println!("  last pull:             {:?}", state.last_pull_at);
    println!(
        "  last pulled cursor:    {:?}",
        state.last_pulled_updated_at
    );
    println!("  outbox depth:          {depth}");
    if depth > 0 {
        println!(
            "    claimable:           {}\n    in flight:           {}\n    dead-lettered:       {} (>= {attempt_cap} attempts)",
            health.pending, health.in_flight, health.dead_lettered
        );
        if let Some(oldest) = health.oldest_unconfirmed {
            println!(
                "    oldest unconfirmed:  {} ({} attempts max)",
                oldest.format("%Y-%m-%d %H:%M:%S"),
                health.max_attempts_seen
            );
        }
        if health.is_wedged() {
            println!(
                "    WEDGED — nothing claimable. `runar sync push` will report \
                 \"nothing to push\" while the backlog never drains."
            );
        }
    }
    println!("  recent conflicts:      {}", conflicts.len());
    println!(
        "  auto-sync:             {}{}",
        if auto_enabled { "enabled" } else { "disabled" },
        if auto_enabled {
            if loop_alive {
                " (loop alive)"
            } else {
                " (loop NOT running — restart mcp-muninn)"
            }
        } else {
            ""
        }
    );
    if let Some(h) = &hb {
        println!(
            "  last loop tick:        {} (backoff {}ms, next at {})",
            h.last_tick.format("%H:%M:%S"),
            h.current_backoff_ms,
            h.next_run_at.format("%H:%M:%S")
        );
    }

    if !conflicts.is_empty() {
        println!();
        println!("  Recent conflicts (most recent first):");
        for c in conflicts.iter().take(5) {
            println!(
                "    {} entry={} policy={:?} winner={:?} dir={:?}",
                c.created_at.format("%Y-%m-%d %H:%M:%S"),
                c.entry_id,
                c.policy,
                c.winner_side,
                c.direction
            );
        }
    }
    Ok(())
}
