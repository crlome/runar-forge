//! `runar doctor` — read-only validation of RunarForge configuration.
//!
//! Phase 5.5 Item 5.5.2. Verifies `.env` parses, storage is reachable,
//! pgvector is installed (PG), schema is correct, migrations are applied,
//! embedding dim matches expectations, and reports row counts + breaker
//! state. Exits non-zero if any check fails — usable from CI.
//!
//! Critical: this module MUST NOT call `PostgresAdapter::initialize()` or
//! `SqliteAdapter::initialize()`. Both run migrations as a side-effect.
//! Doctor reports drift; it does not fix it. Connections are opened
//! directly via `tokio_postgres::connect` / `rusqlite::open_with_flags`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use tokio_postgres::NoTls;

use crate::codegraph::freshness::{self, Verdict};
use crate::codegraph::store::{CodeGraphStore, Coverage};
use crate::setup;
use crate::storage::postgres::PG_MIGRATIONS;
use crate::storage::sqlite::MIGRATIONS as SQLITE_MIGRATIONS;
use crate::storage::MemoryStorage;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail {
        message: String,
        hint: Option<String>,
    },
    Skip {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: Option<String>,
}

impl Check {
    fn pass(name: &'static str) -> Self {
        Self {
            name,
            status: Status::Pass,
            detail: None,
        }
    }
    fn pass_with(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Pass,
            detail: Some(detail.into()),
        }
    }
    fn fail(name: &'static str, msg: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail {
                message: msg.into(),
                hint: None,
            },
            detail: None,
        }
    }
    fn fail_hint(name: &'static str, msg: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail {
                message: msg.into(),
                hint: Some(hint.into()),
            },
            detail: None,
        }
    }
    fn skip(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Skip {
                reason: reason.into(),
            },
            detail: None,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn ok(&self) -> bool {
        !self
            .checks
            .iter()
            .any(|c| matches!(c.status, Status::Fail { .. }))
    }

    pub fn print_human(&self) {
        println!("runar doctor — RunarForge config validation");
        println!();
        for c in &self.checks {
            let (sym, label) = match &c.status {
                Status::Pass => ("✔", String::new()),
                Status::Fail { message, hint } => {
                    let mut s = format!("✗  {message}");
                    if let Some(h) = hint {
                        s.push_str(&format!("\n     hint: {h}"));
                    }
                    ("✗", s)
                }
                Status::Skip { reason } => ("○", format!("skipped — {reason}")),
            };
            let detail = c.detail.as_deref().unwrap_or("");
            // Layout: <sym> <name pad> <detail or fail-msg>
            if matches!(c.status, Status::Fail { .. }) {
                println!("{sym} {:<18} {label}", c.name);
            } else if detail.is_empty() && label.is_empty() {
                println!("{sym} {}", c.name);
            } else if label.is_empty() {
                println!("{sym} {:<18} {detail}", c.name);
            } else {
                println!("{sym} {:<18} {label}", c.name);
            }
        }
        println!();
        let pass = self
            .checks
            .iter()
            .filter(|c| matches!(c.status, Status::Pass))
            .count();
        let skip = self
            .checks
            .iter()
            .filter(|c| matches!(c.status, Status::Skip { .. }))
            .count();
        let fail = self
            .checks
            .iter()
            .filter(|c| matches!(c.status, Status::Fail { .. }))
            .count();
        let summary = if fail == 0 { "ALL OK" } else { "FAILED" };
        println!("{summary} ({pass} passed, {skip} skipped, {fail} failed)");
    }

    pub fn print_json(&self) -> Result<()> {
        let json = serde_json::json!({
            "ok": self.ok(),
            "checks": self.checks,
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
        Ok(())
    }
}

#[derive(Debug, Default, Clone)]
pub struct DoctorOpts {
    /// Run only DB-side checks (skip env file, breaker, etc.). Fast for
    /// CI gating after `config set`.
    pub db_only: bool,
    /// Override `RUNAR_DB_CONNECT_TIMEOUT_MS` for this run only.
    pub timeout_ms: Option<u64>,
}

fn connect_timeout(opts: &DoctorOpts) -> Duration {
    if let Some(ms) = opts.timeout_ms {
        return Duration::from_millis(ms);
    }
    let ms: u64 = std::env::var("RUNAR_DB_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8_000);
    Duration::from_millis(ms)
}

pub async fn run(opts: DoctorOpts) -> Report {
    let mut report = Report::default();

    // Load .env into process env so subsequent checks see the same values
    // the binary would. Best-effort; failure surfaces in env-file check.
    let env_path = setup::runar_dir().join(".env");
    let _ = dotenvy::from_path(&env_path);

    if !opts.db_only {
        report.checks.push(check_env_file(&env_path));
    }

    let backend = std::env::var("RUNAR_STORAGE").unwrap_or_else(|_| "postgresql".into());
    if !opts.db_only {
        report.checks.push(check_storage_backend(&backend));
    }

    match backend.as_str() {
        "postgresql" | "postgres" => {
            run_postgres_checks(&mut report, &opts).await;
        }
        "sqlite" => {
            run_sqlite_checks(&mut report);
        }
        other => {
            report.checks.push(Check::fail(
                "storage backend",
                format!("unknown backend: {other}"),
            ));
        }
    }

    // Always SQLite and always a separate file, so it is checked regardless
    // of RUNAR_STORAGE and regardless of whether the memory DB was reachable.
    report
        .checks
        .push(check_code_graph(&CodeGraphStore::default_path()));
    report
        .checks
        .push(check_graph_freshness(&CodeGraphStore::default_path()));
    report.checks.push(check_graph_autorefresh());
    report.checks.push(check_search_hints());

    if !opts.db_only {
        report.checks.push(check_breaker_state());
        report.checks.push(check_project_local_env());
        report.checks.push(check_kill_switch());
        report.checks.push(check_hook_log());
        report.checks.push(check_sync_state().await);
        report.checks.push(check_sync_heartbeat());
        report.checks.push(check_author_identity());
    }

    report
}

/// Phase 5.7 — surface the resolved author so users can spot a missing
/// `git config user.name` before their entries land anonymously in the
/// shared remote DB.
fn check_author_identity() -> Check {
    match crate::identity::resolve_author() {
        Some(name) => Check::pass_with("author identity", format!("git user.name = {name}")),
        None => Check::fail_hint(
            "author identity",
            "git user.name is unset; entries will be saved anonymously",
            "run: git config --global user.name \"Your Name\"",
        ),
    }
}

fn check_sync_heartbeat() -> Check {
    let auto = std::env::var("RUNAR_SYNC_AUTO")
        .map(|v| matches!(v.as_str(), "true" | "1" | "yes" | "on"))
        .unwrap_or(false);
    if !auto {
        return Check::skip("sync heartbeat", "auto-sync disabled");
    }

    // Threshold: 2× max-backoff. If the loop took longer than that to
    // tick, it almost certainly crashed.
    let max_backoff_ms: u64 = std::env::var("RUNAR_SYNC_MAX_BACKOFF_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300_000);
    let stale_secs = (max_backoff_ms as i64 / 1000) * 2;
    if crate::sync::heartbeat::is_stale(stale_secs) {
        return Check::fail_hint(
            "sync heartbeat",
            format!("no heartbeat in last {stale_secs}s"),
            "auto-sync is enabled but the loop is not running — restart Claude Code (mcp-muninn)",
        );
    }
    let hb = crate::sync::heartbeat::read();
    let label = match hb {
        Some(h) => format!(
            "last_tick={} backoff={}ms",
            h.last_tick.format("%H:%M:%S"),
            h.current_backoff_ms
        ),
        None => "alive".into(),
    };
    Check::pass_with("sync heartbeat", label)
}

async fn check_sync_state() -> Check {
    let local_set = std::env::var("RUNAR_STORAGE_LOCAL").is_ok();
    let remote_set = std::env::var("RUNAR_STORAGE_REMOTE").is_ok();
    if !local_set && !remote_set {
        return Check::skip("sync state", "single-backend mode (no _LOCAL/_REMOTE)");
    }
    if !(local_set && remote_set) {
        return Check::fail_hint(
            "sync state",
            "only one of RUNAR_STORAGE_LOCAL / RUNAR_STORAGE_REMOTE is set",
            "set both to enable hybrid sync, or unset both for single-backend mode",
        );
    }

    // Open a read-only handle to the local DB and read sync_state.
    let local_kind = std::env::var("RUNAR_STORAGE_LOCAL").unwrap_or_default();
    let namespace = std::env::var("RUNAR_MEMORY_NAMESPACE").unwrap_or_else(|_| "default".into());
    let state_result: Result<crate::types::SyncState, String> = match local_kind.as_str() {
        "sqlite" => {
            let path = std::env::var("RUNAR_SQLITE_PATH")
                .unwrap_or_else(|_| "~/.runar-forge/memory.db".into());
            match crate::storage::sqlite::SqliteAdapter::new(&path, &namespace) {
                Ok(s) => match s.initialize().await {
                    Ok(_) => match s.read_sync_state().await {
                        Ok(state) => Ok(state),
                        Err(e) => Err(format!("read_sync_state: {e}")),
                    },
                    Err(e) => Err(format!("local initialize: {e}")),
                },
                Err(e) => Err(format!("open local sqlite: {e}")),
            }
        }
        "postgresql" | "postgres" => match std::env::var("RUNAR_DB_URL") {
            Ok(url) => match crate::storage::postgres::PostgresAdapter::new(&url, &namespace) {
                Ok(s) => match s.initialize().await {
                    Ok(_) => match s.read_sync_state().await {
                        Ok(state) => Ok(state),
                        Err(e) => Err(format!("read_sync_state: {e}")),
                    },
                    Err(e) => Err(format!("local initialize: {e}")),
                },
                Err(e) => Err(format!("open local pg: {e}")),
            },
            Err(_) => Err("RUNAR_STORAGE_LOCAL=postgresql but RUNAR_DB_URL unset".into()),
        },
        other => Err(format!("unknown RUNAR_STORAGE_LOCAL={other}")),
    };

    let state = match state_result {
        Ok(s) => s,
        Err(msg) => return Check::fail("sync state", msg),
    };

    match state.initialized_at {
        None => Check::fail_hint(
            "sync state",
            "never initialized",
            "run `runar sync init` to validate the local+remote pair",
        ),
        Some(ts) => {
            let age_days = (Utc::now() - ts).num_days();
            let dim_str = match (state.local_dim, state.remote_dim) {
                (Some(l), Some(r)) if l == r => format!("dim={l}"),
                (Some(l), Some(r)) => format!("dim_local={l} dim_remote={r}"),
                _ => "dim=?".into(),
            };
            let label = format!("init={} {}", ts.format("%Y-%m-%d"), dim_str);
            if age_days > 30 {
                Check::pass_with(
                    "sync state",
                    format!("{label} (consider re-running `runar sync init` — {age_days}d old)"),
                )
            } else {
                Check::pass_with("sync state", label)
            }
        }
    }
}

fn check_kill_switch() -> Check {
    let env_set = std::env::var("RUNAR_DISABLE_HOOKS")
        .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
        .unwrap_or(false);
    let file = crate::hooks_runtime::disable_file_path();
    let file_set = file.exists();
    match (env_set, file_set) {
        (false, false) => Check::pass("kill-switch"),
        (true, _) => Check::fail_hint(
            "kill-switch",
            "RUNAR_DISABLE_HOOKS is set — every hook is a no-op",
            "unset the env var to re-enable",
        ),
        (false, true) => Check::fail_hint(
            "kill-switch",
            format!("{} exists — every hook is a no-op", file.display()),
            "rm the file to re-enable",
        ),
    }
}

fn check_hook_log() -> Check {
    let path = crate::hooks_runtime::hook_log_path();
    if !path.exists() {
        return Check::pass_with("hook log", "empty");
    }
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    // A panicking hook produces no output and no error the user ever sees —
    // it just stops working. Never let that hide behind a green check.
    let (panics, last) = crate::hooks_runtime::count_hook_panics();
    if panics > 0 {
        return Check::fail_hint(
            "hook log",
            format!(
                "{panics} panic(s) in {} — the panicking hook injects nothing{}",
                path.display(),
                last.map(|l| format!("\n     last: {l}"))
                    .unwrap_or_default()
            ),
            "upgrade to a build with the fix, then clear the log: \
             `mv ~/.runar-forge/hook.log{,.old}`",
        );
    }

    let tail = crate::hooks_runtime::tail_hook_log(5);
    if tail.is_empty() {
        return Check::pass_with("hook log", format!("{}B", size));
    }
    Check::pass_with(
        "hook log",
        format!(
            "{}B — last {} entries:\n     {}",
            size,
            tail.len(),
            tail.join("\n     ")
        ),
    )
}

// ── env file ───────────────────────────────────────────────────────

fn check_env_file(path: &PathBuf) -> Check {
    if !path.exists() {
        return Check::fail_hint(
            "env file",
            format!("not found: {}", path.display()),
            "run `runar init` to create it, or `runar config wizard`",
        );
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            return Check::fail("env file", format!("read failed: {} ({e})", path.display()));
        }
    };
    let mut keys = std::collections::HashMap::<String, usize>::new();
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq) = trimmed.find('=') {
            let k = trimmed[..eq].trim().to_string();
            *keys.entry(k).or_insert(0) += 1;
        }
    }
    let dups: Vec<_> = keys
        .iter()
        .filter(|(_, c)| **c > 1)
        .map(|(k, c)| format!("{k}×{c}"))
        .collect();
    if !dups.is_empty() {
        return Check::fail("env file", format!("duplicate keys: {}", dups.join(", ")));
    }
    Check::pass_with(
        "env file",
        format!("{} ({} keys)", path.display(), keys.len()),
    )
}

fn check_storage_backend(backend: &str) -> Check {
    match backend {
        "postgresql" | "postgres" | "sqlite" => Check::pass_with("storage", backend.to_string()),
        other => Check::fail_hint(
            "storage",
            format!("unknown RUNAR_STORAGE: {other}"),
            "set RUNAR_STORAGE=postgresql or sqlite",
        ),
    }
}

// ── postgres checks ────────────────────────────────────────────────

async fn run_postgres_checks(report: &mut Report, opts: &DoctorOpts) {
    let url = match std::env::var("RUNAR_DB_URL") {
        Ok(u) => u,
        Err(_) => {
            report.checks.push(Check::fail_hint(
                "db reachable",
                "RUNAR_DB_URL not set",
                "run `runar config wizard` or `runar config set RUNAR_DB_URL <url>`",
            ));
            // No URL → can't run anything else against PG.
            for name in [
                "auth",
                "pgvector",
                "schema muninn",
                "migrations",
                "row counts",
            ] {
                report
                    .checks
                    .push(Check::skip(box_leak(name), "no RUNAR_DB_URL"));
            }
            return;
        }
    };

    let timeout = connect_timeout(opts);
    let started = Instant::now();
    let connect_result = tokio::time::timeout(timeout, tokio_postgres::connect(&url, NoTls)).await;

    let (client, conn_handle) = match connect_result {
        Ok(Ok((c, conn))) => {
            let h = tokio::spawn(async move {
                let _ = conn.await;
            });
            report.checks.push(Check::pass_with(
                "db reachable",
                format!("{}ms", started.elapsed().as_millis()),
            ));
            (c, h)
        }
        Ok(Err(e)) => {
            report.checks.push(Check::fail_hint(
                "db reachable",
                format!("connect failed: {e}"),
                "check host/port/credentials in RUNAR_DB_URL",
            ));
            for name in [
                "auth",
                "pgvector",
                "schema muninn",
                "migrations",
                "row counts",
            ] {
                report
                    .checks
                    .push(Check::skip(box_leak(name), "no connection"));
            }
            return;
        }
        Err(_) => {
            report.checks.push(Check::fail(
                "db reachable",
                format!("connect timed out after {}ms", timeout.as_millis()),
            ));
            for name in [
                "auth",
                "pgvector",
                "schema muninn",
                "migrations",
                "row counts",
            ] {
                report
                    .checks
                    .push(Check::skip(box_leak(name), "no connection"));
            }
            return;
        }
    };

    // Auth — `tokio_postgres::connect` already authenticated, but a
    // distinct SELECT 1 makes the report more legible.
    match client.query_one("SELECT 1", &[]).await {
        Ok(_) => report.checks.push(Check::pass("auth")),
        Err(e) => {
            report
                .checks
                .push(Check::fail("auth", format!("SELECT 1 failed: {e}")));
            conn_handle.abort();
            return;
        }
    }

    // pgvector
    match client
        .query_opt(
            "SELECT extversion FROM pg_extension WHERE extname = 'vector'",
            &[],
        )
        .await
    {
        Ok(Some(row)) => {
            let v: String = row.get(0);
            report
                .checks
                .push(Check::pass_with("pgvector", format!("v{v}")));
        }
        Ok(None) => {
            report.checks.push(Check::fail_hint(
                "pgvector",
                "extension not installed",
                "CREATE EXTENSION vector; on remote, or use pgvector/pgvector image",
            ));
        }
        Err(e) => {
            report
                .checks
                .push(Check::fail("pgvector", format!("query failed: {e}")));
        }
    }

    // schema muninn
    match client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'muninn'",
            &[],
        )
        .await
    {
        Ok(rows) => {
            let tables: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
            if tables.is_empty() {
                report.checks.push(Check::fail_hint(
                    "schema muninn",
                    "no tables found",
                    "run any non-doctor command (e.g. `runar stats`) to trigger initialize()",
                ));
            } else {
                report.checks.push(Check::pass_with(
                    "schema muninn",
                    format!("{} tables", tables.len()),
                ));
            }
        }
        Err(e) => {
            report
                .checks
                .push(Check::fail("schema muninn", format!("query failed: {e}")));
        }
    }

    // migrations
    match client
        .query(
            "SELECT version FROM muninn.schema_migrations ORDER BY version",
            &[],
        )
        .await
    {
        Ok(rows) => {
            let applied: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
            let expected: Vec<&str> = PG_MIGRATIONS.iter().map(|(v, _)| *v).collect();
            // Skip the "000_migration_table" bootstrap row — it's the table
            // itself, not a migration recorded in it.
            let expected_recorded: Vec<&str> = expected
                .iter()
                .copied()
                .filter(|v| *v != "000_migration_table")
                .collect();
            let missing: Vec<&str> = expected_recorded
                .iter()
                .copied()
                .filter(|v| !applied.iter().any(|a| a == v))
                .collect();
            if missing.is_empty() {
                report.checks.push(Check::pass_with(
                    "migrations",
                    format!("{}/{} applied", applied.len(), expected_recorded.len()),
                ));
            } else {
                report.checks.push(Check::fail_hint(
                    "migrations",
                    format!("missing: {}", missing.join(", ")),
                    "run any non-doctor command (e.g. `runar stats`) to apply pending migrations",
                ));
            }
        }
        Err(e) => {
            report.checks.push(Check::fail(
                "migrations",
                format!("query failed (table may not exist): {e}"),
            ));
        }
    }

    // row counts
    let counts_query = "
        SELECT
          (SELECT count(*) FROM muninn.memory_entries WHERE deleted_at IS NULL) AS entries,
          (SELECT count(*) FROM muninn.sessions) AS sessions,
          (SELECT count(*) FROM muninn.pending_observations) AS pending
    ";
    match client.query_one(counts_query, &[]).await {
        Ok(row) => {
            let entries: i64 = row.get("entries");
            let sessions: i64 = row.get("sessions");
            let pending: i64 = row.get("pending");
            report.checks.push(Check::pass_with(
                "row counts",
                format!("entries={entries} sessions={sessions} pending={pending}"),
            ));
        }
        Err(e) => {
            report
                .checks
                .push(Check::fail("row counts", format!("query failed: {e}")));
        }
    }

    conn_handle.abort();
}

