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
    // `RUNAR_HOME` lets users (and tests) redirect every runar-managed path
    // to a custom directory. Falls back to `dirs::home_dir()` (reads `HOME`
    // on Unix, `USERPROFILE` on Windows) so a single env var works on every
    // platform — `set_var("HOME", …)` alone is a no-op on Windows.
    if let Ok(p) = std::env::var("RUNAR_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
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

/// Stable, hook-friendly binary location: `~/.runar-forge/bin/runar`
/// (`runar.exe` on Windows). Hooks point at this path so `cargo install`
/// rewriting `~/.cargo/bin/runar` (non-atomic, may race with running hooks)
/// cannot corrupt the active CC session. `runar update` (and re-runs of
/// `runar setup claude-code`) refresh the file via temp + atomic rename.
///
/// The `.exe` suffix on Windows is required: the MCP registration and hooks
/// invoke this path via `CreateProcess`, which appends `.exe` and would fail
/// to find an extension-less file.
pub fn stable_bin_path() -> PathBuf {
    let name = if cfg!(windows) { "runar.exe" } else { "runar" };
    runar_dir().join("bin").join(name)
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

/// Mirror the running binary into `~/.runar-forge/bin/runar` and return that
/// stable path, so every editor's MCP registration points at an atomically
/// updated location (see `install_stable_binary`). Best-effort: on failure
/// (perm denied, source missing) it warns and falls back to the detected path.
pub fn resolve_stable_binary() -> String {
    let detected = detect_binary_path();
    match install_stable_binary(&detected) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!(
                "warn: could not install stable binary at {}: {e} — using {detected}",
                stable_bin_path().display()
            );
            detected
        }
    }
}

/// Read a JSON file into a `Value`, returning an empty object when the file is
/// absent or unparseable. Mirrors the read step the Claude Code setup uses for
/// `~/.claude.json` so every JSON-config editor merges the same way.
fn read_json_or_empty(path: &std::path::Path) -> anyhow::Result<Value> {
    if path.exists() {
        Ok(serde_json::from_str(&fs::read_to_string(path)?).unwrap_or_else(|_| json!({})))
    } else {
        Ok(json!({}))
    }
}

