//! Setup / init subcommands — configure the host environment for a user.
//!
//! Ports the TS `runar setup claude-code` + `runar init` logic to Rust.
//! The single Rust binary exposes all three tool families (muninn, huginn,
//! curator) via the unified `mcp-muninn` command, so only ONE MCP server
//! is registered here (unlike the TS version which registers three).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{json, Value};

pub fn home_dir() -> PathBuf {
    // `dirs::home_dir()` reads `HOME` on Unix and `USERPROFILE` on Windows,
    // which makes this the single safe source of the user's home directory
    // across every supported platform.
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn runar_dir() -> PathBuf {
    home_dir().join(".runar-forge")
}

// ── init ───────────────────────────────────────────────────────────

pub fn write_env_file(storage: &str) -> anyhow::Result<PathBuf> {
    let dir = runar_dir();
    fs::create_dir_all(&dir)?;
    let env_path = dir.join(".env");

    if env_path.exists() {
        return Ok(env_path);
    }

    let content = match storage {
        // `127.0.0.1` (not `localhost`) sidesteps an IPv6-first resolver
        // trap on macOS where Docker Desktop forwards the published port
        // only on IPv4, so `localhost` → `::1` silently fails connect.
        // Port `5433` matches INSTALLATION-GUIDE.md §2.
        "postgresql" | "postgres" => "RUNAR_STORAGE=postgresql\n\
             RUNAR_DB_URL=postgresql://runar:runar_password@127.0.0.1:5433/runar_memory\n"
            .to_string(),
        _ => {
            let db_path = dir.join("memory.db");
            format!(
                "RUNAR_STORAGE=sqlite\nRUNAR_SQLITE_PATH={}\n",
                db_path.display()
            )
        }
    };

    fs::write(&env_path, content)?;
    Ok(env_path)
}

// ── setup claude-code ──────────────────────────────────────────────

pub struct ClaudeCodeSetup {
    pub project_id: String,
    pub claude_json_path: PathBuf,
    pub settings_path: PathBuf,
    pub claude_md_path: PathBuf,
    pub binary_path: String,
}

pub fn detect_project_id() -> String {
    // Try `git remote get-url origin`
    if let Ok(out) = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
    {
        if out.status.success() {
            let remote = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(id) = parse_project_from_remote(&remote) {
                return id;
            }
        }
    }
    // Fallback to current directory name
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".into())
}

fn parse_project_from_remote(remote: &str) -> Option<String> {
    // git@gitlab.com:org/name.git or https://.../name.git → "name"
    let last = remote.rsplit('/').next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

pub fn detect_binary_path() -> String {
    if let Ok(out) = Command::new("which").arg("runar").output() {
        if out.status.success() {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return p;
            }
        }
    }
    // Fall back to the currently-running binary
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "runar".into())
}

/// Stable, hook-friendly binary location: `~/.runar-forge/bin/runar`. Hooks
/// point at this path so `cargo install` rewriting `~/.cargo/bin/runar`
/// (non-atomic, may race with running hooks) cannot corrupt the active CC
/// session. `runar update` (and re-runs of `runar setup claude-code`)
/// refresh the file via temp + atomic rename.
pub fn stable_bin_path() -> PathBuf {
    runar_dir().join("bin").join("runar")
}

/// Copy `source_binary` to `~/.runar-forge/bin/runar` atomically. Previous
/// content is preserved at `runar.previous` so `runar update --rollback`
/// has something to swap back to. Best-effort: if the copy fails (perm
/// denied, source missing, etc.) the caller falls back to the source path.
pub fn install_stable_binary(source_binary: &str) -> std::io::Result<PathBuf> {
    let target = stable_bin_path();
    let bin_dir = target.parent().unwrap().to_path_buf();
    fs::create_dir_all(&bin_dir)?;

    // Skip work when the source already IS the stable target.
    if let Ok(canon_src) = fs::canonicalize(source_binary) {
        if canon_src == target {
            return Ok(target);
        }
    }

    // Roll the existing binary aside. `rename` is atomic on the same FS;
    // any previous `.previous` is overwritten — we keep at most one
    // generation back for rollback.
    if target.exists() {
        let previous = bin_dir.join("runar.previous");
        let _ = fs::rename(&target, &previous);
    }

    // Copy through a temp file in the same directory so the final rename
    // is atomic on POSIX. `persist` swaps the inode in one syscall, which
    // means a hook firing mid-install never sees a partial binary.
    let tmp = tempfile::NamedTempFile::new_in(&bin_dir)?;
    fs::copy(source_binary, tmp.path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(tmp.path())?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(tmp.path(), perms)?;
    }
    tmp.persist(&target)
        .map_err(|e| std::io::Error::other(format!("persist: {e}")))?;
    Ok(target)
}

