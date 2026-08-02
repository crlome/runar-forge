//! Setup / init subcommands — configure the host environment for a user.
//!
//! Ports the TS `runar setup claude-code` + `runar init` logic to Rust.
//! The single Rust binary exposes all three tool families (muninn, huginn,
//! curator) via the unified `mcp-muninn` command, so only ONE MCP server
//! is registered here (unlike the TS version which registers three).

use std::fs;
use std::path::{Path, PathBuf};
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
    /// Whether the opt-in `Grep|Glob` search-hint PreToolUse hook was written,
    /// so the caller's summary reports what is actually on disk.
    pub search_hints: bool,
    /// Likewise for the opt-in auto-refresh hooks.
    pub graph_autorefresh: bool,
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

/// Configure Claude Code for `project_id`.
///
/// `with_search_hints` is authoritative: whatever the caller passes is what
/// ends up on disk. Callers that want a re-run to keep the project's previous
/// choice read it back first with [`search_hints_installed`] — this function
/// does not guess, so an explicit opt-out is always honoured.
pub fn setup_claude_code(
    project_id: &str,
    with_auto_capture: bool,
    with_search_hints: bool,
    with_graph_autorefresh: bool,
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
    let existing_hooks = sobj.get("hooks").cloned().unwrap_or(json!({}));
    let hooks_obj = build_hooks_object(
        &existing_hooks,
        &binary_path,
        project_id,
        &log_path,
        with_auto_capture,
        with_search_hints,
        with_graph_autorefresh,
    );
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
        search_hints: with_search_hints,
        graph_autorefresh: with_graph_autorefresh,
    })
}

/// Does `dir`'s `.claude/settings.json` already carry the opt-in search-hint
/// PreToolUse hook? Lets a re-run default to the choice the project already
/// made, instead of silently switching a PreToolUse hook on or off.
pub fn search_hints_installed(dir: &Path) -> bool {
    let settings_path = dir.join(".claude").join("settings.json");
    let Ok(settings) = read_json_or_empty(&settings_path) else {
        return false;
    };
    installed_search_hints(&settings.get("hooks").cloned().unwrap_or(json!({})))
}

/// The argument vector of every hook command setup would install, with all
/// opt-ins enabled.
///
/// Exists so the binary can check that it still accepts every command it asks
/// Claude Code to run. Nothing else validates that: the hooks are strings in a
/// JSON file, so a subcommand that moves — `autorefresh` becoming
/// `graph autorefresh`, say — leaves settings.json pointing at a command that
/// no longer parses, and the only symptom is a hook that silently does nothing
/// forever.
pub fn installed_hook_argvs(project_id: &str) -> Vec<Vec<String>> {
    let hooks = build_hooks_object(
        &json!({}),
        "/bin/runar",
        project_id,
        "/log/hook.log",
        true,
        true,
        true,
    );
    let mut out = Vec::new();
    for entries in hooks.values().filter_map(|v| v.as_array()) {
        for entry in entries {
            let tokens = runar_hook_tokens(entry);
            // Drop the binary path, and stop at the shell redirect the Unix
            // form appends — neither is part of the command's own arguments.
            let argv: Vec<String> = tokens
                .into_iter()
                .skip(1)
                .take_while(|t| !t.starts_with("2>>"))
                .collect();
            if !argv.is_empty() {
                out.push(argv);
            }
        }
    }
    out
}

/// Same question for the opt-in auto-refresh hooks. `build_hooks_object`
/// rewrites every runar hook from its flags, so without reading the previous
/// choice back a bare `runar setup claude-code` re-run would uninstall this.
pub fn graph_autorefresh_installed(dir: &Path) -> bool {
    let settings_path = dir.join(".claude").join("settings.json");
    let Ok(settings) = read_json_or_empty(&settings_path) else {
        return false;
    };
    installed_graph_autorefresh(&settings.get("hooks").cloned().unwrap_or(json!({})))
}

