//! Runtime guards shared by every Claude Code hook subcommand.
//!
//! Three responsibilities, all designed to make hook invocations cheap and
//! recoverable when something downstream is wrong:
//!
//! 1. **Kill-switch** — `RUNAR_DISABLE_HOOKS=1` env var or a
//!    `~/.runar-forge/.disable-hooks` touch file lets the user neuter every
//!    hook subcommand without editing `.claude/settings.json`. When a freeze
//!    starts during a session, the user can `touch` the file and Claude Code
//!    is responsive on the next interaction.
//!
//! 2. **Wall-clock budget** — `RUNAR_HOOK_BUDGET_MS` (default 800 ms) caps
//!    the total time any hook subcommand can spend before it self-aborts and
//!    exits zero. A dead PG, a stalled embedding download, or a runaway pool
//!    acquire can't freeze the editor for more than the budget.
//!
//! 3. **Hook error log** — a 1 MiB-rotated log at
//!    `~/.runar-forge/hook.log`. Replaces the historical `2>/dev/null` in
//!    generated hook commands so silent freezes leave a forensic trail. The
//!    log writer is best-effort: any IO error is swallowed so logging never
//!    becomes the cause of a hook failure.
//!
//! All three knobs live here so adding a new hook subcommand is a one-liner
//! at the top of its handler.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use crate::setup::runar_dir;

const DEFAULT_HOOK_BUDGET_MS: u64 = 800;
const HOOK_LOG_MAX_BYTES: u64 = 1_048_576; // 1 MiB

/// Touch-file path. `touch ~/.runar-forge/.disable-hooks` short-circuits
/// every hook subcommand. `rm` re-enables them.
pub fn disable_file_path() -> PathBuf {
    runar_dir().join(".disable-hooks")
}

/// Hook error log. Rotated at 1 MiB by `append_hook_log`.
pub fn hook_log_path() -> PathBuf {
    runar_dir().join("hook.log")
}

/// Returns true when hooks are disabled by env var or touch file. Safe to
/// call at the very top of every hook subcommand.
pub fn hooks_disabled() -> bool {
    if std::env::var("RUNAR_DISABLE_HOOKS")
        .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
        .unwrap_or(false)
    {
        return true;
    }
    disable_file_path().exists()
}

/// Wall-clock cap for a single hook invocation. Configurable via
/// `RUNAR_HOOK_BUDGET_MS`. Returns the resolved value so callers can wrap
/// their work in `tokio::time::timeout(hook_budget(), …)`.
pub fn hook_budget() -> Duration {
    let ms: u64 = std::env::var("RUNAR_HOOK_BUDGET_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_HOOK_BUDGET_MS);
    Duration::from_millis(ms)
}

/// Best-effort append to `hook.log`. Rotates when the file exceeds 1 MiB by
/// renaming to `hook.log.1` (single rotation; old `.1` is overwritten). Any
/// IO error is swallowed: logging must never become the reason a hook fails.
pub fn append_hook_log(scope: &str, message: &str) {
    let path = hook_log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > HOOK_LOG_MAX_BYTES {
            let rotated = path.with_extension("log.1");
            let _ = fs::rename(&path, rotated);
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let ts = chrono::Utc::now().to_rfc3339();
        let _ = writeln!(f, "{ts} [{scope}] {message}");
    }
}

/// Read the last `n` non-empty lines of the hook log for `runar doctor` to
/// surface. Returns an empty Vec on any error.
pub fn tail_hook_log(n: usize) -> Vec<String> {
    let Ok(content) = fs::read_to_string(hook_log_path()) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|s| s.to_string())
        .collect();
    if lines.len() > n {
        let drop = lines.len() - n;
        lines.drain(..drop);
    }
    lines
}

/// Count panic lines in the hook log, and return the most recent one.
///
/// A panicking hook is invisible from inside the process that panicked, and
/// `runar doctor` used to report "hook log: 350KB" as a passing check while
/// the log held 1,526 panics that had silently disabled context injection for
/// three projects. Anything that crashes a hook must be surfaced as a failure.
pub fn count_hook_panics() -> (usize, Option<String>) {
    let Ok(content) = fs::read_to_string(hook_log_path()) else {
        return (0, None);
    };
    let panics: Vec<&str> = content
        .lines()
        .filter(|l| l.contains("panicked at"))
        .collect();
    let last = panics.last().map(|s| s.trim().to_string());
    (panics.len(), last)
}