/// Write a `Value` as pretty JSON with a trailing newline.
fn write_json_pretty(path: &std::path::Path, value: &Value) -> anyhow::Result<()> {
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

/// Shell-quote a path for embedding in a hook command string. Wraps in
/// single quotes and escapes any internal single quotes. Sufficient for
/// the path strings we generate (no embedded NULs). Unix-only — Windows
/// hooks use exec form and never build a shell string.
#[cfg(not(windows))]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Build the full hook command line. Redirects stderr to the rotated
/// `~/.runar-forge/hook.log` so silent freezes leave a forensic trail
/// (replaces the historical `2>/dev/null`). Trailing `; exit 0` makes
/// every hook return zero — Claude Code treats non-zero as a hard error
/// and a stale binary's "command not found" used to surface as one.
#[cfg(not(windows))]
fn hook_command(binary_path: &str, args: &str, log_path: &str) -> String {
    format!(
        "{} {} 2>>{} ; exit 0",
        shell_quote(binary_path),
        args,
        shell_quote(log_path),
    )
}

/// Build a Claude Code hook entry from a raw argument vector, choosing the
/// representation that is robust on the host OS.
///
/// On **Unix** we emit *shell form*: a single command string Claude Code runs
/// via `sh -c`, with each arg single-quoted, stderr appended to `hook.log`,
/// and a trailing `; exit 0` so a stale binary can't surface as a hard error.
///
/// On **Windows** we emit *exec form* (`command` + `args`), which Claude Code
/// spawns directly with **no shell**. This is the only representation that
/// works whether Claude Code falls back to Git Bash *or* PowerShell — the
/// POSIX `'…'`/`;`/`2>>` syntax is invalid under PowerShell. We lose the shell
/// stderr redirect, but the hook subcommands already log to `hook.log`
/// internally (`hooks_runtime::append_hook_log`) and swallow their own errors,
/// so no safety is lost.
fn runar_hook_entry(matcher: &str, binary_path: &str, args: &[&str], log_path: &str) -> Value {
    #[cfg(not(windows))]
    {
        let quoted = args
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        hook_entry(matcher, &hook_command(binary_path, &quoted, log_path))
    }
    #[cfg(windows)]
    {
        let _ = log_path; // captured by the binary's own hook.log writer
        json!({
            "matcher": matcher,
            "hooks": [{
                "type": "command",
                "command": binary_path,
                "args": args,
            }],
        })
    }
}

pub fn setup_claude_code(
    project_id: &str,
    with_auto_capture: bool,
) -> anyhow::Result<ClaudeCodeSetup> {
    let home = home_dir();

    // Mirror the running binary into `~/.runar-forge/bin/runar` so hooks
    // and the MCP registration both point at a stable, atomically-updated
    // path. Failure is non-fatal — falls back to the detected path.
    let binary_path = resolve_stable_binary();

    // Step 1: ~/.claude.json — register the unified mcp-muninn server
    let claude_json_path = home.join(".claude.json");
    let mut claude_json: Value = read_json_or_empty(&claude_json_path)?;

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
    write_json_pretty(&claude_json_path, &claude_json)?;

    // Step 2: .claude/settings.json in CWD — hooks
    let cwd = std::env::current_dir()?;
    let claude_dir = cwd.join(".claude");
    fs::create_dir_all(&claude_dir)?;
    let settings_path = claude_dir.join("settings.json");
    let mut settings: Value = read_json_or_empty(&settings_path)?;

    let sobj = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json is not a JSON object"))?;

    let log_path = runar_dir().join("hook.log").to_string_lossy().into_owned();
    // Render each hook from a raw arg vector. Shell-form (quoted) on Unix,
    // exec-form (no shell) on Windows — see `runar_hook_entry`.
    let entry =
        |matcher: &str, args: &[&str]| runar_hook_entry(matcher, &binary_path, args, &log_path);

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
    pre_tool.push(entry(
        ".*",
        &["context", "--silent", "--project", project_id],
    ));

    let mut post_tool = post_tool;
    post_tool.push(entry(
        "Write|Edit|Create|MultiEdit",
        &["session", "ping", "--silent", "--project", project_id],
    ));
    post_tool.push(entry(
        "mcp__muninn__muninn_save",
        &["save-ack", "--silent", "--project", project_id],
    ));
    post_tool.push(entry(
        "Write|Edit|Create|MultiEdit|Bash",
        &["extract", "--silent", "--project", project_id],
    ));
    if with_auto_capture {
        // Auto-capture queue commands (Phase 6 — opt-in).
        post_tool.push(entry(
            "Write|Edit|Create|MultiEdit|Bash",
            &["enqueue", "--silent", "--project", project_id],
        ));
    }

    let mut user_prompt = user_prompt;
    user_prompt.push(entry(".*", &["nudge", "--silent", "--project", project_id]));

    let mut session_end = session_end;
    if with_auto_capture {
        session_end.push(entry(
            ".*",
            &["summarize", "--silent", "--project", project_id],
        ));
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

    write_json_pretty(&settings_path, &settings)?;

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

/// Shell-form hook entry (Unix). Windows builds exec-form inline in
/// `runar_hook_entry`, so this is only compiled off-Windows.
#[cfg(not(windows))]
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

// ── setup vscode / opencode / codex (auto-write config) ────────────

/// Auto-write `.vscode/mcp.json` in the CWD. VSCode's native MCP config uses
/// the top-level `servers` key; a stdio server is `{ command, args }` (the
/// `type` field defaults to stdio). Merge-preserves any sibling servers/keys.
pub fn setup_vscode() -> anyhow::Result<PathBuf> {
    let binary_path = resolve_stable_binary();
    let dir = std::env::current_dir()?.join(".vscode");
    fs::create_dir_all(&dir)?;
    let path = dir.join("mcp.json");

    let mut root = read_json_or_empty(&path)?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    let servers = obj
        .entry("servers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`servers` in {} is not an object", path.display()))?;
    servers.insert(
        "muninn".into(),
        json!({ "command": binary_path, "args": ["mcp-muninn"] }),
    );

    write_json_pretty(&path, &root)?;
    Ok(path)
}

/// Auto-write `opencode.json` in the CWD. OpenCode's config uses the top-level
/// `mcp` key; a local (stdio) server is `{ type: "local", command: [argv...],
/// enabled: true }`. Merge-preserves the `$schema` line and any sibling keys.
pub fn setup_opencode() -> anyhow::Result<PathBuf> {
    let binary_path = resolve_stable_binary();
    let path = std::env::current_dir()?.join("opencode.json");

    let mut root = read_json_or_empty(&path)?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?;
    obj.entry("$schema")
        .or_insert_with(|| json!("https://opencode.ai/config.json"));
    let mcp = obj
        .entry("mcp")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`mcp` in {} is not an object", path.display()))?;
    mcp.insert(
        "muninn".into(),
        json!({
            "type": "local",
            "command": [binary_path, "mcp-muninn"],
            "enabled": true,
        }),
    );

    write_json_pretty(&path, &root)?;
    Ok(path)
}

/// Auto-write the global `~/.codex/config.toml`. Codex's MCP config is TOML
/// and global (no per-project file), keyed by `[mcp_servers.<name>]`. We use
/// `toml_edit` so existing comments/formatting/tables survive the merge.
pub fn setup_codex() -> anyhow::Result<PathBuf> {
    use toml_edit::{value, Array, DocumentMut, Item, Table};

    let binary_path = resolve_stable_binary();
    let path = home_dir().join(".codex").join("config.toml");
    fs::create_dir_all(path.parent().expect(".codex has a parent"))?;

    let mut doc: DocumentMut = if path.exists() {
        fs::read_to_string(&path)?
            .parse()
            .map_err(|e| anyhow::anyhow!("{} is not valid TOML: {e}", path.display()))?
    } else {
        DocumentMut::new()
    };

    // Header-style `[mcp_servers.muninn]` table (not an inline table), so the
    // file stays hand-editable. Preserves any existing servers + comments.
    let servers = doc
        .entry("mcp_servers")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("`mcp_servers` in {} is not a table", path.display()))?;

    let mut muninn = Table::new();
    muninn["command"] = value(binary_path);
    muninn["args"] = value(Array::from_iter(["mcp-muninn"]));
    servers.insert("muninn", Item::Table(muninn));

    fs::write(&path, doc.to_string())?;
    Ok(path)
}

// ── Path helper for hook subcommands ───────────────────────────────

/// Per-project temp ping file, used by session/nudge/save-ack. Uses
/// `std::env::temp_dir()` so it honors `TMPDIR` on Unix and `TEMP`/`TMP` on
/// Windows — the old hardcoded `/tmp` fallback wrote a bogus path on Windows.
pub fn ping_file_path(project_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("runar-ping-{project_id}"))
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

    #[cfg(not(windows))]
    #[test]
    fn hook_entry_shape() {
        let h = hook_entry(".*", "echo hi");
        assert_eq!(h["matcher"], ".*");
        assert_eq!(h["hooks"][0]["type"], "command");
        assert_eq!(h["hooks"][0]["command"], "echo hi");
    }

    #[test]
    fn runar_hook_entry_is_robust_per_os() {
        let h = runar_hook_entry(
            ".*",
            "/bin/runar",
            &["context", "--silent", "--project", "proj"],
            "/log/hook.log",
        );
        assert_eq!(h["matcher"], ".*");
        assert_eq!(h["hooks"][0]["type"], "command");
        let inner = &h["hooks"][0];

        // Idempotency: re-running setup must re-detect & replace this entry,
        // which `filter_runar_hooks` keys off "runar" in the command field.
        let cmd = inner["command"].as_str().unwrap();
        assert!(cmd.contains("runar"));

        #[cfg(not(windows))]
        {
            // Shell form: every token single-quoted, stderr→log, exit 0.
            assert!(cmd.contains("'context'"));
            assert!(cmd.contains("'--project' 'proj'"));
            assert!(cmd.contains("2>>"));
            assert!(cmd.ends_with("; exit 0"));
            assert!(inner.get("args").is_none());
        }
        #[cfg(windows)]
        {
            // Exec form: bare executable + raw arg vector, no shell syntax.
            assert_eq!(cmd, "/bin/runar");
            assert_eq!(
                inner["args"],
                serde_json::json!(["context", "--silent", "--project", "proj"])
            );
        }
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
        crate::test_support::with_runar_home(|| {
            let path = write_env_file("postgresql").unwrap();
            let content = fs::read_to_string(&path).unwrap();
            assert!(content.contains("RUNAR_STORAGE=postgresql"));
            assert!(content.contains("RUNAR_DB_URL=postgresql://"));
        });
    }

    #[test]
    fn write_env_file_sqlite() {
        crate::test_support::with_runar_home(|| {
            let path = write_env_file("sqlite").unwrap();
            let content = fs::read_to_string(&path).unwrap();
            assert!(content.contains("RUNAR_STORAGE=sqlite"));
            assert!(content.contains("RUNAR_SQLITE_PATH="));
        });
    }
}