/// Resolved binary path for hooks. Prefers the stable installed copy when
/// it exists; otherwise falls back to whatever `detect_binary_path()`
/// returns. Centralizes the policy so MCP server registration and hook
/// command-strings always agree on which binary to invoke.
pub fn resolve_hook_binary_path() -> String {
    let stable = stable_bin_path();
    if stable.exists() {
        return stable.to_string_lossy().into_owned();
    }
    detect_binary_path()
}

/// Shell-quote a path for embedding in a hook command string. Wraps in
/// single quotes and escapes any internal single quotes. Sufficient for
/// the path strings we generate (no embedded NULs).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Build the full hook command line. Redirects stderr to the rotated
/// `~/.runar-forge/hook.log` so silent freezes leave a forensic trail
/// (replaces the historical `2>/dev/null`). Trailing `; exit 0` makes
/// every hook return zero — Claude Code treats non-zero as a hard error
/// and a stale binary's "command not found" used to surface as one.
fn hook_command(binary_path: &str, args: &str, log_path: &str) -> String {
    format!(
        "{} {} 2>>{} ; exit 0",
        shell_quote(binary_path),
        args,
        shell_quote(log_path),
    )
}

pub fn setup_claude_code(
    project_id: &str,
    with_auto_capture: bool,
) -> anyhow::Result<ClaudeCodeSetup> {
    let home = home_dir();
    let detected = detect_binary_path();

    // Mirror the running binary into `~/.runar-forge/bin/runar` so hooks
    // and the MCP registration both point at a stable, atomically-updated
    // path. Failure is non-fatal — fall back to the detected path.
    let binary_path = match install_stable_binary(&detected) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!(
                "warn: could not install stable binary at {}: {e} — using {detected}",
                stable_bin_path().display()
            );
            detected
        }
    };

    // Step 1: ~/.claude.json — register the unified mcp-muninn server
    let claude_json_path = home.join(".claude.json");
    let mut claude_json: Value = if claude_json_path.exists() {
        serde_json::from_str(&fs::read_to_string(&claude_json_path)?).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    let obj = claude_json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("~/.claude.json is not a JSON object"))?;

    let mut servers = obj
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    // The Rust binary exposes all tool families via one server.
    servers.insert(
        "muninn".into(),
        json!({
            "command": binary_path.clone(),
            "args": ["mcp-muninn"],
        }),
    );
    // Remove the legacy separate huginn/curator servers if they exist from a
    // prior TS setup; they'd fail now since the Rust binary doesn't expose them.
    servers.remove("huginn");
    servers.remove("curator");

    obj.insert("mcpServers".into(), Value::Object(servers));
    fs::write(
        &claude_json_path,
        serde_json::to_string_pretty(&claude_json)? + "\n",
    )?;

    // Step 2: .claude/settings.json in CWD — hooks
    let cwd = std::env::current_dir()?;
    let claude_dir = cwd.join(".claude");
    fs::create_dir_all(&claude_dir)?;
    let settings_path = claude_dir.join("settings.json");
    let mut settings: Value = if settings_path.exists() {
        serde_json::from_str(&fs::read_to_string(&settings_path)?).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    let sobj = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json is not a JSON object"))?;

    let log_path = runar_dir().join("hook.log").to_string_lossy().into_owned();
    let pid_q = shell_quote(project_id);
    let mk = |args: String| hook_command(&binary_path, &args, &log_path);
    let context_cmd   = mk(format!("context      --silent --project {pid_q}"));
    let ping_cmd      = mk(format!("session ping --silent --project {pid_q}"));
    let save_ack_cmd  = mk(format!("save-ack     --silent --project {pid_q}"));
    let nudge_cmd     = mk(format!("nudge        --silent --project {pid_q}"));
    let extract_cmd   = mk(format!("extract      --silent --project {pid_q}"));
    // Auto-capture queue commands (Phase 6 — opt-in).
    let enqueue_cmd   = mk(format!("enqueue      --silent --project {pid_q}"));
    let summarize_cmd = mk(format!("summarize    --silent --project {pid_q}"));

    let existing_hooks = sobj.get("hooks").cloned().unwrap_or(json!({}));
    let pre_tool = filter_runar_hooks(
        existing_hooks
            .get("PreToolUse")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    );
    let post_tool = filter_runar_hooks(
        existing_hooks
            .get("PostToolUse")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    );
    let user_prompt = filter_runar_hooks(
        existing_hooks
            .get("UserPromptSubmit")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    );
    let session_end = filter_runar_hooks(
        existing_hooks
            .get("SessionEnd")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    );

    let mut pre_tool = pre_tool;
    pre_tool.push(hook_entry(".*", &context_cmd));

    let mut post_tool = post_tool;
    post_tool.push(hook_entry("Write|Edit|Create|MultiEdit", &ping_cmd));
    post_tool.push(hook_entry("mcp__muninn__muninn_save", &save_ack_cmd));
    post_tool.push(hook_entry("Write|Edit|Create|MultiEdit|Bash", &extract_cmd));
    if with_auto_capture {
        post_tool.push(hook_entry(
            "Write|Edit|Create|MultiEdit|Bash",
            &enqueue_cmd,
        ));
    }

    let mut user_prompt = user_prompt;
    user_prompt.push(hook_entry(".*", &nudge_cmd));

    let mut session_end = session_end;
    if with_auto_capture {
        session_end.push(hook_entry(".*", &summarize_cmd));
    }

    let mut hooks_obj = existing_hooks.as_object().cloned().unwrap_or_default();
    hooks_obj.insert("PreToolUse".into(), Value::Array(pre_tool));
    hooks_obj.insert("PostToolUse".into(), Value::Array(post_tool));
    hooks_obj.insert("UserPromptSubmit".into(), Value::Array(user_prompt));
    if with_auto_capture {
        hooks_obj.insert("SessionEnd".into(), Value::Array(session_end));
    } else if hooks_obj.get("SessionEnd").is_none() {
        // Don't introduce an empty array if the user never turned it on.
    }
    sobj.insert("hooks".into(), Value::Object(hooks_obj));

    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings)? + "\n",
    )?;

    // Step 3: Append Memory section to CLAUDE.md if missing
    let claude_md_path = cwd.join("CLAUDE.md");
    let md = if claude_md_path.exists() {
        fs::read_to_string(&claude_md_path).unwrap_or_default()
    } else {
        String::new()
    };
    if !md.contains("## Memory") {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&claude_md_path)?;
        f.write_all(MEMORY_SECTION.as_bytes())?;
    }

    Ok(ClaudeCodeSetup {
        project_id: project_id.to_string(),
        claude_json_path,
        settings_path,
        claude_md_path,
        binary_path,
    })
}

