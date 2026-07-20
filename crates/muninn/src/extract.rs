//! Passive tool-output learning — Phase 4.8 item 4.8.14.
//!
//! Reads PostToolUse hook stdin (tool_name, tool_input, tool_response),
//! runs lightweight heuristic extraction rules, and produces
//! `ExtractedInsight`s that the CLI handler saves as memory entries.
//!
//! Design priorities:
//! - **Fast**: rules are string-ops only (< 50 ms), no LLM calls.
//! - **Low noise**: each rule carries a confidence score; threshold at 0.7.
//! - **Dedup**: per-session state file prevents repeated saves of the same
//!   topic_key. Max 20 auto-saves per session.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::types::EntryType;

// ── Public types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct HookPayload {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    #[serde(default)]
    pub tool_response: serde_json::Value,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExtractedInsight {
    pub title: String,
    pub content: String,
    pub entry_type: EntryType,
    pub tags: Vec<String>,
    pub topic_key: Option<String>,
    pub confidence: f64,
}

const CONFIDENCE_THRESHOLD: f64 = 0.70;
pub const MAX_AUTO_SAVES_PER_SESSION: u32 = 20;
pub const MIN_SAVE_INTERVAL_MS: i64 = 30_000;

// ── Dispatch ──────────────────────────────────────────────────────

pub fn extract_insights(payload: &HookPayload) -> Vec<ExtractedInsight> {
    if payload.tool_name.starts_with("mcp__muninn__") {
        return Vec::new();
    }

    let mut out = match payload.tool_name.as_str() {
        "Edit" | "MultiEdit" => extract_from_edit(payload),
        "Write" | "Create" => extract_from_write(payload),
        "Bash" => extract_from_bash(payload),
        _ => Vec::new(),
    };

    out.retain(|i| i.confidence >= CONFIDENCE_THRESHOLD);
    out
}

// ── Edit extraction ───────────────────────────────────────────────