/// Serialize a Claude Code hook response that actually reaches the model.
///
/// The contract is `hookSpecificOutput.additionalContext` with the event name
/// echoed back. A bare top-level `{"additionalContext": …}` is well-formed
/// JSON, parses as structured hook output, matches no recognised field, and is
/// dropped without a warning — that single mistake meant every context packet,
/// nudge, and reminder Muninn emitted before v0.9.0 was discarded, while
/// telemetry happily counted the characters it had printed.
pub fn additional_context_response(hook_event: &str, body: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event,
            "additionalContext": body,
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{with_runar_home, HOME_LOCK};

    /// The regression test for the defect that made the whole memory system a
    /// no-op: assert on the shape the *consumer* reads, not on what we print.
    #[test]
    fn hook_response_nests_additional_context_under_hook_specific_output() {
        let raw = additional_context_response("SessionStart", "remembered thing");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");

        let hso = v
            .get("hookSpecificOutput")
            .expect("additionalContext must be nested under hookSpecificOutput");
        assert_eq!(hso["hookEventName"], "SessionStart");
        assert_eq!(hso["additionalContext"], "remembered thing");

        // The pre-v0.9.0 shape must never come back.
        assert!(
            v.get("additionalContext").is_none(),
            "top-level additionalContext is silently discarded by Claude Code"
        );
    }

    #[test]
    fn hook_response_round_trips_every_event_we_wire() {
        for event in ["SessionStart", "UserPromptSubmit", "PostToolUse"] {
            let raw = additional_context_response(event, "x");
            let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], event);
            assert_eq!(v["hookSpecificOutput"]["additionalContext"], "x");
        }
    }

    #[test]
    fn hook_response_escapes_content_safely() {
        // Packets carry code, quotes, newlines and non-ASCII routinely.
        let body = "fn main() {\n  println!(\"café \\ 配置\");\n}";
        let raw = additional_context_response("SessionStart", body);
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], body);
    }

    // Group every env-mutating test under a single combined mutex-locked
    // function so cargo's default parallel runner doesn't race them. Also
    // serializes against `with_runar_home` callers via the same lock. The
    // `RUNAR_HOME` override redirects `disable_file_path()` to a tempdir
    // so a stray `.disable-hooks` from another test can't flip the assert.
    #[test]
    fn env_var_behaviors_serial() {
        let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("RUNAR_HOME").ok();
        std::env::set_var("RUNAR_HOME", tmp.path());

        std::env::remove_var("RUNAR_HOOK_BUDGET_MS");
        assert_eq!(hook_budget(), Duration::from_millis(800));

        std::env::set_var("RUNAR_HOOK_BUDGET_MS", "150");
        assert_eq!(hook_budget(), Duration::from_millis(150));
        std::env::remove_var("RUNAR_HOOK_BUDGET_MS");

        std::env::set_var("RUNAR_DISABLE_HOOKS", "1");
        assert!(hooks_disabled());
        std::env::set_var("RUNAR_DISABLE_HOOKS", "0");
        assert!(!hooks_disabled());
        std::env::remove_var("RUNAR_DISABLE_HOOKS");

        match prev_home {
            Some(v) => std::env::set_var("RUNAR_HOME", v),
            None => std::env::remove_var("RUNAR_HOME"),
        }
    }

    #[test]
    fn disabled_via_touch_file() {
        with_runar_home(|| {
            std::env::remove_var("RUNAR_DISABLE_HOOKS");
            assert!(!hooks_disabled());
            fs::create_dir_all(runar_dir()).unwrap();
            fs::write(disable_file_path(), "").unwrap();
            assert!(hooks_disabled());
        });
    }

    #[test]
    fn log_appends_and_rotates() {
        with_runar_home(|| {
            fs::create_dir_all(runar_dir()).unwrap();
            append_hook_log("test", "first");
            append_hook_log("test", "second");
            let tail = tail_hook_log(10);
            assert_eq!(tail.len(), 2);
            assert!(tail[0].contains("first"));
            assert!(tail[1].contains("second"));

            // Simulate oversize file → rotation on next append.
            let big = "x".repeat((HOOK_LOG_MAX_BYTES + 1) as usize);
            fs::write(hook_log_path(), big).unwrap();
            append_hook_log("test", "after-rotate");
            assert!(hook_log_path().with_extension("log.1").exists());
            let tail = tail_hook_log(5);
            assert_eq!(tail.len(), 1);
            assert!(tail[0].contains("after-rotate"));
        });
    }
}
