//! `runar sync push` — drain the local outbox to the remote backend.
//!
//! Phase 5.6.2. Reuses the resolver from `sync::conflict` indirectly:
//! the remote's `apply_remote_entry` runs the resolver from its own
//! perspective. This module just shuttles claimed outbox rows over
//! and translates outcomes into confirm/fail/conflict bookkeeping.

use std::collections::HashMap;
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
    /// Stale claims released back to the queue before claiming.
    pub reaped: u64,
    /// Rows confirmed because a newer row for the same entry subsumed
    /// them, rather than because they were pushed.
    pub subsumed: usize,
}

/// Log loudly once a row has failed this many times.
const ATTEMPT_WARN_THRESHOLD: i32 = 5;

/// Dead-letter cap. `claim_outbox` refuses to claim a row at or beyond
/// this many attempts, so one poisoned payload cannot be retried
/// forever ahead of the rows behind it. Dogfood found a single row at
/// **64,634** attempts with this unenforced. Override with
/// `RUNAR_SYNC_MAX_ATTEMPTS`.
const DEFAULT_MAX_ATTEMPTS: i32 = 10;

/// Claims older than this are assumed to belong to a pusher that died.
/// Override with `RUNAR_SYNC_STALE_CLAIM_SECS`.
const DEFAULT_STALE_CLAIM_SECS: i64 = 900;

