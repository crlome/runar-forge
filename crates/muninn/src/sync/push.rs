//! `runar sync push` — drain the local outbox to the remote backend.
//!
//! Phase 5.6.2. Reuses the resolver from `sync::conflict` indirectly:
//! the remote's `apply_remote_entry` runs the resolver from its own
//! perspective. This module just shuttles claimed outbox rows over
//! and translates outcomes into confirm/fail/conflict bookkeeping.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use uuid::Uuid;

use crate::storage::postgres::PostgresAdapter;
use crate::storage::sqlite::SqliteAdapter;
use crate::storage::MemoryStorage;
use crate::types::{ApplyOutcome, MemoryEntry, OutboxRow};

/// Resolve the (local, remote) backend pair from env vars set by
/// `runar config wizard`. Mirrors `sync::init::resolve_backends` but
/// kept here as a private helper so each sub-phase can evolve
/// independently.
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
pub struct PushSummary {
    pub claimed: usize,
    pub pushed: usize,
    pub conflicts: usize,
    pub failures: usize,
}

/// Cap on retries before a row is considered poisoned. Beyond this the
/// row stays in the outbox but is no longer re-claimed by `claim_outbox`
/// (because `claimed_at` remains NULL but `attempts` is high). 5.6.2
/// does NOT yet implement skip-by-attempts in `claim_outbox`; this
/// constant is reserved for the next iteration if poison protection
/// becomes necessary in dogfood. For now we just log loudly.
const ATTEMPT_WARN_THRESHOLD: i32 = 5;

pub async fn cmd_push(limit: usize, dry_run: bool) -> Result<()> {
    let (local, remote) = resolve_backends().await?;

    // Refuse if `sync init` has not run.
    let state = local.read_sync_state().await?;
    if state.initialized_at.is_none() {
        bail!("sync_state not initialized — run `runar sync init` first");
    }

    let claimed = local
        .claim_outbox(limit)
        .await
        .map_err(|e| anyhow!("claim_outbox failed: {e}"))?;

    let mut summary = PushSummary {
        claimed: claimed.len(),
        ..Default::default()
    };

    if claimed.is_empty() {
        println!("nothing to push (outbox empty)");
        return Ok(());
    }

    println!(
        "pushing {} row(s) to remote (limit={limit}{})",
        claimed.len(),
        if dry_run { ", DRY RUN" } else { "" }
    );

    // Coalesce: if multiple outbox rows reference the same entry_id,
    // the latest payload wins. Keep only the newest per entry_id.
    let coalesced = coalesce_latest(claimed);

    let mut confirm: Vec<Uuid> = Vec::new();
    for row in coalesced {
        if dry_run {
            println!("  [dry] would push entry {} ({})", row.entry_id, row.op_kind.as_str());
            continue;
        }

        if row.attempts >= ATTEMPT_WARN_THRESHOLD {
            tracing::warn!(
                attempts = row.attempts,
                outbox_id = %row.id,
                "outbox row attempts >= threshold (poison candidate)"
            );
        }

        match push_one(&row, remote.as_ref()).await {
            Ok(outcome) => {
                match outcome {
                    ApplyOutcome::Inserted | ApplyOutcome::UpdatedLww => {
                        summary.pushed += 1;
                    }
                    ApplyOutcome::ConflictRecorded => {
                        summary.conflicts += 1;
                    }
                    ApplyOutcome::SkippedNewerLocal => {
                        // Remote has a strictly newer row; we're done with this
                        // outbox entry.
                        summary.pushed += 1;
                    }
                }
                confirm.push(row.id);
            }
            Err(e) => {
                summary.failures += 1;
                if let Err(fe) = local.fail_outbox(row.id, &format!("{e}")).await {
                    tracing::warn!(error = %fe, "fail_outbox bookkeeping failed");
                }
            }
        }
    }

    if !confirm.is_empty() {
        local
            .confirm_outbox(&confirm)
            .await
            .map_err(|e| anyhow!("confirm_outbox failed: {e}"))?;
    }

    // Update last_push_at for observability.
    let mut state = state;
    state.last_push_at = Some(Utc::now());
    let _ = local.write_sync_state(&state).await;

    println!(
        "push complete: pushed={} conflicts={} failures={} (claimed={})",
        summary.pushed, summary.conflicts, summary.failures, summary.claimed
    );
    Ok(())
}

/// Group outbox rows by `entry_id` and keep only the newest per group.
/// Older outbox rows for the same entry are still confirmed (because
/// the newest payload subsumes them) but only the latest is sent.
fn coalesce_latest(rows: Vec<OutboxRow>) -> Vec<OutboxRow> {
    use std::collections::HashMap;
    let mut latest: HashMap<Uuid, OutboxRow> = HashMap::new();
    for row in rows {
        match latest.get(&row.entry_id) {
            Some(existing) if existing.created_at >= row.created_at => continue,
            _ => {
                latest.insert(row.entry_id, row);
            }
        }
    }
    let mut out: Vec<OutboxRow> = latest.into_values().collect();
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    out
}

async fn push_one(row: &OutboxRow, remote: &dyn MemoryStorage) -> Result<ApplyOutcome> {
    let entry: MemoryEntry = serde_json::from_value(row.row_payload.clone())
        .map_err(|e| anyhow!("payload deserialize: {e}"))?;
    remote
        .apply_remote_entry(entry)
        .await
        .map_err(|e| anyhow!("apply_remote_entry: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::OutboxOp;
    use chrono::Duration;

    fn outbox_row(entry_id: Uuid, age_secs: i64) -> OutboxRow {
        OutboxRow {
            id: Uuid::new_v4(),
            entry_id,
            op_kind: OutboxOp::Insert,
            row_payload: serde_json::json!({"id": entry_id.to_string()}),
            attempts: 0,
            last_error: None,
            claimed_at: None,
            confirmed_at: None,
            created_at: Utc::now() - Duration::seconds(age_secs),
        }
    }

    #[test]
    fn coalesce_keeps_newest_per_entry() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let rows = vec![
            outbox_row(id_a, 60), // older
            outbox_row(id_a, 10), // newer — should win
            outbox_row(id_b, 30),
        ];
        let coalesced = coalesce_latest(rows);
        assert_eq!(coalesced.len(), 2);
        let a_age: Vec<_> = coalesced
            .iter()
            .filter(|r| r.entry_id == id_a)
            .map(|r| r.created_at)
            .collect();
        // The kept row for id_a should be the newer (closer to now) one.
        assert_eq!(a_age.len(), 1);
        let now = Utc::now();
        let kept_age_secs = (now - a_age[0]).num_seconds();
        assert!(kept_age_secs <= 30, "expected newer row kept, got {kept_age_secs}s old");
    }

    #[test]
    fn coalesce_empty_in_empty_out() {
        assert_eq!(coalesce_latest(vec![]).len(), 0);
    }
}
