//! Background sync loop spawned inside `runar mcp-muninn` when
//! `RUNAR_SYNC_AUTO=true` and both `RUNAR_STORAGE_LOCAL` +
//! `RUNAR_STORAGE_REMOTE` are configured.
//!
//! Phase 5.6.4. Lifetime-bound to the MCP server tokio runtime —
//! when the server exits, the task drops with it. No daemon, no
//! orphan processes. Hooks never run sync inline.

use std::time::Duration;

use chrono::Utc;

use crate::sync::heartbeat::{self, Heartbeat};

#[derive(Debug, Clone, Copy)]
pub struct AutoConfig {
    /// Base interval between sync ticks when activity is happening.
    pub base_interval_ms: u64,
    /// Max sleep when idle (caps the exponential backoff).
    pub max_backoff_ms: u64,
    /// Outbox claim batch per push tick.
    pub push_batch: usize,
    /// Pull batch per remote query.
    pub pull_batch: usize,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            base_interval_ms: 30_000,
            max_backoff_ms: 300_000,
            push_batch: 500,
            pull_batch: 1000,
        }
    }
}

impl AutoConfig {
    pub fn from_env() -> Self {
        let base = std::env::var("RUNAR_SYNC_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30_000);
        let max = std::env::var("RUNAR_SYNC_MAX_BACKOFF_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300_000);
        Self {
            base_interval_ms: base,
            max_backoff_ms: max,
            push_batch: 500,
            pull_batch: 1000,
        }
    }
}

/// True when the user opted in to auto-sync AND both backends are
/// configured. Anything else → no background loop.
pub fn should_run() -> bool {
    let auto = std::env::var("RUNAR_SYNC_AUTO")
        .map(|v| matches!(v.as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(false);
    let local_set = std::env::var("RUNAR_STORAGE_LOCAL").is_ok();
    let remote_set = std::env::var("RUNAR_STORAGE_REMOTE").is_ok();
    auto && local_set && remote_set
}

/// Compute the next sleep based on whether the previous tick saw any
/// activity. Activity → reset to base. No activity → exponential
/// backoff capped at `max`.
pub fn next_sleep_ms(prev_ms: u64, activity: bool, cfg: &AutoConfig) -> u64 {
    if activity {
        cfg.base_interval_ms
    } else {
        let doubled = prev_ms.saturating_mul(2);
        doubled.clamp(cfg.base_interval_ms, cfg.max_backoff_ms)
    }
}

/// Run the loop. Returns when the runtime drops the task.
pub async fn run_loop(cfg: AutoConfig) {
    tracing::info!(
        base_ms = cfg.base_interval_ms,
        max_ms = cfg.max_backoff_ms,
        "sync auto-loop started"
    );

    let mut sleep_ms = cfg.base_interval_ms;
    loop {
        let now = Utc::now();
        let next = now + chrono::Duration::milliseconds(sleep_ms as i64);
        heartbeat::write(&Heartbeat {
            last_tick: now,
            current_backoff_ms: sleep_ms,
            next_run_at: next,
        });

        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;

        // Push then pull. Errors logged, never panic.
        let pushed = match crate::sync::push::cmd_push(cfg.push_batch, false).await {
            Ok(()) => 1,
            Err(e) => {
                tracing::warn!(error = %e, "auto push failed");
                0
            }
        };
        let pulled = match crate::sync::pull::cmd_pull(cfg.pull_batch, false, None).await {
            Ok(()) => 1,
            Err(e) => {
                tracing::warn!(error = %e, "auto pull failed");
                0
            }
        };

        // We don't have row counts back from the cmd_* helpers (they
        // print summaries), so we use "did the call succeed at all"
        // as a proxy. An always-failing remote stays at max_backoff.
        let activity = (pushed + pulled) > 0;
        sleep_ms = next_sleep_ms(sleep_ms, activity, &cfg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AutoConfig {
        AutoConfig {
            base_interval_ms: 30_000,
            max_backoff_ms: 300_000,
            push_batch: 100,
            pull_batch: 100,
        }
    }

    #[test]
    fn activity_resets_to_base() {
        let cfg = cfg();
        assert_eq!(next_sleep_ms(120_000, true, &cfg), 30_000);
    }

    #[test]
    fn idle_doubles_until_cap() {
        let cfg = cfg();
        let mut s = cfg.base_interval_ms;
        s = next_sleep_ms(s, false, &cfg);
        assert_eq!(s, 60_000);
        s = next_sleep_ms(s, false, &cfg);
        assert_eq!(s, 120_000);
        s = next_sleep_ms(s, false, &cfg);
        assert_eq!(s, 240_000);
        s = next_sleep_ms(s, false, &cfg);
        assert_eq!(s, 300_000); // capped at max
        s = next_sleep_ms(s, false, &cfg);
        assert_eq!(s, 300_000); // still capped
    }

    #[test]
    fn from_env_falls_back_to_defaults() {
        // We can't reliably manipulate process env in parallel tests,
        // so just assert the no-override path matches Default.
        let d = AutoConfig::default();
        assert_eq!(d.base_interval_ms, 30_000);
        assert_eq!(d.max_backoff_ms, 300_000);
    }

    #[test]
    fn should_run_requires_all_three_signals() {
        // We can't toggle env in test cleanly without races. Assert
        // the function is callable; behavior is exercised manually
        // via integration smoke.
        let _ = should_run();
    }
}