fn hook_entry(matcher: &str, command: &str) -> Value {
    json!({
        "matcher": matcher,
        "hooks": [{ "type": "command", "command": command }],
    })
}

/// Remove any existing hook entries whose command contains "runar" — we'll
/// re-add the fresh ones. This avoids duplicating hooks on re-run.
fn filter_runar_hooks(hooks: Vec<Value>) -> Vec<Value> {
    hooks
        .into_iter()
        .filter(|entry| {
            let inner = entry
                .get("hooks")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            !inner.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|s| s.contains("runar"))
                    .unwrap_or(false)
            })
        })
        .collect()
}

const MEMORY_SECTION: &str = "
## Memory (Muninn)
Memory protocol is injected dynamically via hooks.
Do not edit this section — it updates automatically via `runar setup claude-code`.
";

// ── setup cursor / windsurf (stdout-only config) ───────────────────

pub fn cursor_config(binary_path: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "mcpServers": {
            "muninn": {
                "command": binary_path,
                "args": ["mcp-muninn"],
            }
        }
    }))
    .unwrap_or_default()
}

pub fn windsurf_config(binary_path: &str) -> String {
    // Same shape as cursor for the MCP stdio server
    cursor_config(binary_path)
}

// ── Path helper for hook subcommands ───────────────────────────────

/// Per-project temp ping file, used by session/nudge/save-ack.
pub fn ping_file_path(project_id: &str) -> PathBuf {
    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(tmpdir).join(format!("runar-ping-{project_id}"))
}

/// Render a Claude Code hook additionalContext response (empty = no injection).
/// Real content comes from item 2 (`runar context`). For now, emitting empty
/// JSON so hooks execute cleanly without errors.
pub fn empty_hook_response() -> String {
    json!({ "additionalContext": "" }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_patterns() {
        assert_eq!(
            parse_project_from_remote("git@gitlab.com:org/runar-forge.git"),
            Some("runar-forge".into())
        );
        assert_eq!(
            parse_project_from_remote("https://github.com/foo/bar.git"),
            Some("bar".into())
        );
        assert_eq!(
            parse_project_from_remote("https://github.com/foo/bar"),
            Some("bar".into())
        );
    }

    #[test]
    fn hook_entry_shape() {
        let h = hook_entry(".*", "echo hi");
        assert_eq!(h["matcher"], ".*");
        assert_eq!(h["hooks"][0]["type"], "command");
        assert_eq!(h["hooks"][0]["command"], "echo hi");
    }

    #[test]
    fn filter_removes_runar_hooks_but_keeps_others() {
        let hooks = vec![
            json!({
                "matcher": ".*",
                "hooks": [{ "type": "command", "command": "runar context" }],
            }),
            json!({
                "matcher": ".*",
                "hooks": [{ "type": "command", "command": "other-tool" }],
            }),
        ];
        let filtered = filter_runar_hooks(hooks);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["hooks"][0]["command"], "other-tool");
    }

    #[test]
    fn write_env_file_postgres() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let path = write_env_file("postgresql").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("RUNAR_STORAGE=postgresql"));
        assert!(content.contains("RUNAR_DB_URL=postgresql://"));
    }

    #[test]
    fn write_env_file_sqlite() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let path = write_env_file("sqlite").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("RUNAR_STORAGE=sqlite"));
        assert!(content.contains("RUNAR_SQLITE_PATH="));
    }
}
