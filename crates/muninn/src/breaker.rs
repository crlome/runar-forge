//! Circuit breaker for the summarizer API call.
//!
//! State persists to `~/.runar-forge/breaker-<project>.json` so a trip
//! survives CLI invocation boundaries (each hook fires a new process).
//!
//! Policy:
//! - On success → reset state (closed).
//! - On failure → increment `consecutive_failures`; if ≥ TRIP_THRESHOLD,
//!   set `tripped_until = now + TRIP_DURATION`.
//! - `is_tripped()` returns true iff `tripped_until > now`.
//! - `with_retry()` attempts up to 3 times with 1s/2s/4s backoff between
//!   failures, then records the final failure and propagates the error.
//!
//! Wraps **only** the summarizer. DB enqueue never retries — dropping a
//! single PostToolUse observation is cheaper than blocking a hook.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const TRIP_THRESHOLD: u32 = 3;
const TRIP_DURATION_SECS: i64 = 60;
const RETRY_BACKOFFS_MS: &[u64] = &[1_000, 2_000, 4_000];

/// Per-project DB-connectivity breaker. Trips after a few consecutive
/// `create_librarian()` failures so subsequent hooks short-circuit instead
/// of paying the full `RUNAR_DB_CONNECT_TIMEOUT_MS` wait again. One trip
/// per minute, not six per turn.
const DB_TRIP_THRESHOLD: u32 = 2;
const DB_TRIP_DURATION_SECS: i64 = 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BreakerState {
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Unix-ms timestamp; state is tripped while `now_ms() < tripped_until`.
    #[serde(default)]
    pub tripped_until: i64,
    /// Last failure timestamp for observability.
    #[serde(default)]
    pub last_failure_ts: i64,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn state_path(project_id: &str) -> PathBuf {
    let mut p = crate::setup::runar_dir();
    // Sanitize project id so arbitrary slashes don't escape the state dir.
    let safe: String = project_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    p.push(format!("breaker-{safe}.json"));
    p
}

pub fn read_state(project_id: &str) -> BreakerState {
    let path = state_path(project_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_state(project_id: &str, state: &BreakerState) -> std::io::Result<()> {
    let path = state_path(project_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string(state).unwrap_or_default();
    std::fs::write(path, json)
}

pub fn is_tripped(project_id: &str) -> bool {
    read_state(project_id).tripped_until > now_ms()
}

pub fn record_success(project_id: &str) {
    // Only write if state needs clearing — avoid unnecessary IO.
    let state = read_state(project_id);
    if state.consecutive_failures == 0 && state.tripped_until == 0 {
        return;
    }
    let _ = write_state(project_id, &BreakerState::default());
}

pub fn record_failure(project_id: &str) {
    let mut state = read_state(project_id);
    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    state.last_failure_ts = now_ms();
    if state.consecutive_failures >= TRIP_THRESHOLD {
        state.tripped_until = now_ms() + TRIP_DURATION_SECS * 1000;
    }
    let _ = write_state(project_id, &state);
}

/// Run `op` with up to 3 attempts and 1s/2s/4s backoff. Records the final
/// outcome into breaker state. Returns immediately without running `op` if
/// the breaker is currently tripped.
pub async fn with_retry<F, Fut, T, E>(project_id: &str, mut op: F) -> Result<T, BreakerOutcome<E>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    if is_tripped(project_id) {
        return Err(BreakerOutcome::Tripped);
    }

    let mut last_err: Option<E> = None;
    for (attempt, backoff_ms) in RETRY_BACKOFFS_MS.iter().enumerate() {
        match op().await {
            Ok(val) => {
                record_success(project_id);
                return Ok(val);
            }
            Err(e) => {
                last_err = Some(e);
                // No sleep after the final attempt.
                if attempt + 1 < RETRY_BACKOFFS_MS.len() {
                    tokio::time::sleep(std::time::Duration::from_millis(*backoff_ms)).await;
                }
            }
        }
    }

    record_failure(project_id);
    Err(BreakerOutcome::Failed(last_err.unwrap()))
}

// ── DB-connectivity breaker ────────────────────────────────────────
//
// Same persistence model as the summarizer breaker, separate state file
// (`db-breaker-<project>.json`) so a tripped DB doesn't mask summarizer
// health and vice versa.

fn db_state_path(project_id: &str) -> PathBuf {
    let mut p = crate::setup::runar_dir();
    let safe: String = project_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    p.push(format!("db-breaker-{safe}.json"));
    p
}

fn db_read_state(project_id: &str) -> BreakerState {
    std::fs::read_to_string(db_state_path(project_id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn db_write_state(project_id: &str, state: &BreakerState) -> std::io::Result<()> {
    let path = db_state_path(project_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string(state).unwrap_or_default();
    std::fs::write(path, json)
}

pub fn is_db_tripped(project_id: &str) -> bool {
    db_read_state(project_id).tripped_until > now_ms()
}

pub fn db_record_success(project_id: &str) {
    let state = db_read_state(project_id);
    if state.consecutive_failures == 0 && state.tripped_until == 0 {
        return;
    }
    let _ = db_write_state(project_id, &BreakerState::default());
}

pub fn db_record_failure(project_id: &str) {
    let mut state = db_read_state(project_id);
    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    state.last_failure_ts = now_ms();
    if state.consecutive_failures >= DB_TRIP_THRESHOLD {
        state.tripped_until = now_ms() + DB_TRIP_DURATION_SECS * 1000;
    }
    let _ = db_write_state(project_id, &state);
}

/// Read-only snapshot for `runar doctor` and tests.
pub fn db_state(project_id: &str) -> BreakerState {
    db_read_state(project_id)
}

#[derive(Debug)]
pub enum BreakerOutcome<E> {
    Tripped,
    Failed(E),
}

impl<E: std::fmt::Display> std::fmt::Display for BreakerOutcome<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BreakerOutcome::Tripped => write!(f, "breaker tripped"),
            BreakerOutcome::Failed(e) => write!(f, "{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_project() -> String {
        format!("breaker-test-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn fresh_project_is_not_tripped() {
        assert!(!is_tripped(&unique_project()));
    }

    #[test]
    fn trip_after_threshold_failures() {
        let pid = unique_project();
        for _ in 0..TRIP_THRESHOLD {
            record_failure(&pid);
        }
        assert!(is_tripped(&pid));

        let state = read_state(&pid);
        assert_eq!(state.consecutive_failures, TRIP_THRESHOLD);
        assert!(state.tripped_until > now_ms());
    }

    #[test]
    fn success_resets_counter() {
        let pid = unique_project();
        record_failure(&pid);
        record_failure(&pid);
        record_success(&pid);

        let state = read_state(&pid);
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.tripped_until, 0);
        assert!(!is_tripped(&pid));
    }

    #[tokio::test]
    async fn retry_success_after_transient_failures() {
        let pid = unique_project();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

        let result: Result<i32, BreakerOutcome<&'static str>> = with_retry(&pid, || {
            let attempts = attempts.clone();
            async move {
                let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 2 {
                    Err("transient")
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert!(matches!(result, Ok(42)));
        assert!(!is_tripped(&pid));
    }

    #[tokio::test]
    async fn tripped_short_circuits_operation() {
        let pid = unique_project();
        for _ in 0..TRIP_THRESHOLD {
            record_failure(&pid);
        }
        assert!(is_tripped(&pid));

        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = called.clone();

        let result: Result<i32, BreakerOutcome<&'static str>> = with_retry(&pid, || {
            let called = called_clone.clone();
            async move {
                called.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok::<i32, &'static str>(1)
            }
        })
        .await;

        assert!(matches!(result, Err(BreakerOutcome::Tripped)));
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }
}