fn extract_from_edit(payload: &HookPayload) -> Vec<ExtractedInsight> {
    let file_path = payload
        .tool_input
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let old_str = payload
        .tool_input
        .get("old_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new_str = payload
        .tool_input
        .get("new_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if old_str.is_empty() || new_str.is_empty() || file_path.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let file_stem = Path::new(file_path)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let hash6 = short_hash(&format!("{old_str}{new_str}"));

    // Rule: null/none check added
    if !has_null_check(old_str) && has_null_check(new_str) {
        out.push(ExtractedInsight {
            title: format!("Bug fix: null check added in {file_stem}"),
            content: format!(
                "Null/none check added in `{file_path}`.\n\n**Before:**\n```\n{}\n```\n\n**After:**\n```\n{}\n```",
                truncate(old_str, 300),
                truncate(new_str, 300)
            ),
            entry_type: EntryType::Bug,
            tags: vec!["auto-extract".into(), "bugfix".into(), file_path.into()],
            topic_key: Some(format!("bugfix:{file_stem}:{hash6}")),
            confidence: 0.80,
        });
    }

    // Rule: error handling added
    if !has_error_handling(old_str) && has_error_handling(new_str) {
        out.push(ExtractedInsight {
            title: format!("Bug fix: error handling added in {file_stem}"),
            content: format!(
                "Error handling added in `{file_path}`.\n\n**Before:**\n```\n{}\n```\n\n**After:**\n```\n{}\n```",
                truncate(old_str, 300),
                truncate(new_str, 300)
            ),
            entry_type: EntryType::Bug,
            tags: vec!["auto-extract".into(), "bugfix".into(), file_path.into()],
            topic_key: Some(format!("bugfix:{file_stem}:{hash6}")),
            confidence: 0.75,
        });
    }

    // Rule: off-by-one fix
    if is_off_by_one_fix(old_str, new_str) {
        out.push(ExtractedInsight {
            title: format!("Bug fix: off-by-one in {file_stem}"),
            content: format!(
                "Boundary condition fix in `{file_path}`.\n\n**Before:**\n```\n{}\n```\n\n**After:**\n```\n{}\n```",
                truncate(old_str, 300),
                truncate(new_str, 300)
            ),
            entry_type: EntryType::Bug,
            tags: vec!["auto-extract".into(), "bugfix".into(), "off-by-one".into(), file_path.into()],
            topic_key: Some(format!("bugfix:{file_stem}:{hash6}")),
            confidence: 0.85,
        });
    }

    // Rule: config value change
    if is_config_file(file_path) {
        out.push(ExtractedInsight {
            title: format!("Config change: {file_stem}"),
            content: format!(
                "Configuration updated in `{file_path}`.\n\n**Changed:**\n```\n{}\n```\n→\n```\n{}\n```",
                truncate(old_str, 200),
                truncate(new_str, 200)
            ),
            entry_type: EntryType::Decision,
            tags: vec!["auto-extract".into(), "config".into(), file_path.into()],
            topic_key: Some(format!("config:{file_stem}")),
            confidence: 0.70,
        });
    }

    // Rule: env var addition
    if !has_env_var_read(old_str) && has_env_var_read(new_str) {
        let var_hint = extract_env_var_name(new_str).unwrap_or_else(|| "unknown".into());
        out.push(ExtractedInsight {
            title: format!("New env var: {var_hint} in {file_stem}"),
            content: format!(
                "Environment variable `{var_hint}` introduced in `{file_path}`.\n\n```\n{}\n```",
                truncate(new_str, 300)
            ),
            entry_type: EntryType::Decision,
            tags: vec!["auto-extract".into(), "env-var".into(), file_path.into()],
            topic_key: Some(format!("env:{var_hint}")),
            confidence: 0.70,
        });
    }

    out
}

// ── Write extraction ──────────────────────────────────────────────

fn extract_from_write(payload: &HookPayload) -> Vec<ExtractedInsight> {
    let file_path = payload
        .tool_input
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if file_path.is_empty() {
        return Vec::new();
    }

    let lower = file_path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);

    // Dockerfile
    if name.starts_with("dockerfile") || name == "containerfile" {
        return vec![arch_file_insight(file_path, "Dockerfile created", 0.90)];
    }

    // CI config
    if lower.contains(".github/workflows/") || name == ".gitlab-ci.yml" || name == ".gitlab-ci.yaml"
    {
        return vec![arch_file_insight(
            file_path,
            "CI configuration created",
            0.85,
        )];
    }

    // Migration
    if (lower.contains("migration") || lower.contains("migrate"))
        && (lower.ends_with(".sql") || lower.ends_with(".rs") || lower.ends_with(".py"))
    {
        return vec![arch_file_insight(
            file_path,
            "Database migration created",
            0.80,
        )];
    }

    // Config file
    if is_config_file(file_path) {
        let stem = Path::new(file_path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        return vec![ExtractedInsight {
            title: format!("New config: {stem}"),
            content: format!("Configuration file created at `{file_path}`."),
            entry_type: EntryType::Decision,
            tags: vec!["auto-extract".into(), "config".into(), file_path.into()],
            topic_key: Some(format!("newfile:{file_path}")),
            confidence: 0.70,
        }];
    }

    Vec::new()
}

fn arch_file_insight(file_path: &str, label: &str, confidence: f64) -> ExtractedInsight {
    ExtractedInsight {
        title: format!("{label}: {file_path}"),
        content: format!("{label} at `{file_path}`."),
        entry_type: EntryType::Architecture,
        tags: vec![
            "auto-extract".into(),
            "architecture".into(),
            file_path.into(),
        ],
        topic_key: Some(format!("newfile:{file_path}")),
        confidence,
    }
}

// ── Bash extraction (failures only) ───────────────────────────────

fn extract_from_bash(payload: &HookPayload) -> Vec<ExtractedInsight> {
    let exit_code = payload
        .tool_response
        .get("exitCode")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if exit_code == 0 {
        return Vec::new();
    }

    let command = payload
        .tool_input
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stderr = payload
        .tool_response
        .get("stderr")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stdout = payload
        .tool_response
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if command.is_empty() {
        return Vec::new();
    }

    let cmd_lower = command.to_lowercase();
    let output = if stderr.is_empty() { stdout } else { stderr };
    let hash6 = short_hash(output);

    // Test failure
    if is_test_command(&cmd_lower) {
        let test_name = extract_failing_test(output).unwrap_or_else(|| "unknown".into());
        return vec![ExtractedInsight {
            title: format!("Test failure: {}", truncate(&test_name, 60)),
            content: format!(
                "Test command failed: `{}`\n\n**Failing test:** {test_name}\n\n**Output (truncated):**\n```\n{}\n```",
                truncate(command, 100),
                truncate(output, 500)
            ),
            entry_type: EntryType::Bug,
            tags: vec!["auto-extract".into(), "test-failure".into()],
            topic_key: Some(format!("testfail:{hash6}")),
            confidence: 0.75,
        }];
    }

    // Build failure
    if is_build_command(&cmd_lower) {
        return vec![ExtractedInsight {
            title: format!("Build error: {}", truncate(command, 60)),
            content: format!(
                "Build failed: `{}`\n\n**Error (truncated):**\n```\n{}\n```",
                truncate(command, 100),
                truncate(output, 500)
            ),
            entry_type: EntryType::AutoChange,
            tags: vec!["auto-extract".into(), "build-error".into()],
            topic_key: Some(format!("builderr:{hash6}")),
            confidence: 0.70,
        }];
    }

    // Lint violations
    if is_lint_command(&cmd_lower) {
        return vec![ExtractedInsight {
            title: format!("Lint violations: {}", truncate(command, 60)),
            content: format!(
                "Linter found issues: `{}`\n\n**Output (truncated):**\n```\n{}\n```",
                truncate(command, 100),
                truncate(output, 500)
            ),
            entry_type: EntryType::TechDebt,
            tags: vec!["auto-extract".into(), "lint".into()],
            topic_key: Some(format!("lint:{hash6}")),
            confidence: 0.70,
        }];
    }

    Vec::new()
}

// ── Dedup / rate-limiting state ───────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractState {
    #[serde(default)]
    pub topic_keys_saved: Vec<String>,
    #[serde(default)]
    pub save_count: u32,
    #[serde(default)]
    pub last_save_ts: Option<i64>,
    /// Claude Code session this state belongs to. The state file lives in
    /// temp_dir and never rotates on its own — without this marker the
    /// MAX_AUTO_SAVES_PER_SESSION cap was effectively a lifetime cap.
    #[serde(default)]
    pub session_id: Option<String>,
}

pub fn extract_state_path(project_id: &str) -> std::path::PathBuf {
    let tmp = std::env::temp_dir();
    tmp.join(format!("runar-extract-{project_id}"))
}

pub fn read_extract_state(project_id: &str) -> ExtractState {
    let path = extract_state_path(project_id);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_extract_state(project_id: &str, state: &ExtractState) -> std::io::Result<()> {
    let path = extract_state_path(project_id);
    let json = serde_json::to_string(state).unwrap_or_default();
    std::fs::write(path, json)
}

pub fn dedup_insights(
    insights: Vec<ExtractedInsight>,
    state: &ExtractState,
    now_ms: i64,
) -> Vec<ExtractedInsight> {
    if state.save_count >= MAX_AUTO_SAVES_PER_SESSION {
        return Vec::new();
    }
    if let Some(last) = state.last_save_ts {
        if now_ms - last < MIN_SAVE_INTERVAL_MS {
            return Vec::new();
        }
    }
    let remaining = (MAX_AUTO_SAVES_PER_SESSION - state.save_count) as usize;
    insights
        .into_iter()
        .filter(|i| {
            i.topic_key
                .as_ref()
                .map(|tk| !state.topic_keys_saved.contains(tk))
                .unwrap_or(true)
        })
        .take(remaining)
        .collect()
}

// ── Helpers ───────────────────────────────────────────────────────

fn has_null_check(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.contains("!= null")
        || lower.contains("!== null")
        || lower.contains("!= nil")
        || lower.contains("is_some()")
        || lower.contains("is_none()")
        || lower.contains("if let some")
        || lower.contains("!= none")
        || lower.contains("is not none")
}

fn has_error_handling(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.contains("try {")
        || lower.contains("try{")
        || lower.contains("catch(")
        || lower.contains("catch (")
        || lower.contains(".unwrap_or")
        || lower.contains("unwrap_or_else")
        || lower.contains("match err")
        || lower.contains("=> err(")
        || (s.contains('?') && (lower.contains(".await") || lower.contains("fn ")))
}

fn is_off_by_one_fix(old: &str, new: &str) -> bool {
    let boundary_ctx = |s: &str| {
        let l = s.to_lowercase();
        l.contains("len") || l.contains("length") || l.contains("count") || l.contains("size")
    };
    if !boundary_ctx(old) && !boundary_ctx(new) {
        return false;
    }
    // Check if < became <= or vice versa, or +1/-1 added/removed
    (old.contains("< ") && new.contains("<= "))
        || (old.contains("<= ") && new.contains("< "))
        || (old.contains("> ") && new.contains(">= "))
        || (old.contains(">= ") && new.contains("> "))
        || (!old.contains("+ 1") && new.contains("+ 1"))
        || (!old.contains("- 1") && new.contains("- 1"))
        || (old.contains("+ 1") && !new.contains("+ 1"))
        || (old.contains("- 1") && !new.contains("- 1"))
}

fn is_config_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if name.contains("lock") || name == "package-lock.json" || name == "pnpm-lock.yaml" {
        return false;
    }
    name.ends_with(".toml")
        || name.ends_with(".yaml")
        || name.ends_with(".yml")
        || name.contains(".config.")
        || name == ".env.example"
        || name == "tsconfig.json"
        || name == "tailwind.config.js"
        || name == "vite.config.ts"
        || name == "next.config.js"
        || name == "next.config.mjs"
}

fn has_env_var_read(s: &str) -> bool {
    s.contains("env::var")
        || s.contains("process.env")
        || s.contains("os.environ")
        || s.contains("os.getenv")
        || s.contains("std::env::var")
}

fn extract_env_var_name(s: &str) -> Option<String> {
    // Try to pull the variable name from common patterns
    for pattern in [
        "env::var(\"",
        "env::var('",
        "process.env.",
        "process.env[\"",
        "os.environ[\"",
        "os.getenv(\"",
    ] {
        if let Some(idx) = s.find(pattern) {
            let rest = &s[idx + pattern.len()..];
            let end = rest.find(|c: char| !c.is_alphanumeric() && c != '_');
            let name = &rest[..end.unwrap_or(rest.len())];
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn is_test_command(cmd: &str) -> bool {
    cmd.contains("cargo test")
        || cmd.contains("npm test")
        || cmd.contains("npx jest")
        || cmd.contains("npx vitest")
        || cmd.contains("pytest")
        || cmd.contains("go test")
        || cmd.contains("mix test")
}

fn is_build_command(cmd: &str) -> bool {
    cmd.contains("cargo build")
        || cmd.contains("cargo check")
        || cmd.contains("npm run build")
        || cmd.contains("npx tsc")
        || cmd.contains("go build")
        || cmd.contains("make ")
}

fn is_lint_command(cmd: &str) -> bool {
    cmd.contains("clippy")
        || cmd.contains("eslint")
        || cmd.contains("ruff")
        || cmd.contains("pylint")
        || cmd.contains("golangci-lint")
        || cmd.contains("rustfmt")
}

fn extract_failing_test(output: &str) -> Option<String> {
    // Look for common test failure patterns
    for line in output.lines().rev() {
        let trimmed = line.trim();
        // Rust: "test module::test_name ... FAILED"
        if trimmed.ends_with("FAILED") && trimmed.starts_with("test ") {
            return Some(
                trimmed
                    .trim_start_matches("test ")
                    .trim_end_matches(" ... FAILED")
                    .to_string(),
            );
        }
        // Jest/Vitest: "FAIL src/foo.test.ts"
        if trimmed.starts_with("FAIL ") {
            return Some(trimmed.trim_start_matches("FAIL ").to_string());
        }
        // pytest: "FAILED test_foo.py::test_bar"
        if trimmed.starts_with("FAILED ") {
            return Some(trimmed.trim_start_matches("FAILED ").to_string());
        }
    }
    None
}

/// Public wrapper so hook handlers outside this module can reuse the same
/// stable hash function for queue dedup (enqueue path).
pub fn short_hash_public(s: &str) -> String {
    short_hash(s)
}

fn short_hash(s: &str) -> String {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:06x}", hasher.finish() & 0xFFFFFF)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn edit_payload(file: &str, old: &str, new: &str) -> HookPayload {
        HookPayload {
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({
                "file_path": file,
                "old_string": old,
                "new_string": new,
            }),
            tool_response: serde_json::json!({"success": true}),
            session_id: None,
            cwd: None,
        }
    }

    fn write_payload(file: &str) -> HookPayload {
        HookPayload {
            tool_name: "Write".into(),
            tool_input: serde_json::json!({
                "file_path": file,
                "content": "some content",
            }),
            tool_response: serde_json::json!({"success": true}),
            session_id: None,
            cwd: None,
        }
    }

    fn bash_payload(cmd: &str, exit: i64, stderr: &str) -> HookPayload {
        HookPayload {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": cmd}),
            tool_response: serde_json::json!({
                "exitCode": exit,
                "stdout": "",
                "stderr": stderr,
            }),
            session_id: None,
            cwd: None,
        }
    }

    #[test]
    fn edit_null_check_detected() {
        let p = edit_payload(
            "src/auth.rs",
            "fn check(token: Token) { token.validate() }",
            "fn check(token: Token) { if let Some(t) = token.as_ref() { t.validate() } }",
        );
        let insights = extract_insights(&p);
        assert!(insights
            .iter()
            .any(|i| i.entry_type == EntryType::Bug && i.title.contains("null check")));
    }

    #[test]
    fn edit_comment_only_no_false_positive() {
        let p = edit_payload(
            "src/main.rs",
            "// old comment\nfn main() {}",
            "// new comment\nfn main() {}",
        );
        let insights = extract_insights(&p);
        assert!(insights.is_empty());
    }

    #[test]
    fn edit_config_change_detected() {
        let p = edit_payload("config/database.toml", "pool_size = 5", "pool_size = 20");
        let insights = extract_insights(&p);
        assert!(insights
            .iter()
            .any(|i| i.entry_type == EntryType::Decision && i.title.contains("Config")));
    }

    #[test]
    fn edit_env_var_detected() {
        let p = edit_payload(
            "src/config.rs",
            "let port = 3000;",
            "let port = std::env::var(\"PORT\").unwrap_or(\"3000\".into());",
        );
        let insights = extract_insights(&p);
        assert!(insights
            .iter()
            .any(|i| i.title.contains("PORT") && i.entry_type == EntryType::Decision));
    }

    #[test]
    fn edit_off_by_one_detected() {
        let p = edit_payload(
            "src/iter.rs",
            "for i in 0..items.len() - 1 {",
            "for i in 0..items.len() {",
        );
        let insights = extract_insights(&p);
        assert!(insights
            .iter()
            .any(|i| i.entry_type == EntryType::Bug && i.title.contains("off-by-one")));
    }

    #[test]
    fn write_dockerfile_detected() {
        let p = write_payload("Dockerfile");
        let insights = extract_insights(&p);
        assert!(insights
            .iter()
            .any(|i| i.entry_type == EntryType::Architecture));
    }

    #[test]
    fn write_ci_config_detected() {
        let p = write_payload(".github/workflows/ci.yml");
        let insights = extract_insights(&p);
        assert!(insights
            .iter()
            .any(|i| i.entry_type == EntryType::Architecture && i.title.contains("CI")));
    }

    #[test]
    fn write_regular_file_no_match() {
        let p = write_payload("src/utils/helpers.ts");
        let insights = extract_insights(&p);
        assert!(insights.is_empty());
    }

    #[test]
    fn bash_test_failure_detected() {
        let p = bash_payload(
            "cargo test",
            1,
            "test storage::tests::test_save ... FAILED\nfailures: 1",
        );
        let insights = extract_insights(&p);
        assert!(insights
            .iter()
            .any(|i| i.entry_type == EntryType::Bug && i.title.contains("Test failure")));
    }

    #[test]
    fn bash_success_ignored() {
        let p = bash_payload("cargo test", 0, "test result: ok. 50 passed");
        let insights = extract_insights(&p);
        assert!(insights.is_empty());
    }

    #[test]
    fn bash_build_error_detected() {
        let p = bash_payload("cargo build", 1, "error[E0308]: mismatched types");
        let insights = extract_insights(&p);
        assert!(insights
            .iter()
            .any(|i| i.entry_type == EntryType::AutoChange));
    }

    #[test]
    fn mcp_muninn_skipped() {
        let p = HookPayload {
            tool_name: "mcp__muninn__muninn_save".into(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
            session_id: None,
            cwd: None,
        };
        assert!(extract_insights(&p).is_empty());
    }

    #[test]
    fn dedup_prevents_duplicate_topic_key() {
        let insight = ExtractedInsight {
            title: "test".into(),
            content: "test".into(),
            entry_type: EntryType::Bug,
            tags: vec![],
            topic_key: Some("bugfix:auth:abc123".into()),
            confidence: 0.80,
        };
        let state = ExtractState {
            topic_keys_saved: vec!["bugfix:auth:abc123".into()],
            save_count: 1,
            last_save_ts: Some(0),
            session_id: None,
        };
        let filtered = dedup_insights(vec![insight], &state, 100_000);
        assert!(filtered.is_empty());
    }

    #[test]
    fn dedup_rate_limit_enforced() {
        let insight = ExtractedInsight {
            title: "test".into(),
            content: "test".into(),
            entry_type: EntryType::Bug,
            tags: vec![],
            topic_key: Some("new".into()),
            confidence: 0.80,
        };
        let state = ExtractState {
            topic_keys_saved: vec![],
            save_count: MAX_AUTO_SAVES_PER_SESSION,
            last_save_ts: Some(0),
            session_id: None,
        };
        let filtered = dedup_insights(vec![insight], &state, 100_000);
        assert!(filtered.is_empty());
    }

    #[test]
    fn dedup_min_interval_enforced() {
        let insight = ExtractedInsight {
            title: "test".into(),
            content: "test".into(),
            entry_type: EntryType::Bug,
            tags: vec![],
            topic_key: Some("new".into()),
            confidence: 0.80,
        };
        let now = 50_000i64;
        let state = ExtractState {
            topic_keys_saved: vec![],
            save_count: 0,
            last_save_ts: Some(now - 10_000), // 10s ago, below 30s min
            session_id: None,
        };
        let filtered = dedup_insights(vec![insight], &state, now);
        assert!(filtered.is_empty());
    }

    #[test]
    fn confidence_threshold_filters_low() {
        let p = edit_payload("src/x.rs", "a", "b");
        let insights = extract_insights(&p);
        assert!(insights
            .iter()
            .all(|i| i.confidence >= CONFIDENCE_THRESHOLD));
    }
}