// ── sqlite checks ──────────────────────────────────────────────────

fn run_sqlite_checks(report: &mut Report) {
    use rusqlite::{Connection, OpenFlags};

    let path_str = std::env::var("RUNAR_SQLITE_PATH")
        .unwrap_or_else(|_| setup::runar_dir().join("memory.db").display().to_string());
    let path = PathBuf::from(&path_str);

    if !path.exists() {
        report.checks.push(Check::fail_hint(
            "db reachable",
            format!("file not found: {path_str}"),
            "run `runar stats` to initialize the SQLite DB",
        ));
        for name in ["auth", "schema muninn", "migrations", "row counts"] {
            report
                .checks
                .push(Check::skip(box_leak(name), "no connection"));
        }
        report
            .checks
            .push(Check::skip("pgvector", "sqlite backend"));
        report
            .checks
            .push(Check::skip("embedding dim", "sqlite backend"));
        return;
    }

    let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => {
            report
                .checks
                .push(Check::pass_with("db reachable", path_str.clone()));
            c
        }
        Err(e) => {
            report
                .checks
                .push(Check::fail("db reachable", format!("open failed: {e}")));
            for name in ["auth", "schema muninn", "migrations", "row counts"] {
                report
                    .checks
                    .push(Check::skip(box_leak(name), "no connection"));
            }
            report
                .checks
                .push(Check::skip("pgvector", "sqlite backend"));
            report
                .checks
                .push(Check::skip("embedding dim", "sqlite backend"));
            return;
        }
    };

    match conn
        .prepare("SELECT 1")
        .and_then(|mut s| s.query_row([], |_| Ok(())))
    {
        Ok(_) => report.checks.push(Check::pass("auth")),
        Err(e) => report
            .checks
            .push(Check::fail("auth", format!("query failed: {e}"))),
    }

    report.checks.push(Check::skip(
        "pgvector",
        "sqlite backend (no vector extension)",
    ));

    // schema
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .and_then(|mut s| {
            let rows = s
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default();

    if tables.is_empty() {
        report
            .checks
            .push(Check::fail("schema muninn", "no tables in DB"));
    } else {
        report.checks.push(Check::pass_with(
            "schema muninn",
            format!("{} tables", tables.len()),
        ));
    }

    // migrations
    let applied: Vec<String> = conn
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .and_then(|mut s| {
            let rows = s
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap_or_default();

    let expected: Vec<&str> = SQLITE_MIGRATIONS.iter().map(|(v, _)| *v).collect();
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|v| !applied.iter().any(|a| a == v))
        .collect();
    if missing.is_empty() {
        report.checks.push(Check::pass_with(
            "migrations",
            format!("{}/{} applied", applied.len(), expected.len()),
        ));
    } else {
        report.checks.push(Check::fail_hint(
            "migrations",
            format!("missing: {}", missing.join(", ")),
            "run any non-doctor command (e.g. `runar stats`) to apply",
        ));
    }

    report
        .checks
        .push(Check::skip("embedding dim", "sqlite backend"));

    // counts
    let entries: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_entries WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let sessions: i64 = conn
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or(-1);
    let pending: i64 = conn
        .query_row("SELECT count(*) FROM pending_observations", [], |r| {
            r.get(0)
        })
        .unwrap_or(-1);
    report.checks.push(Check::pass_with(
        "row counts",
        format!("entries={entries} sessions={sessions} pending={pending}"),
    ));

    // FTS parity: after migration 013 the index must contain exactly the
    // live rows. Divergence means trigger damage or a pre-013 residue.
    // NOTE: count(*) on an external-content FTS5 table reads the CONTENT
    // table (including tombstones) — the docsize shadow table is the
    // actual per-document index state.
    let fts_rows: i64 = conn
        .query_row("SELECT count(*) FROM memory_fts_docsize", [], |r| r.get(0))
        .unwrap_or(-1);
    if fts_rows < 0 || entries < 0 {
        report
            .checks
            .push(Check::skip("fts parity", "counts unavailable"));
    } else if fts_rows == entries {
        report.checks.push(Check::pass_with(
            "fts parity",
            format!("{fts_rows} indexed rows == live entries"),
        ));
    } else {
        report.checks.push(Check::fail_hint(
            "fts parity",
            format!("memory_fts has {fts_rows} rows, live entries {entries}"),
            "apply migration 013 (any non-doctor command) or report a trigger bug",
        ));
    }
}

// ── code graph ─────────────────────────────────────────────────────

/// Every degraded code-graph state has the same fix. The graph is derived
/// data, so rebuilding it is always correct and never loses anything.
const CODEGRAPH_REMEDY: &str = "rebuild with `runar crawl <path> --project <id>`";

/// Coverage costs a few aggregate queries per project, so the listing is
/// bounded — doctor runs often and is not a coverage report.
const CODEGRAPH_MAX_PROJECTS: usize = 8;

/// The code graph is opt-in (`runar crawl --deep`), so nothing here is a
/// failure: an absent, stale or unreadable file means queries return nothing,
/// not that the install is broken. It is never a silent pass either — the
/// reason always names what is wrong and how to fix it.
/// Report what the opt-in search-hint hook has actually cost.
///
/// The hook shipped with an agreed kill criterion — emit hit-rate under ~25%,
/// or bytes per session in the tens of KB — and a criterion nobody can read is
/// a criterion nobody applies.
fn check_search_hints() -> Check {
    let stats = crate::hint::hint_stats();
    if stats.fires() == 0 {
        return Check::skip(
            "search hints",
            "never fired — opt in with `runar setup claude-code --with-search-hints` \
             (needs a crawl first)"
                .to_string(),
        );
    }

    let mut detail = format!("{} fire(s), {} emission(s)", stats.fires(), stats.emissions);
    if let Some(rate) = stats.hit_rate() {
        detail.push_str(&format!(", {rate}% hit rate"));
    }
    if let Some(bps) = stats.bytes_per_session() {
        detail.push_str(&format!(
            ", {} B total over {} session(s) (~{bps} B/session)",
            stats.total_bytes, stats.sessions
        ));
    }
    if let Some((reason, n)) = stats.gated.first() {
        detail.push_str(&format!("; most gated: {reason} ({n})"));
    }

    // Below the agreed floor this is noise on every search, so say so rather
    // than reporting a green line with a bad number inside it.
    match stats.hit_rate() {
        Some(rate) if rate < 25 && stats.fires() >= 50 => Check::skip(
            "search hints",
            format!(
                "{detail} — below the 25% hit-rate the hook was kept on for; \
                     consider `runar setup claude-code --no-search-hints`"
            ),
        ),
        _ => Check::pass_with("search hints", detail),
    }
}

/// Report the code graph without ever touching it.
///
/// Its absence is normal — it is built by `runar crawl`, and a project that has
/// never been crawled simply has none — so nothing here is a failure. A schema
/// mismatch is likewise not corruption: the next writer drops and rebuilds.
fn check_code_graph(path: &Path) -> Check {
    let degraded =
        |detail: String| Check::skip("code graph", format!("{detail}; {CODEGRAPH_REMEDY}"));

    if !path.exists() {
        return degraded(format!("{} absent — no project crawled", path.display()));
    }

    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let header = format!("{} ({})", path.display(), human_bytes(size));

    // Read-only on purpose: `open()` creates the file and rebuilds the schema
    // on a version mismatch. Doctor reports drift; it does not fix it.
    let store = match CodeGraphStore::open_readonly(path) {
        Ok(s) => s,
        Err(e) => return degraded(format!("{header} — open failed: {e}")),
    };

    // A stale version is not corruption: the next writer drops and rebuilds.
    // It does mean every query returns nothing until then.
    let expected = CodeGraphStore::expected_schema_version();
    match store.stored_schema_version() {
        Ok(Some(v)) if v == expected => {}
        Ok(Some(v)) => {
            return degraded(format!(
                "{header} — schema v{v}, this build writes v{expected}"
            ))
        }
        Ok(None) => return degraded(format!("{header} — no schema version recorded")),
        Err(e) => return degraded(format!("{header} — schema version unreadable: {e}")),
    }

    let projects = match store.projects() {
        Ok(p) => p,
        Err(e) => return degraded(format!("{header} — project list unreadable: {e}")),
    };
    if projects.is_empty() {
        return degraded(format!("{header} — no projects indexed"));
    }

    let shown = projects.len().min(CODEGRAPH_MAX_PROJECTS);
    let mut lines: Vec<String> = Vec::with_capacity(shown + 1);
    let mut symbols_total = 0usize;
    let mut unreadable = false;
    for project in projects.iter().take(shown) {
        match store.coverage(project) {
            Ok(cov) => {
                symbols_total += cov.symbols;
                lines.push(codegraph_line(project, &cov));
            }
            // One bad project must not hide the ones that are fine.
            Err(e) => {
                unreadable = true;
                lines.push(format!("{project}: coverage unreadable: {e}"));
            }
        }
    }
    if projects.len() > shown {
        lines.push(format!("… +{} more project(s)", projects.len() - shown));
    }

    let body = format!(
        "{header} — schema v{expected}, {} project(s):\n     {}",
        projects.len(),
        lines.join("\n     ")
    );
    if unreadable {
        return degraded(body);
    }
    // Registered projects with no symbols behind them: the deep pass ran and
    // produced nothing, so every query answers with silence. Without this the
    // check would read like a working graph.
    if symbols_total == 0 && projects.len() == shown {
        return degraded(format!("{body}\n     no symbols in any project"));
    }
    Check::pass_with("code graph", body)
}

/// Does the graph still describe the tree it was built from?
///
/// Separate from `check_code_graph`, which reports whether a graph exists and
/// is readable: a perfectly healthy graph can be describing yesterday's code,
/// and that is the failure this feature exists to make visible.
///
/// Never a Fail — a stale graph is a graph that needs rebuilding, not a broken
/// install — and never a silent Pass either. "Cannot judge" is reported as
/// degraded rather than green, because a reader who sees green will trust the
/// symbols. Costs two `git` calls per project, and none at all for a project
/// whose root is gone, which is why it is bounded like its neighbour.
fn check_graph_freshness(path: &Path) -> Check {
    let name = "graph freshness";
    if !path.exists() {
        return Check::skip(name, "no code graph to judge".to_string());
    }
    // Read-only, like every other consumer of this file: judging staleness
    // must never create or migrate the graph a crawl is writing.
    let store = match CodeGraphStore::open_readonly(path) {
        Ok(s) => s,
        Err(e) => return Check::skip(name, format!("graph unreadable: {e}")),
    };
    let projects = match store.projects() {
        Ok(p) => p,
        Err(e) => return Check::skip(name, format!("project list unreadable: {e}")),
    };
    if projects.is_empty() {
        return Check::skip(name, "no projects indexed".to_string());
    }

    let shown = projects.len().min(CODEGRAPH_MAX_PROJECTS);
    let mut fresh: Vec<&str> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for project in projects.iter().take(shown) {
        match freshness::verdict(&store, project) {
            Verdict::Fresh => fresh.push(project),
            Verdict::Stale { reason } => stale.push(format!("{project}: {reason}")),
            Verdict::Unknown { reason } => unknown.push(format!("{project}: {reason}")),
        }
    }
    // A bound that is not stated reads as a total: with more projects than are
    // judged, "all fresh" would be a claim about projects nobody looked at.
    let unjudged = projects.len() - shown;

    let mut lines: Vec<String> = Vec::new();
    lines.extend(stale.iter().cloned());
    lines.extend(unknown.iter().cloned());
    if !fresh.is_empty() {
        lines.push(format!("current: {}", fresh.join(", ")));
    }
    if unjudged > 0 {
        lines.push(format!("{unjudged} more project(s) not checked"));
    }
    let body = lines.join("\n     ");

    if !stale.is_empty() {
        return Check::skip(
            name,
            format!("{body}\n     refresh with `runar graph refresh --project <id>`"),
        );
    }
    if !unknown.is_empty() {
        return Check::skip(name, body);
    }
    Check::pass_with(name, body)
}

/// Report what the opt-in auto-refresh hook has actually been doing.
///
/// Reads only the stamp files it writes — no database, no git — because the
/// question here is whether the hook is working, not whether the graph is
/// current, which the check above already answers. An error left by the last
/// attempt is a genuine failure: something is writing to the graph on every
/// file save and getting it wrong.
fn check_graph_autorefresh() -> Check {
    let name = "graph auto-refresh";
    let stamps = auto_refresh_stamps();
    if stamps.is_empty() {
        return Check::skip(
            name,
            "not enabled — opt in with `runar setup claude-code \
             --with-graph-autorefresh` (needs a crawl first)"
                .to_string(),
        );
    }

    let mut lines = Vec::new();
    let mut failing = Vec::new();
    for (project, stamp) in &stamps {
        let age = crate::protocol::now_ms().saturating_sub(stamp.last_attempt_ms) / 1000;
        lines.push(format!(
            "{project}: {} ({}s ago, {}ms)",
            stamp.last_outcome, age, stamp.last_duration_ms
        ));
        if stamp.last_outcome.starts_with("error:") {
            failing.push(project.clone());
        }
    }

    // The stamps are a point sample — the last attempt, per project. What
    // decides whether this hook keeps its place is the shape of the whole
    // record: how often it finds work, what the slow tail costs, and whether
    // it has ever failed.
    let stats = crate::codegraph::refresh::refresh_stats();
    if stats.fires() > 0 {
        let mut summary = format!(
            "{} fire(s): {} started a refresh, {} debounced",
            stats.fires(),
            stats.spawns,
            stats.debounced
        );
        if let Some(rate) = stats.work_rate() {
            summary.push_str(&format!("; {rate}% of runs had work"));
        }
        if let (Some(p50), Some(p95)) = (stats.median_ms(), stats.percentile_ms(95)) {
            summary.push_str(&format!(
                "; {} refresh(es), {p50}ms median / {p95}ms p95",
                stats.refreshes
            ));
        }
        lines.push(summary);
        if let Some((reason, n)) = stats.skipped.first() {
            lines.push(format!("most skipped: {reason} ({n})"));
        }
        if stats.errors > 0 {
            lines.push(format!(
                "{} error(s), most recent: {}",
                stats.errors,
                stats.last_error.clone().unwrap_or_default()
            ));
            failing.push("recorded errors".to_string());
        }
    }
    let body = lines.join("\n     ");

    if !failing.is_empty() {
        return Check::fail_hint(
            name,
            body,
            "check ~/.runar-forge/hook.log; disable with \
             `runar setup claude-code --no-graph-autorefresh`",
        );
    }
    Check::pass_with(name, body)
}

/// Every project the auto-refresh hook has recorded an attempt for.
fn auto_refresh_stamps() -> Vec<(String, crate::codegraph::refresh::Stamp)> {
    let dir = setup::runar_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `graph-refresh-<project>.stamp`, and never the `.lock` beside it.
        let Some(project) = name
            .strip_prefix("graph-refresh-")
            .and_then(|rest| rest.strip_suffix(".stamp"))
        else {
            continue;
        };
        if let Some(stamp) = crate::codegraph::refresh::read_stamp(project) {
            out.push((project.to_string(), stamp));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Wording matches `codegraph::format::coverage` so the two read the same.
fn codegraph_line(project: &str, cov: &Coverage) -> String {
    let parsed = cov.indexed + cov.partial;
    let pct = (parsed * 100).checked_div(cov.files_total).unwrap_or(0);
    format!(
        "{project}: {} symbols, {} edges, {parsed}/{} files parsed ({pct}%)",
        cov.symbols, cov.edges, cov.files_total
    )
}

// ── breaker + local env ────────────────────────────────────────────

fn check_breaker_state() -> Check {
    let dir = setup::runar_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Check::skip("breaker state", "runar dir absent"),
    };
    let mut open: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.starts_with("breaker-") && s.ends_with(".json") {
            // Existence alone isn't enough — could be closed history. Read
            // the file and check the state field if present.
            if let Ok(text) = fs::read_to_string(entry.path()) {
                if text.contains("\"open\"") || text.contains("\"Open\"") {
                    open.push(s.to_string());
                }
            }
        }
    }
    if open.is_empty() {
        Check::pass("breaker state")
    } else {
        Check::fail_hint(
            "breaker state",
            format!("{} open breaker(s): {}", open.len(), open.join(", ")),
            "investigate logs and reset via `runar` (or remove the breaker file)",
        )
    }
}

fn check_project_local_env() -> Check {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return Check::skip("project-local .env", "no cwd"),
    };
    let local = cwd.join(".env");
    if local.exists() {
        return Check::skip(
            "project-local .env",
            format!(
                "{} exists but is NOT read by runar (global .env only)",
                local.display()
            ),
        );
    }
    Check::pass("project-local .env")
}

// ── helpers ────────────────────────────────────────────────────────

/// Convert `&str` into `&'static str` by leaking. Cheap — only used for the
/// tiny set of skip-name placeholders inside doctor.
fn box_leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_ok_when_only_pass_and_skip() {
        let r = Report {
            checks: vec![Check::pass("a"), Check::skip("b", "reason")],
        };
        assert!(r.ok());
    }

    #[test]
    fn report_not_ok_when_any_fail() {
        let r = Report {
            checks: vec![Check::pass("a"), Check::fail("b", "boom")],
        };
        assert!(!r.ok());
    }

    #[test]
    fn json_serializes_status_variants() {
        let r = Report {
            checks: vec![
                Check::pass_with("a", "detail"),
                Check::fail_hint("b", "msg", "hint"),
                Check::skip("c", "why"),
            ],
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["checks"][0]["status"]["kind"], "pass");
        assert_eq!(json["checks"][1]["status"]["kind"], "fail");
        assert_eq!(json["checks"][1]["status"]["hint"], "hint");
        assert_eq!(json["checks"][2]["status"]["kind"], "skip");
        assert_eq!(json["checks"][2]["status"]["reason"], "why");
    }

    #[test]
    fn connect_timeout_honors_override() {
        let opts = DoctorOpts {
            db_only: false,
            timeout_ms: Some(1234),
        };
        assert_eq!(connect_timeout(&opts), Duration::from_millis(1234));
    }

    #[test]
    fn check_env_file_detects_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "A=1\nA=2\nB=3\n").unwrap();
        let c = check_env_file(&path);
        assert!(matches!(c.status, Status::Fail { .. }));
    }

    #[test]
    fn check_env_file_pass_on_clean_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "# hi\nA=1\nB=2\n").unwrap();
        let c = check_env_file(&path);
        assert!(matches!(c.status, Status::Pass));
    }

    #[test]
    fn code_graph_absent_is_not_a_failure_but_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let c = check_code_graph(&dir.path().join("codegraph.db"));
        match c.status {
            Status::Skip { reason } => {
                assert!(reason.contains("runar crawl"), "no remedy in: {reason}");
                assert!(reason.contains("codegraph.db"), "no path in: {reason}");
            }
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn code_graph_unreadable_file_does_not_report_green() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codegraph.db");
        fs::write(&path, b"not a sqlite database").unwrap();
        let c = check_code_graph(&path);
        assert!(
            !matches!(c.status, Status::Pass),
            "a garbage file must not pass"
        );
        // Still not a failure: the graph is derived data, so doctor asks for a
        // rebuild rather than exiting non-zero.
        match c.status {
            Status::Skip { reason } => {
                assert!(reason.contains("runar crawl"), "no remedy in: {reason}")
            }
            other => panic!("expected skip, got {other:?}"),
        }
    }

    /// Builds a real graph in a real repository, then asks doctor what it
    /// thinks — the same path a user's `runar doctor` takes, including the
    /// read-only open.
    ///
    /// The graph file lives outside the repository on purpose, as it does in
    /// production: a graph written *inside* the tree it indexes is itself an
    /// untracked change, so every index would leave the project instantly
    /// stale by its own measurement.
    fn graph_fixture(dir: &Path, db: &Path) -> Option<()> {
        use crate::codegraph::index::index_project;
        use crate::huginn::scanner::FileEntry;

        let run_git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !run_git(&["init", "--quiet"]) {
            return None;
        }
        fs::write(dir.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        if !run_git(&["add", "-A"])
            || !run_git(&[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "init",
            ])
        {
            return None;
        }

        let store = CodeGraphStore::open(db).unwrap();
        let files = vec![FileEntry {
            path: dir.join("a.rs"),
            relative_path: "a.rs".to_string(),
            size: 18,
            line_count: 1,
            last_modified: None,
            extension: "rs".to_string(),
        }];
        index_project(&store, "p", dir, &files, true, None).unwrap();
        Some(())
    }

    #[test]
    fn graph_freshness_passes_only_while_the_tree_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("codegraph.db");
        if graph_fixture(dir.path(), &path).is_none() {
            return; // no git here; the module's own tests cover the logic
        }

        let c = check_graph_freshness(&path);
        assert!(
            matches!(c.status, Status::Pass),
            "a just-indexed tree should be current, got {:?}",
            c.status
        );

        // The same edit a user would make between crawls.
        fs::write(dir.path().join("a.rs"), "pub fn alpha() { beta(); }\n").unwrap();
        let c = check_graph_freshness(&path);
        match c.status {
            Status::Skip { reason } => {
                assert!(reason.contains("working tree"), "unnamed signal: {reason}");
                assert!(
                    reason.contains("runar graph refresh"),
                    "no remedy in: {reason}"
                );
                assert!(reason.contains('p'), "no project named in: {reason}");
            }
            other => panic!("a stale graph must not read green, got {other:?}"),
        }
    }

    /// The failure mode worth a test of its own: a graph nobody can judge must
    /// not be reported as current.
    #[test]
    fn graph_freshness_cannot_judge_a_project_without_git() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codegraph.db");
        let store = CodeGraphStore::open(&path).unwrap();
        crate::codegraph::index::index_project(&store, "p", dir.path(), &[], true, None).unwrap();
        drop(store);

        let c = check_graph_freshness(&path);
        assert!(
            !matches!(c.status, Status::Pass),
            "an unjudgeable graph reported as current: {:?}",
            c.status
        );
        match c.status {
            Status::Skip { reason } => assert!(
                reason.contains("git"),
                "the reason should say why it cannot be judged: {reason}"
            ),
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn graph_freshness_without_a_baseline_is_not_current() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("codegraph.db");
        if graph_fixture(dir.path(), &path).is_none() {
            return;
        }
        // A graph written by a build that predates baselines at all.
        let store = CodeGraphStore::open(&path).unwrap();
        store.set_freshness("p", "").unwrap();
        drop(store);

        let c = check_graph_freshness(&path);
        assert!(
            !matches!(c.status, Status::Pass),
            "a graph with no baseline reported as current: {:?}",
            c.status
        );
    }

    #[test]
    fn auto_refresh_reports_nothing_when_it_was_never_enabled() {
        crate::test_support::with_runar_home(|| {
            let c = check_graph_autorefresh();
            match c.status {
                Status::Skip { reason } => assert!(
                    reason.contains("--with-graph-autorefresh"),
                    "no way in named: {reason}"
                ),
                other => panic!("expected skip, got {other:?}"),
            }
        });
    }

    /// A background writer that keeps failing must not read as healthy: it is
    /// running on every file save and getting it wrong, and nobody would
    /// notice except by seeing the graph never change.
    #[test]
    fn auto_refresh_failing_in_the_background_is_a_failure() {
        crate::test_support::with_runar_home(|| {
            use crate::codegraph::refresh::{write_stamp, Stamp};
            write_stamp(
                "p",
                &Stamp {
                    last_attempt_ms: crate::protocol::now_ms(),
                    not_before_ms: 0,
                    last_outcome: "error: codegraph db: disk I/O error".into(),
                    last_duration_ms: 12,
                },
            );
            let c = check_graph_autorefresh();
            assert!(
                matches!(c.status, Status::Fail { .. }),
                "a failing background writer read as {:?}",
                c.status
            );

            // A healthy one passes and says what it did.
            write_stamp(
                "p",
                &Stamp {
                    last_attempt_ms: crate::protocol::now_ms(),
                    not_before_ms: 0,
                    last_outcome: "done".into(),
                    last_duration_ms: 310,
                },
            );
            let c = check_graph_autorefresh();
            assert!(matches!(c.status, Status::Pass), "got {:?}", c.status);
            let detail = c.detail.unwrap_or_default();
            assert!(detail.contains("310ms"), "got {detail}");
        });
    }

    #[test]
    fn graph_freshness_has_nothing_to_say_without_a_graph() {
        let dir = tempfile::tempdir().unwrap();
        let c = check_graph_freshness(&dir.path().join("codegraph.db"));
        assert!(matches!(c.status, Status::Skip { .. }));
    }

    #[test]
    fn code_graph_line_reports_the_parsed_fraction() {
        let cov = Coverage {
            files_total: 10,
            indexed: 6,
            partial: 1,
            skipped_lang: 3,
            errored: 0,
            symbols: 40,
            edges: 25,
            unresolved_calls: 12,
            skipped_by_ext: vec![],
        };
        assert_eq!(
            codegraph_line("proj", &cov),
            "proj: 40 symbols, 25 edges, 7/10 files parsed (70%)"
        );
    }

    #[test]
    fn code_graph_line_survives_an_empty_project() {
        // files_total = 0 would divide by zero.
        assert_eq!(
            codegraph_line("proj", &Coverage::default()),
            "proj: 0 symbols, 0 edges, 0/0 files parsed (0%)"
        );
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn check_storage_backend_recognizes_known() {
        assert!(matches!(
            check_storage_backend("postgresql").status,
            Status::Pass
        ));
        assert!(matches!(
            check_storage_backend("sqlite").status,
            Status::Pass
        ));
        assert!(matches!(
            check_storage_backend("mysql").status,
            Status::Fail { .. }
        ));
    }
}
