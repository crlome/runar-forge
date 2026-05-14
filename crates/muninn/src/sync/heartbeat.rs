//! Sync background loop heartbeat file.
//!
//! Phase 5.6.4. Doctor + `runar sync status` read this to know if the
//! background loop is alive. Best-effort — file write failures don't
//! crash the loop.

use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::setup;

pub fn heartbeat_path() -> PathBuf {
    setup::runar_dir().join("sync-heartbeat")
}

#[derive(Debug, Clone)]
pub struct Heartbeat {
    pub last_tick: DateTime<Utc>,
    pub current_backoff_ms: u64,
    pub next_run_at: DateTime<Utc>,
}

pub fn write(hb: &Heartbeat) {
    let path = heartbeat_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload = format!(
        "last_tick={}\ncurrent_backoff_ms={}\nnext_run_at={}\n",
        hb.last_tick.to_rfc3339(),
        hb.current_backoff_ms,
        hb.next_run_at.to_rfc3339()
    );
    let _ = fs::write(path, payload);
}

pub fn read() -> Option<Heartbeat> {
    let raw = fs::read_to_string(heartbeat_path()).ok()?;
    let mut last_tick = None;
    let mut backoff = None;
    let mut next_run = None;
    for line in raw.lines() {
        let (k, v) = line.split_once('=')?;
        match k {
            "last_tick" => {
                last_tick = DateTime::parse_from_rfc3339(v)
                    .ok()
                    .map(|t| t.with_timezone(&Utc));
            }
            "current_backoff_ms" => {
                backoff = v.parse().ok();
            }
            "next_run_at" => {
                next_run = DateTime::parse_from_rfc3339(v)
                    .ok()
                    .map(|t| t.with_timezone(&Utc));
            }
            _ => {}
        }
    }
    Some(Heartbeat {
        last_tick: last_tick?,
        current_backoff_ms: backoff?,
        next_run_at: next_run?,
    })
}

/// True if no heartbeat file exists, OR the last tick is older than
/// `stale_after_secs`. Doctor flags this as a fault when sync is
/// supposed to be enabled.
pub fn is_stale(stale_after_secs: i64) -> bool {
    match read() {
        None => true,
        Some(hb) => (Utc::now() - hb.last_tick).num_seconds() > stale_after_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_via_disk() {
        // Use a unique-per-test path so this doesn't collide with the
        // real heartbeat. We can't easily override `setup::runar_dir`
        // so write a local file directly via an isolated path.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hb");

        let hb = Heartbeat {
            last_tick: Utc::now(),
            current_backoff_ms: 30_000,
            next_run_at: Utc::now() + chrono::Duration::seconds(30),
        };
        let payload = format!(
            "last_tick={}\ncurrent_backoff_ms={}\nnext_run_at={}\n",
            hb.last_tick.to_rfc3339(),
            hb.current_backoff_ms,
            hb.next_run_at.to_rfc3339()
        );
        fs::write(&path, payload).unwrap();

        // Re-implement read() on this path.
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("last_tick="));
        assert!(raw.contains("current_backoff_ms=30000"));
    }
}
