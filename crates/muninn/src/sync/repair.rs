//! `runar sync repair` — undo the two ways the outbox wedged itself.
//!
//! Both problems are historical: the code paths that produced them are
//! fixed, but a database written by an older binary still carries the
//! damage, and neither problem heals on its own.
//!
//! 1. **Stranded claims.** `claim_outbox` stamps `claimed_at` and only
//!    ever selects rows where it is NULL. Any row claimed but never
//!    confirmed, failed or released is invisible forever. `sync push`
//!    reaps stale claims on every run now, so this is mostly a way to
//!    fix it without waiting for the staleness cutoff.
//!
//! 2. **Missing tombstones.** `deprecate` used to read its outbox
//!    payload *after* the soft-delete, and `get` filters
//!    `deleted_at IS NULL`, so the read failed and the enqueue bailed —
//!    silently, since outbox writes are best-effort. Entries the local
//!    side deleted are queued to the remote as inserts with no delete
//!    behind them. Draining that outbox resurrects them.
//!
//! Enqueueing a tombstone is additive: it appends outbox rows and
//! touches no memory entry. Nothing here deletes anything.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};

use crate::storage::postgres::PostgresAdapter;
use crate::storage::sqlite::SqliteAdapter;
use crate::storage::MemoryStorage;
use crate::types::{OutboxInput, OutboxOp};

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

pub async fn cmd_repair(dry_run: bool, limit: usize, release_all: bool) -> Result<()> {
    let local = resolve_local().await?;
    let cap = crate::sync::push::max_attempts();

    let before = local
        .outbox_health(cap)
        .await
        .map_err(|e| anyhow!("outbox_health: {e}"))?;

    println!(
        "runar sync repair{}",
        if dry_run { " (DRY RUN)" } else { "" }
    );
    println!();
    println!("  outbox before:");
    println!("    unconfirmed:   {}", before.total());
    println!("    claimable:     {}", before.pending);
    println!("    in flight:     {}", before.in_flight);
    println!(
        "    dead-lettered: {} (>= {cap} attempts)",
        before.dead_lettered
    );

    // ── 1. stranded claims ────────────────────────────────────────
    // `release_all` uses a 0s cutoff, which also releases claims held
    // by a pusher running right now. Only correct when nothing is
    // pushing, hence the opt-in flag.
    let cutoff = if release_all {
        0
    } else {
        crate::sync::push::stale_claim_secs()
    };
    if before.in_flight > 0 {
        if dry_run {
            println!(
                "\n  would release claims older than {cutoff}s \
                 (up to {} in flight)",
                before.in_flight
            );
        } else {
            let released = local
                .reap_stale_claims(cutoff)
                .await
                .map_err(|e| anyhow!("reap_stale_claims: {e}"))?;
            println!("\n  released {released} stranded claim(s)");
        }
    }

    // ── 2. missing tombstones ─────────────────────────────────────
    let orphans = local
        .deleted_entries_without_tombstone(limit)
        .await
        .map_err(|e| anyhow!("deleted_entries_without_tombstone: {e}"))?;

    if orphans.is_empty() {
        println!("\n  no deleted entries are missing a tombstone");
    } else if dry_run {
        println!(
            "\n  would enqueue {} delete op(s) for soft-deleted entries whose\n  \
             deletion never reached the outbox (limit {limit})",
            orphans.len()
        );
        for id in orphans.iter().take(5) {
            println!("    {id}");
        }
        if orphans.len() > 5 {
            println!("    … and {} more", orphans.len() - 5);
        }
    } else {
        let mut enqueued = 0usize;
        let mut unreadable = 0usize;
        for id in &orphans {
            // The payload must be the soft-deleted row itself: `push_one`
            // deserializes into a full `MemoryEntry`, and the remote's
            // resolver applies the deletion from that snapshot's
            // `deleted_at`. An id stub would be unpushable.
            let payload = match local.get_including_deleted(*id).await {
                Ok(e) => match serde_json::to_value(&e) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, entry = %id, "tombstone serialize failed");
                        unreadable += 1;
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, entry = %id, "tombstone row unreadable");
                    unreadable += 1;
                    continue;
                }
            };
            let res = local
                .enqueue_outbox(OutboxInput {
                    entry_id: *id,
                    op_kind: OutboxOp::Delete,
                    row_payload: payload,
                })
                .await;
            match res {
                Ok(_) => enqueued += 1,
                Err(e) => tracing::warn!(error = %e, entry = %id, "tombstone enqueue failed"),
            }
        }
        println!("\n  enqueued {enqueued} tombstone(s)");
        if unreadable > 0 {
            println!("  {unreadable} entr(ies) could not be read and were skipped");
        }
        if orphans.len() == limit {
            println!(
                "  hit the {limit}-row limit — re-run to continue \
                 (`--limit` raises it)"
            );
        }
    }

    // ── 3. delete rows queued with an unpushable payload ──────────
    // An earlier build of this command enqueued id-only stubs. They can
    // never deserialize, so they fail their way to the dead-letter queue
    // instead of propagating the deletion.
    let malformed = local
        .malformed_delete_payloads(limit)
        .await
        .map_err(|e| anyhow!("malformed_delete_payloads: {e}"))?;
    if malformed.is_empty() {
        println!("  no delete rows carry an unpushable payload");
    } else if dry_run {
        println!(
            "  would rewrite {} delete payload(s) that cannot deserialize",
            malformed.len()
        );
    } else {
        let mut fixed = 0usize;
        for (outbox_id, entry_id) in &malformed {
            let Ok(entry) = local.get_including_deleted(*entry_id).await else {
                continue;
            };
            let Ok(payload) = serde_json::to_value(&entry) else {
                continue;
            };
            if local
                .rewrite_outbox_payload(*outbox_id, &payload)
                .await
                .is_ok()
            {
                fixed += 1;
            }
        }
        println!("  rewrote {fixed} unpushable delete payload(s)");
    }

    if !dry_run {
        let after = local
            .outbox_health(cap)
            .await
            .map_err(|e| anyhow!("outbox_health: {e}"))?;
        println!();
        println!("  outbox after:");
        println!("    unconfirmed:   {}", after.total());
        println!("    claimable:     {}", after.pending);
        println!("    in flight:     {}", after.in_flight);
        println!("    dead-lettered: {}", after.dead_lettered);
        if after.dead_lettered > 0 {
            println!(
                "\n  {} row(s) remain dead-lettered and will not retry. They are\n  \
                 kept, not deleted; inspect their last_error before deciding.",
                after.dead_lettered
            );
        }
    }
    Ok(())
}