/// Result of `setup claude-code --all-projects`.
pub struct MigrationOutcome {
    pub migrated: Vec<HookMigration>,
    /// Projects carrying runar hooks whose `--project` id could not be read.
    /// Surfaced rather than skipped quietly — a migration that silently does
    /// nothing is worse than one that fails loudly.
    pub skipped: Vec<PathBuf>,
}

/// One project's outcome from `setup claude-code --all-projects`.
pub struct HookMigration {
    pub dir: PathBuf,
    pub project_id: String,
    pub had_legacy_pre_tool_use: bool,
}

/// Re-point every already-installed project at the current hook layout.
///
/// Hooks live in each project's own `.claude/settings.json`, so a change to
/// the layout only reaches the project you happen to be standing in. After the
/// v0.9.0 PreToolUse → SessionStart move that is not good enough: every other
/// project keeps a stale hook that injects nothing.
///
/// Projects come from the `projects` map in `~/.claude.json` (the paths Claude
/// Code itself has opened). Only directories that already carry a runar hook
/// are touched, and each keeps the `--project` id it was installed with — this
/// migrates, it does not enroll.
pub fn migrate_installed_hooks() -> anyhow::Result<MigrationOutcome> {
    let binary_path = resolve_stable_binary();
    let log_path = runar_dir().join("hook.log").to_string_lossy().into_owned();

    let claude_json: Value = read_json_or_empty(&home_dir().join(".claude.json"))?;
    let Some(projects) = claude_json.get("projects").and_then(|v| v.as_object()) else {
        return Ok(MigrationOutcome {
            migrated: Vec::new(),
            skipped: Vec::new(),
        });
    };

    let mut migrated = Vec::new();
    let mut skipped: Vec<PathBuf> = Vec::new();
    for dir in projects.keys() {
        let dir = PathBuf::from(dir);
        let settings_path = dir.join(".claude").join("settings.json");
        if !settings_path.exists() {
            continue;
        }
        let mut settings: Value = match read_json_or_empty(&settings_path) {
            Ok(v) => v,
            Err(_) => continue, // unreadable or malformed — leave it alone
        };
        let Some(sobj) = settings.as_object_mut() else {
            continue;
        };
        let existing_hooks = sobj.get("hooks").cloned().unwrap_or(json!({}));
        let Some(project_id) = installed_project_id(&existing_hooks) else {
            // A project carrying runar hooks whose `--project` we cannot read
            // is a migration failure, not a project to pass over in silence.
            if !runar_hook_entries(&existing_hooks).is_empty() {
                skipped.push(dir);
            }
            continue;
        };

        let had_legacy_pre_tool_use = has_legacy_pre_tool_use(&existing_hooks);
        // Preserve the choices this project was set up with. A migration
        // re-points hooks at the current layout; it does not enroll anyone in
        // a feature they did not ask for, and it does not revoke one they did.
        let with_auto_capture = installed_auto_capture(&existing_hooks);
        let with_search_hints = installed_search_hints(&existing_hooks);
        let with_graph_autorefresh = installed_graph_autorefresh(&existing_hooks);

        let hooks_obj = build_hooks_object(
            &existing_hooks,
            &binary_path,
            &project_id,
            &log_path,
            with_auto_capture,
            with_search_hints,
            with_graph_autorefresh,
        );
        sobj.insert("hooks".into(), Value::Object(hooks_obj));
        write_json_pretty(&settings_path, &settings)?;

        migrated.push(HookMigration {
            dir,
            project_id,
            had_legacy_pre_tool_use,
        });
    }
    Ok(MigrationOutcome { migrated, skipped })
}

/// Strip the shell quoting `shell_quote` adds around every argument. The FLAG
/// is quoted too, not just its value: a hook written by a recent version reads
/// `'--project' 'proj'`, so comparing a raw token against `--project` matched
/// nothing and skipped the whole project. Since v0.9.0's own setup writes that
/// quoted form, that made the migration a no-op on anything it had already
/// touched.
fn unquote(s: &str) -> &str {
    s.trim_matches(|c| c == '\'' || c == '"')
}