pub fn max_attempts() -> i32 {
    std::env::var("RUNAR_SYNC_MAX_ATTEMPTS")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

pub fn stale_claim_secs() -> i64 {
    std::env::var("RUNAR_SYNC_STALE_CLAIM_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_STALE_CLAIM_SECS)
}

pub async fn cmd_push(limit: usize, dry_run: bool) -> Result<()> {
    let (local, remote) = resolve_backends().await?;

    // Refuse if `sync init` has not run.
    let state = local.read_sync_state().await?;
    if state.initialized_at.is_none() {
        bail!("sync_state not initialized — run `runar sync init` first");
    }

    // Release claims stranded by a pusher that died between claim and
    // confirm. Without this they are invisible to `claim_outbox`
    // forever, because it only selects `claimed_at IS NULL`.
    let reaped = local
        .reap_stale_claims(stale_claim_secs())
        .await
        .map_err(|e| anyhow!("reap_stale_claims failed: {e}"))?;
    if reaped > 0 {
        println!("released {reaped} stale claim(s) back to the queue");
    }

    let attempt_cap = max_attempts();
    let claimed = local
        .claim_outbox(limit, attempt_cap)
        .await
        .map_err(|e| anyhow!("claim_outbox failed: {e}"))?;

    let mut summary = PushSummary {
        claimed: claimed.len(),
        reaped,
        ..Default::default()
    };

    if claimed.is_empty() {
        // Distinguish "drained" from "wedged" — both used to print the
        // same reassuring line.
        let health = local
            .outbox_health(attempt_cap)
            .await
            .map_err(|e| anyhow!("outbox_health failed: {e}"))?;
        if health.dead_lettered > 0 {
            println!(
                "nothing to push — {} row(s) dead-lettered at >= {attempt_cap} attempts. \
                 Inspect with `runar sync status`.",
                health.dead_lettered
            );
        } else {
            println!("nothing to push (outbox empty)");
        }
        return Ok(());
    }

    println!(
        "pushing {} row(s) to remote (limit={limit}{})",
        claimed.len(),
        if dry_run { ", DRY RUN" } else { "" }
    );

    // Coalesce: if multiple outbox rows reference the same entry_id,
    // the latest payload wins. The older rows are subsumed — they are
    // confirmed alongside the representative, never dropped on the
    // floor. Dropping them was the bug: `claim_outbox` had already set
    // their `claimed_at`, and nothing ever cleared it again, so they
    // became permanently unclaimable.
    let (coalesced, subsumed) = coalesce_latest(claimed);

    if dry_run {
        for row in &coalesced {
            println!(
                "  [dry] would push entry {} ({})",
                row.entry_id,
                row.op_kind.as_str()
            );
        }
        // A dry run claims rows it never pushes. Hand every one of them
        // back, or --dry-run silently poisons `limit` rows per run.
        let all: Vec<Uuid> = coalesced
            .iter()
            .map(|r| r.id)
            .chain(subsumed.values().flatten().copied())
            .collect();
        local
            .release_outbox(&all)
            .await
            .map_err(|e| anyhow!("release_outbox failed: {e}"))?;
        println!(
            "dry run complete: {} row(s) would be pushed, {} claim(s) released",
            coalesced.len(),
            all.len()
        );
        return Ok(());
    }

    let mut confirm: Vec<Uuid> = Vec::new();
    for row in coalesced {
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
                // Only now are the older rows for this entry safe to
                // retire: the payload that subsumes them has landed. If
                // the representative had failed, they must stay pending
                // so the retry still covers them.
                if let Some(older) = subsumed.get(&row.entry_id) {
                    summary.subsumed += older.len();
                    confirm.extend(older.iter().copied());
                }
            }
            Err(e) => {
                summary.failures += 1;
                if let Err(fe) = local.fail_outbox(row.id, &format!("{e}")).await {
                    tracing::warn!(error = %fe, "fail_outbox bookkeeping failed");
                }
                // Release the subsumed rows too — they were claimed and
                // are not being confirmed, so without this they strand.
                if let Some(older) = subsumed.get(&row.entry_id) {
                    if let Err(re) = local.release_outbox(older).await {
                        tracing::warn!(error = %re, "release_outbox for subsumed rows failed");
                    }
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
        "push complete: pushed={} conflicts={} failures={} subsumed={} (claimed={})",
        summary.pushed, summary.conflicts, summary.failures, summary.subsumed, summary.claimed
    );

    let health = local
        .outbox_health(attempt_cap)
        .await
        .map_err(|e| anyhow!("outbox_health failed: {e}"))?;
    if health.dead_lettered > 0 {
        println!(
            "  warning: {} row(s) dead-lettered at >= {attempt_cap} attempts and will not retry",
            health.dead_lettered
        );
    }
    Ok(())
}

/// Group outbox rows by `entry_id` and keep only the newest per group.
///
/// Returns the representatives to push, plus a map from `entry_id` to
/// the ids of the older rows it subsumes. Every claimed row appears in
/// exactly one of the two: the caller must confirm or release all of
/// them, because `claim_outbox` has already stamped `claimed_at` and
/// only `confirm_outbox`, `fail_outbox` or `release_outbox` clears it.
fn coalesce_latest(rows: Vec<OutboxRow>) -> (Vec<OutboxRow>, HashMap<Uuid, Vec<Uuid>>) {
    let mut latest: HashMap<Uuid, OutboxRow> = HashMap::new();
    let mut subsumed: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for row in rows {
        match latest.get(&row.entry_id) {
            Some(existing) if existing.created_at >= row.created_at => {
                subsumed.entry(row.entry_id).or_default().push(row.id);
            }
            _ => {
                if let Some(displaced) = latest.insert(row.entry_id, row) {
                    subsumed
                        .entry(displaced.entry_id)
                        .or_default()
                        .push(displaced.id);
                }
            }
        }
    }
    let mut out: Vec<OutboxRow> = latest.into_values().collect();
    out.sort_by_key(|a| a.created_at);
    (out, subsumed)
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
        let (coalesced, _) = coalesce_latest(rows);
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
        assert!(
            kept_age_secs <= 30,
            "expected newer row kept, got {kept_age_secs}s old"
        );
    }

    #[test]
    fn coalesce_empty_in_empty_out() {
        let (rows, subsumed) = coalesce_latest(vec![]);
        assert_eq!(rows.len(), 0);
        assert!(subsumed.is_empty());
    }

    /// The bug behind 606 permanently-stuck rows on the dogfood DB.
    ///
    /// `claim_outbox` stamps `claimed_at` on every row it hands back.
    /// Coalescing then kept one row per entry and dropped the rest — but
    /// dropped rows were never confirmed, failed or released, and
    /// `claim_outbox` only ever selects `claimed_at IS NULL`. They could
    /// never be seen again. Every claimed row must be accounted for.
    #[test]
    fn coalesce_accounts_for_every_claimed_row() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let rows = vec![
            outbox_row(id_a, 90),
            outbox_row(id_a, 60),
            outbox_row(id_a, 10), // representative for a
            outbox_row(id_b, 30), // sole row for b
        ];
        let claimed_ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();

        let (coalesced, subsumed) = coalesce_latest(rows);

        let mut seen: Vec<Uuid> = coalesced.iter().map(|r| r.id).collect();
        seen.extend(subsumed.values().flatten().copied());
        seen.sort();
        let mut expected = claimed_ids;
        expected.sort();
        assert_eq!(
            seen, expected,
            "every claimed row must be either pushed or recorded as subsumed"
        );

        assert_eq!(subsumed.get(&id_a).map(Vec::len), Some(2));
        assert!(
            !subsumed.contains_key(&id_b),
            "an entry with one row subsumes nothing"
        );
    }

    /// Insertion order must not change which rows are subsumed — the
    /// newest wins whether it arrives first or last.
    #[test]
    fn coalesce_is_order_independent() {
        let id = Uuid::new_v4();
        let newest = outbox_row(id, 10);
        let oldest = outbox_row(id, 90);

        let (fwd, fwd_sub) = coalesce_latest(vec![oldest.clone(), newest.clone()]);
        let (rev, rev_sub) = coalesce_latest(vec![newest.clone(), oldest.clone()]);

        assert_eq!(fwd[0].id, newest.id);
        assert_eq!(rev[0].id, newest.id);
        assert_eq!(fwd_sub.get(&id).unwrap(), &vec![oldest.id]);
        assert_eq!(rev_sub.get(&id).unwrap(), &vec![oldest.id]);
    }
}
