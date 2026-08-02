//! Diagnostic: is the sync outbox draining, or is it wedged?
//!
//! `runar sync status` prints a single "outbox depth", which reads as a
//! healthy backlog whether the rows are waiting to be claimed or can
//! never be claimed again. This splits the depth into its three real
//! states and, with `--reap`, shows what a push would release.
//!
//!     cargo run --example outbox -- <path-to-memory.db> [--reap] [--cap N]
//!
//! Read-only unless `--reap` is passed. Point it at a *copy* first.

use runar_muninn::storage::sqlite::SqliteAdapter;
use runar_muninn::storage::MemoryStorage;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = match args.get(1) {
        Some(p) if !p.starts_with("--") => p.clone(),
        _ => {
            eprintln!("usage: outbox <path-to-memory.db> [--reap] [--cap N]");
            std::process::exit(2);
        }
    };
    let reap = args.iter().any(|a| a == "--reap");
    let cap: i32 = args
        .windows(2)
        .find(|w| w[0] == "--cap")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(10);

    let store = SqliteAdapter::new(&path, "default")?;
    store.initialize().await?;

    let before = store.outbox_health(cap).await?;
    println!("outbox at {path} (attempt cap {cap})");
    report(&before);

    if before.is_wedged() {
        println!(
            "\n  WEDGED: depth is {} but nothing is claimable, so `sync push`\n  \
             reports \"nothing to push\" while the backlog never drains.",
            before.total()
        );
    }

    if reap {
        // 0s cutoff: release every claim, not just the old ones. Only safe
        // when no pusher is running.
        let released = store.reap_stale_claims(0).await?;
        println!("\nreaped {released} stale claim(s)");
        report(&store.outbox_health(cap).await?);
    } else if before.in_flight > 0 {
        println!("\n  re-run with --reap to release the in-flight claims");
    }
    Ok(())
}

fn report(h: &runar_muninn::types::OutboxHealth) {
    println!("  unconfirmed:    {}", h.total());
    println!("    claimable:    {}", h.pending);
    println!("    in flight:    {}", h.in_flight);
    println!("    dead-letter:  {}", h.dead_lettered);
    println!("  max attempts:   {}", h.max_attempts_seen);
    if let Some(t) = h.oldest_unconfirmed {
        println!("  oldest:         {}", t.format("%Y-%m-%d %H:%M:%S"));
    }
}