/// Unquoted argument tokens of every runar-owned command in one hook entry.
///
/// Reads both representations `runar_hook_entry` emits: the Unix shell-form
/// string and the Windows exec form, where the subcommand lives in `args` and
/// a scan of `command` alone sees nothing but the binary path.
fn runar_hook_tokens(entry: &Value) -> Vec<String> {
    let Some(inner) = entry.get("hooks").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut tokens = Vec::new();
    for h in inner {
        let Some(command) = h.get("command").and_then(|c| c.as_str()) else {
            continue;
        };
        if !command.contains("runar") {
            continue;
        }
        tokens.extend(command.split_whitespace().map(|t| unquote(t).to_string()));
        if let Some(args) = h.get("args").and_then(|v| v.as_array()) {
            tokens.extend(
                args.iter()
                    .filter_map(|a| a.as_str())
                    .map(|a| unquote(a).to_string()),
            );
        }
    }
    tokens
}

/// Every runar hook entry across all events, in any order.
fn runar_hook_entries(existing_hooks: &Value) -> Vec<&Value> {
    let Some(events) = existing_hooks.as_object() else {
        return Vec::new();
    };
    events
        .values()
        .filter_map(|v| v.as_array())
        .flatten()
        .filter(|entry| !runar_hook_tokens(entry).is_empty())
        .collect()
}

/// The runar-owned PreToolUse entries only.
fn pre_tool_runar_entries(existing_hooks: &Value) -> Vec<&Value> {
    existing_hooks
        .get("PreToolUse")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|entry| !runar_hook_tokens(entry).is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn is_search_hint_entry(entry: &Value) -> bool {
    runar_hook_tokens(entry).iter().any(|t| t == "hint")
}

/// Is the opt-in search-hint hook already installed?
/// The auto-refresh entries, identified by their unique subcommand token.
fn is_graph_autorefresh_entry(entry: &Value) -> bool {
    runar_hook_tokens(entry).iter().any(|t| t == "autorefresh")
}

fn installed_graph_autorefresh(existing_hooks: &Value) -> bool {
    runar_hook_entries(existing_hooks)
        .into_iter()
        .any(is_graph_autorefresh_entry)
}

fn installed_search_hints(existing_hooks: &Value) -> bool {
    pre_tool_runar_entries(existing_hooks)
        .into_iter()
        .any(is_search_hint_entry)
}

/// A runar PreToolUse entry that is *not* the opt-in search-hint hook — i.e.
/// the pre-v0.9.0 `context` hook this migration exists to remove. A project
/// that opted into search hints carries a PreToolUse entry by design, so it
/// must not be counted as a legacy install on every re-run.
fn has_legacy_pre_tool_use(existing_hooks: &Value) -> bool {
    pre_tool_runar_entries(existing_hooks)
        .into_iter()
        .any(|e| !is_search_hint_entry(e))
}

/// Was this project set up with auto-capture? Matched on unquoted tokens: a
/// substring test for " enqueue" misses `'enqueue'` and would silently turn
/// auto-capture off for every recently-configured project.
fn installed_auto_capture(existing_hooks: &Value) -> bool {
    runar_hook_entries(existing_hooks).into_iter().any(|e| {
        runar_hook_tokens(e)
            .iter()
            .any(|t| t == "enqueue" || t == "summarize")
    })
}

/// Recover the `--project <id>` a project's hooks were installed with, so a
/// migration never silently re-namespaces someone's memories.
fn installed_project_id(existing_hooks: &Value) -> Option<String> {
    for entry in runar_hook_entries(existing_hooks) {
        let tokens = runar_hook_tokens(entry);
        let mut parts = tokens.iter();
        while let Some(tok) = parts.next() {
            if tok != "--project" {
                continue;
            }
            if let Some(id) = parts.next() {
                if !id.is_empty() {
                    return Some(id.clone());
                }
            }
        }
    }
    None
}

/// Build the complete `hooks` object for `.claude/settings.json`.
///
/// Pure so the wiring is testable without touching `$HOME` or the CWD — the
/// PreToolUse → SessionStart move in v0.9.0 is exactly the kind of change that
/// needs a regression test rather than a manual eyeball of a settings file.
fn build_hooks_object(
    existing_hooks: &Value,
    binary_path: &str,
    project_id: &str,
    log_path: &str,
    with_auto_capture: bool,
    with_search_hints: bool,
    with_graph_autorefresh: bool,
) -> serde_json::Map<String, Value> {
    // Render each hook from a raw arg vector. Shell-form (quoted) on Unix,
    // exec-form (no shell) on Windows — see `runar_hook_entry`.
    let entry =
        |matcher: &str, args: &[&str]| runar_hook_entry(matcher, binary_path, args, log_path);

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
    let session_start = filter_runar_hooks(
        existing_hooks
            .get("SessionStart")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    );

    // `pre_tool` now holds only non-runar entries; the runar ones were stripped
    // by filter_runar_hooks, which is also what migrates existing installs off
    // the pre-v0.9.0 context hook on re-run.
    let mut pre_tool = pre_tool;
    if with_search_hints {
        // The one PreToolUse hook allowed back in, and only opt-in. What keeps
        // it survivable is the matcher: `Grep|Glob` fire a handful of times per
        // session where the v0.9.0 ".*" hook fired 243×. Widening this to
        // `Read` or `Search` puts it straight back on the hot path.
        //
        // Routed through `hint`, never `context`: main.rs deliberately empties
        // any PreToolUse `context` payload, so `context` here would be a hook
        // that fires and delivers nothing.
        pre_tool.push(entry(
            "Grep|Glob",
            &["hint", "--silent", "--project", project_id],
        ));
    }

    // The context packet is session-scoped, so it belongs on SessionStart.
    // Until v0.9.0 it was wired to PreToolUse with matcher ".*" — every tool
    // call, no cache: 243 fires per session, 98.8% of them byte-identical.
    let mut session_start = session_start;
    session_start.push(entry(
        ".*",
        &["context", "--silent", "--project", project_id],
    ));
    if with_graph_autorefresh {
        // Also at session start, because the edits a session opens against
        // were often made somewhere this hook could not see: another editor, a
        // pull, a branch switch. Same command, same debounce — it costs
        // nothing when the graph is already current.
        session_start.push(entry(
            ".*",
            &["graph", "autorefresh", "--silent", "--project", project_id],
        ));
    }

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
    if with_graph_autorefresh {
        // Opt-in, and the first hook that writes to the code graph rather than
        // reading it. What keeps it cheap is that it does no work itself: it
        // reads one timestamp file and hands off to a detached child, so the
        // tool call that triggered it is never waiting on an index.
        //
        // No `Bash` in the matcher, unlike the passive-learning hooks: a
        // command that happens to touch files is not a signal worth paying a
        // process spawn for on every shell invocation.
        post_tool.push(entry(
            "Write|Edit|Create|MultiEdit",
            &["graph", "autorefresh", "--silent", "--project", project_id],
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
    // Drop the key entirely when nothing is left, rather than leaving an empty
    // array behind from a pre-v0.9.0 install.
    if pre_tool.is_empty() {
        hooks_obj.remove("PreToolUse");
    } else {
        hooks_obj.insert("PreToolUse".into(), Value::Array(pre_tool));
    }
    hooks_obj.insert("SessionStart".into(), Value::Array(session_start));
    hooks_obj.insert("PostToolUse".into(), Value::Array(post_tool));
    hooks_obj.insert("UserPromptSubmit".into(), Value::Array(user_prompt));
    if with_auto_capture {
        hooks_obj.insert("SessionEnd".into(), Value::Array(session_end));
    }
    // When auto-capture is off we leave any existing SessionEnd key alone
    // rather than introducing an empty array the user never asked for.

    hooks_obj
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

    fn hooks_for(existing: Value) -> serde_json::Map<String, Value> {
        build_hooks_object(
            &existing,
            "/bin/runar",
            "proj",
            "/log/hook.log",
            false,
            false,
            false,
        )
    }

    /// Same, with the opt-in search-hint hook enabled.
    fn hooks_with_hints(existing: Value) -> serde_json::Map<String, Value> {
        build_hooks_object(
            &existing,
            "/bin/runar",
            "proj",
            "/log/hook.log",
            false,
            true,
            false,
        )
    }

    /// Same, with the opt-in auto-refresh hooks enabled.
    fn hooks_with_autorefresh(existing: Value) -> serde_json::Map<String, Value> {
        build_hooks_object(
            &existing,
            "/bin/runar",
            "proj",
            "/log/hook.log",
            false,
            false,
            true,
        )
    }

    fn entries_for(hooks: &serde_json::Map<String, Value>, event: &str) -> Vec<Value> {
        hooks
            .get(event)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// Full invocation per hook entry. Unix stores one shell-form string;
    /// Windows stores the binary in `command` and the subcommand in `args`,
    /// so asserting on `command` alone would silently pass there.
    fn commands_for(hooks: &serde_json::Map<String, Value>, event: &str) -> Vec<String> {
        hooks
            .get(event)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let inner = &e["hooks"][0];
                        let command = inner["command"].as_str()?;
                        let args = inner["args"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default();
                        Some(if args.is_empty() {
                            command.to_string()
                        } else {
                            format!("{command} {args}")
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn project_id_is_read_from_both_quoting_styles() {
        // `shell_quote` wraps every argument, so hooks written by a recent
        // version read `'--project' 'proj'`. Comparing the raw token against
        // `--project` matched nothing, so `--all-projects` skipped those
        // projects in silence — including every project it had itself
        // configured, which made a second run a no-op.
        let fully_quoted = json!({
            "PreToolUse": [{
                "matcher": ".*",
                "hooks": [{
                    "type": "command",
                    "command": "'/Users/x/.runar-forge/bin/runar' 'context' '--silent' '--project' 'valcore-lead-generator' 2>>'/x/hook.log' ; exit 0"
                }]
            }]
        });
        assert_eq!(
            installed_project_id(&fully_quoted).as_deref(),
            Some("valcore-lead-generator")
        );

        let flag_unquoted = json!({
            "PreToolUse": [{
                "matcher": ".*",
                "hooks": [{
                    "type": "command",
                    "command": "'/Users/x/.runar-forge/bin/runar' context --silent --project 'gsv2' 2>>'/x/hook.log' ; exit 0"
                }]
            }]
        });
        assert_eq!(
            installed_project_id(&flag_unquoted).as_deref(),
            Some("gsv2")
        );
    }

    #[test]
    fn auto_capture_is_preserved_through_quoted_hooks() {
        // Same quoting trap: a substring test for " enqueue" misses
        // `'enqueue'` and would silently turn auto-capture off on migration.
        let quoted_with_capture = json!({
            "PostToolUse": [{
                "matcher": "Write|Edit",
                "hooks": [{
                    "type": "command",
                    "command": "'/x/runar' 'enqueue' '--silent' '--project' 'proj' 2>>'/x/hook.log' ; exit 0"
                }]
            }]
        });
        assert!(
            installed_auto_capture(&quoted_with_capture),
            "auto-capture must survive a migration"
        );
        let fresh = Value::Object(hooks_for(json!({})));
        assert!(!installed_auto_capture(&fresh));
    }

    #[test]
    fn context_hook_is_wired_to_session_start_not_pre_tool_use() {
        let hooks = hooks_for(json!({}));

        let session_start = commands_for(&hooks, "SessionStart");
        assert!(
            session_start.iter().any(|c| c.contains("context")),
            "context packet must fire once per session: {session_start:?}"
        );
        assert!(
            !hooks.contains_key("PreToolUse"),
            "PreToolUse fires on every tool call — 243×/session with no cache"
        );
    }

    #[test]
    fn rerunning_setup_migrates_a_legacy_pre_tool_use_install() {
        // What every pre-v0.9.0 project has on disk today.
        let legacy = json!({
            "PreToolUse": [{
                "matcher": ".*",
                "hooks": [{
                    "type": "command",
                    "command": "'/Users/x/.runar-forge/bin/runar' context --silent --project 'proj'"
                }]
            }]
        });

        let hooks = hooks_for(legacy);

        assert!(
            !hooks.contains_key("PreToolUse"),
            "the stale runar PreToolUse entry must be removed, not left behind"
        );
        assert!(commands_for(&hooks, "SessionStart")
            .iter()
            .any(|c| c.contains("context")));
    }

    #[test]
    fn migration_preserves_third_party_pre_tool_use_hooks() {
        let existing = json!({
            "PreToolUse": [
                {
                    "matcher": ".*",
                    "hooks": [{ "type": "command", "command": "'/x/.runar-forge/bin/runar' context --silent" }]
                },
                {
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "/usr/local/bin/my-own-linter" }]
                }
            ]
        });

        let hooks = hooks_for(existing.clone());
        let pre_tool = commands_for(&hooks, "PreToolUse");

        assert_eq!(
            pre_tool,
            vec!["/usr/local/bin/my-own-linter".to_string()],
            "someone else's hooks are not ours to delete"
        );

        // Same with the opt-in hook on: ours is added, theirs is untouched.
        let hooks = hooks_with_hints(existing);
        let pre_tool = commands_for(&hooks, "PreToolUse");
        assert_eq!(pre_tool.len(), 2, "{pre_tool:?}");
        assert!(pre_tool.iter().any(|c| c == "/usr/local/bin/my-own-linter"));
        assert!(!pre_tool.iter().any(|c| c.contains("context")));
    }

    #[test]
    fn search_hints_add_exactly_one_narrow_pre_tool_use_hook() {
        let hooks = hooks_with_hints(json!({}));
        let pre_tool = entries_for(&hooks, "PreToolUse");

        assert_eq!(
            pre_tool.len(),
            1,
            "PreToolUse is the hot path — one entry, never a second: {pre_tool:?}"
        );
        assert_eq!(pre_tool[0]["matcher"], "Grep|Glob");

        let tokens = runar_hook_tokens(&pre_tool[0]);
        let rendered = commands_for(&hooks, "PreToolUse");
        assert!(
            tokens.iter().any(|t| t == "hint"),
            "must run the code-graph hint subcommand: {rendered:?}"
        );
        assert!(
            !tokens.iter().any(|t| t == "context"),
            "main.rs empties any PreToolUse `context` payload by design — a \
             `context` hook here fires and delivers nothing: {rendered:?}"
        );
        assert_eq!(
            tokens.iter().filter(|t| *t == "--project").count(),
            1,
            "{rendered:?}"
        );
        assert!(tokens.iter().any(|t| t == "proj"), "{rendered:?}");

        // Everything else stays exactly where v0.9.0 put it.
        assert!(commands_for(&hooks, "SessionStart")
            .iter()
            .any(|c| c.contains("context")));
    }

    #[test]
    fn search_hint_matcher_can_never_widen() {
        // The incident this guards: matcher ".*" on PreToolUse fired 243×
        // per session, 98.8% of the payloads byte-identical. `Grep|Glob` fire
        // a handful of times; `Read`, `Search` or a bare wildcard would put
        // the hook back on the hot path with no other code change needed.
        let hooks = hooks_with_hints(json!({}));
        for entry in entries_for(&hooks, "PreToolUse") {
            let matcher = entry["matcher"].as_str().expect("matcher is a string");
            assert_eq!(
                matcher, "Grep|Glob",
                "the search-hint matcher must not widen"
            );
            let tools: Vec<&str> = matcher.split('|').collect();
            assert_eq!(tools, ["Grep", "Glob"]);
            for banned in [
                "Read",
                "Search",
                "Bash",
                "Task",
                "Write",
                "Edit",
                "MultiEdit",
                ".*",
                "*",
                "",
            ] {
                assert!(
                    !tools.contains(&banned),
                    "PreToolUse must not match `{banned}`"
                );
            }
        }
    }

    #[test]
    fn legacy_context_pre_tool_use_is_removed_even_with_search_hints_on() {
        // The opt-in hook must not become a hiding place for the hook v0.9.0
        // removed: the legacy `.*` context entry still has to go.
        let legacy = json!({
            "PreToolUse": [{
                "matcher": ".*",
                "hooks": [{
                    "type": "command",
                    "command": "'/Users/x/.runar-forge/bin/runar' context --silent --project 'proj'"
                }]
            }]
        });

        let hooks = hooks_with_hints(legacy);
        let pre_tool = entries_for(&hooks, "PreToolUse");

        assert_eq!(pre_tool.len(), 1, "{pre_tool:?}");
        assert_eq!(pre_tool[0]["matcher"], "Grep|Glob");
        let rendered = commands_for(&hooks, "PreToolUse");
        assert!(
            !rendered.iter().any(|c| c.contains("context")),
            "{rendered:?}"
        );
        assert!(commands_for(&hooks, "SessionStart")
            .iter()
            .any(|c| c.contains("context")));
    }

    #[test]
    fn a_rerun_preserves_an_installed_search_hint_hook() {
        // `build_hooks_object` always rewrites from the flag, so what carries
        // the choice across a re-run is the detection. Migration reads the
        // settings file; the plain path reads `search_hints_installed`.
        let installed = Value::Object(hooks_with_hints(json!({})));
        assert!(installed_search_hints(&installed));
        assert!(
            !has_legacy_pre_tool_use(&installed),
            "the opt-in hook is not a legacy install to migrate off"
        );

        let rebuilt = build_hooks_object(
            &installed,
            "/bin/runar",
            "proj",
            "/log/hook.log",
            false,
            installed_search_hints(&installed),
            false,
        );
        let pre_tool = entries_for(&rebuilt, "PreToolUse");
        assert_eq!(pre_tool.len(), 1, "no duplicate on re-run: {pre_tool:?}");
        assert_eq!(pre_tool[0]["matcher"], "Grep|Glob");
    }

    #[test]
    /// What Claude Code actually reads is settings.json, so the assertion is
    /// on the entries themselves — matcher included, since that is what
    /// decides how often this fires.
    fn graph_autorefresh_writes_both_triggers_and_only_when_asked() {
        let off = hooks_for(json!({}));
        assert!(
            !entries_for(&off, "PostToolUse")
                .iter()
                .any(is_graph_autorefresh_entry),
            "auto-refresh must be opt-in"
        );
        assert!(!entries_for(&off, "SessionStart")
            .iter()
            .any(is_graph_autorefresh_entry));

        let on = hooks_with_autorefresh(json!({}));
        let post: Vec<Value> = entries_for(&on, "PostToolUse")
            .into_iter()
            .filter(is_graph_autorefresh_entry)
            .collect();
        assert_eq!(post.len(), 1, "expected one write trigger: {post:?}");
        assert_eq!(
            post[0]["matcher"], "Write|Edit|Create|MultiEdit",
            "a wider matcher puts this on the hot path"
        );
        let tokens = runar_hook_tokens(&post[0]);
        assert!(tokens.iter().any(|t| t == "autorefresh"), "{tokens:?}");
        assert!(tokens.iter().any(|t| t == "--silent"), "{tokens:?}");
        assert!(tokens.iter().any(|t| t == "proj"), "{tokens:?}");

        let start: Vec<Value> = entries_for(&on, "SessionStart")
            .into_iter()
            .filter(is_graph_autorefresh_entry)
            .collect();
        assert_eq!(start.len(), 1, "expected one session trigger");
        assert_eq!(start[0]["matcher"], ".*");

        // The context hook is still there beside it.
        assert_eq!(entries_for(&on, "SessionStart").len(), 2);
    }

    #[test]
    /// `build_hooks_object` rewrites every runar hook from its flags, so a
    /// bare re-run that did not read the previous choice back would silently
    /// uninstall this. The same defect once killed auto-capture.
    fn a_rerun_that_reads_the_choice_back_keeps_auto_refresh() {
        let installed = Value::Object(hooks_with_autorefresh(json!({})));
        assert!(installed_graph_autorefresh(&installed));

        let rebuilt = build_hooks_object(
            &installed,
            "/bin/runar",
            "proj",
            "/log/hook.log",
            false,
            false,
            installed_graph_autorefresh(&installed),
        );
        let post: Vec<Value> = entries_for(&rebuilt, "PostToolUse")
            .into_iter()
            .filter(is_graph_autorefresh_entry)
            .collect();
        assert_eq!(post.len(), 1, "no duplicate on re-run: {post:?}");
        assert!(installed_graph_autorefresh(&Value::Object(rebuilt)));

        // And an explicit opt-out still removes it.
        let removed = build_hooks_object(
            &installed,
            "/bin/runar",
            "proj",
            "/log/hook.log",
            false,
            false,
            false,
        );
        assert!(!installed_graph_autorefresh(&Value::Object(removed)));
    }

    #[test]
    /// The hint hook and the auto-refresh hooks are independent opt-ins;
    /// detecting one must never read as the other.
    fn the_two_opt_ins_do_not_shadow_each_other() {
        let hints = Value::Object(hooks_with_hints(json!({})));
        assert!(installed_search_hints(&hints));
        assert!(!installed_graph_autorefresh(&hints));

        let auto = Value::Object(hooks_with_autorefresh(json!({})));
        assert!(installed_graph_autorefresh(&auto));
        assert!(!installed_search_hints(&auto));
    }

    #[test]
    fn a_rerun_does_not_silently_enroll_a_project_in_search_hints() {
        let fresh = Value::Object(hooks_for(json!({})));
        assert!(!installed_search_hints(&fresh));

        // A pre-v0.9.0 install carries a PreToolUse entry, but it is the
        // `context` hook — migrating it must not read as an opt-in.
        let legacy = json!({
            "PreToolUse": [{
                "matcher": ".*",
                "hooks": [{
                    "type": "command",
                    "command": "'/Users/x/.runar-forge/bin/runar' 'context' '--silent' '--project' 'proj'"
                }]
            }]
        });
        assert!(!installed_search_hints(&legacy));
        assert!(has_legacy_pre_tool_use(&legacy));
    }

    #[test]
    fn installed_state_is_read_from_exec_form_hooks() {
        // Windows hooks put the subcommand in `args`; reading `command` alone
        // sees only the binary path, so every migration decision — project id,
        // auto-capture, search hints — would silently read as "not set".
        let exec_form = json!({
            "PreToolUse": [{
                "matcher": "Grep|Glob",
                "hooks": [{
                    "type": "command",
                    "command": "C:\\Users\\x\\.runar-forge\\bin\\runar.exe",
                    "args": ["hint", "--silent", "--project", "proj"]
                }]
            }],
            "SessionEnd": [{
                "matcher": ".*",
                "hooks": [{
                    "type": "command",
                    "command": "C:\\Users\\x\\.runar-forge\\bin\\runar.exe",
                    "args": ["summarize", "--silent", "--project", "proj"]
                }]
            }]
        });

        assert!(installed_search_hints(&exec_form));
        assert!(!has_legacy_pre_tool_use(&exec_form));
        assert!(installed_auto_capture(&exec_form));
        assert_eq!(installed_project_id(&exec_form).as_deref(), Some("proj"));
    }

    #[test]
    fn search_hints_installed_reads_a_projects_settings_file() {
        crate::test_support::with_runar_home(|| {
            let dir = home_dir();
            let claude_dir = dir.join(".claude");
            fs::create_dir_all(&claude_dir).unwrap();
            let settings_path = claude_dir.join("settings.json");

            assert!(
                !search_hints_installed(&dir),
                "a missing settings file is not an opt-in"
            );

            write_json_pretty(&settings_path, &json!({ "hooks": hooks_for(json!({})) })).unwrap();
            assert!(!search_hints_installed(&dir));

            write_json_pretty(
                &settings_path,
                &json!({ "hooks": hooks_with_hints(json!({})) }),
            )
            .unwrap();
            assert!(search_hints_installed(&dir));
        });
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
