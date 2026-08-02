use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

// Import the library crate rather than re-declaring every module, so that
// items used only by MCP tool dispatch aren't flagged as dead from the
// binary's perspective.
use runar_muninn::{
    breaker, codegraph, config_cmd, curator, doctor, embedding, extract, hint, hooks_runtime,
    huginn, librarian, maintenance, mcp, protocol, redact, setup, storage, summarizer,
    sync as sync_cmd, text, types, update as update_cmd, wizard,
};

use text::char_prefix;

use codegraph::store::{CodeGraphStore, Direction};
use librarian::MemoryLibrarian;
use storage::postgres::PostgresAdapter;
use storage::sqlite::SqliteAdapter;
use storage::MemoryStorage;

#[derive(Parser)]
#[command(
    name = "runar",
    version,
    about = "RunarForge — Persistent memory for AI coding tools"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start Muninn MCP server (stdio transport)
    McpMuninn,

    /// Search memory entries
    Search {
        /// Natural language query
        query: String,
        /// Maximum results
        #[arg(short, long, default_value = "10")]
        limit: usize,
        /// Project to search (auto-detected from git remote / directory
        /// if omitted; entries live in their project's namespace)
        #[arg(short, long)]
        project: Option<String>,
    },

    /// Show memory system stats (all namespaces by default)
    Stats {
        /// Scope stats to one project (its namespace)
        #[arg(short, long)]
        project: Option<String>,
    },

    /// Crawl a project directory
    Crawl {
        /// Path to project root (defaults to current directory)
        #[arg(default_value = ".")]
        path: String,
        /// Project identifier
        #[arg(short, long)]
        project: String,
        /// Crawl mode: auto | full | incremental (default: auto)
        #[arg(long, default_value = "auto")]
        mode: String,
        /// Deprecated no-op: the code graph is built by default.
        #[arg(long, hide = true)]
        deep: bool,
        /// Skip the symbol-level code graph (definitions, calls, metrics).
        #[arg(long)]
        no_deep: bool,
        /// Restrict the crawl to a subdirectory. Cross-file patterns, the
        /// architecture summary and the code graph are whole-project, so a
        /// focused crawl leaves all three as the last unfocused crawl built
        /// them rather than rewriting them from a fragment.
        #[arg(long)]
        focus: Option<String>,
    },

    /// Initialize RunarForge configuration
    Init {
        /// Storage backend (used in non-interactive mode)
        #[arg(long, default_value = "postgresql")]
        storage: String,
        /// Launch the interactive setup wizard
        #[arg(short, long)]
        interactive: bool,
    },

    /// Run the memory-quality benchmark against a project
    Benchmark {
        /// Project identifier to benchmark
        #[arg(short, long)]
        project: String,
        /// Question set: quick (9) or full (30)
        #[arg(long, default_value = "quick")]
        mode: String,
    },

    /// Configure an AI tool to use RunarForge
    Setup {
        /// Tool name: claude-code | vscode | opencode | codex | cursor | windsurf
        tool: String,
        /// Project ID (default: auto-detect from git remote or directory name)
        #[arg(short, long)]
        project: Option<String>,
        /// Deprecated no-op: auto-capture is now installed by default.
        #[arg(long, hide = true)]
        with_auto_capture: bool,
        /// Skip the auto-capture queue hooks (enqueue + SessionEnd
        /// summarize). Persisted to ~/.runar-forge/.env so later re-runs
        /// keep the choice.
        #[arg(long)]
        no_auto_capture: bool,
        /// Install the opt-in PreToolUse search-hint hook: before a Grep or
        /// Glob, name the definitions in the code graph matching it. Requires
        /// a crawl run with --deep. Off by default.
        #[arg(long)]
        with_search_hints: bool,
        /// Remove the search-hint hook if it is installed.
        #[arg(long)]
        no_search_hints: bool,
        /// Run `runar config wizard` first to (re)configure storage backend.
        /// Phase 5.5 — replaces the manual ".env edit then setup" flow.
        #[arg(long)]
        configure: bool,
        /// Re-point every already-installed project at the current hook
        /// layout. Hooks live per project, so a layout change (like the
        /// v0.9.0 PreToolUse → SessionStart move) otherwise only reaches
        /// the directory you are standing in. Migrates only; never enrolls
        /// a new project, and keeps each project's existing --project id.
        #[arg(long)]
        all_projects: bool,
    },

    // ── Hook-support commands (called by Claude Code hooks) ────────
    /// Print the memory context packet (SessionStart hook payload)
    Context {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        silent: bool,
    },

    /// Nudge Claude Code to save if idle (UserPromptSubmit hook stub)
    Nudge {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        silent: bool,
    },

    /// PreToolUse hook (Grep/Glob): name definitions matching the search
    Hint {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        silent: bool,
    },

    /// Acknowledge a muninn_save call (PostToolUse hook stub)
    SaveAck {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        silent: bool,
    },

    /// Session commands
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },

    /// Save a memory entry directly from the CLI
    Save {
        /// Entry title (5-200 chars)
        title: String,
        /// Entry content
        content: String,
        /// Entry type (default: note)
        #[arg(short = 't', long, default_value = "note")]
        entry_type: String,
        /// Project identifier (optional)
        #[arg(short, long)]
        project: Option<String>,
        /// Comma-separated tags (e.g. "refactor,db,auth")
        #[arg(long)]
        tags: Option<String>,
        /// Stable topic key for upsert semantics (e.g. "auth:approach")
        #[arg(long)]
        topic_key: Option<String>,
    },

    /// Show the latest architecture summary for a project
    Architecture {
        #[arg(short, long)]
        project: String,
    },

    /// Show tech-debt markers found by the crawler
    Techdebt {
        #[arg(short, long)]
        project: String,
        /// Filter by marker type: todo | fixme | hack | xxx | all (default: all)
        #[arg(long, default_value = "all")]
        r#type: String,
    },

    /// Query the symbol-level code graph built by `runar crawl --deep`
    Graph {
        #[command(subcommand)]
        cmd: GraphCmd,
    },

    /// Ask the Curator a question about a project
    Ask {
        /// Natural-language question
        question: String,
        #[arg(short, long)]
        project: Option<String>,
    },

    /// Passive extraction — parse tool output from PostToolUse hook stdin
    /// and auto-save detected insights (bug fixes, decisions, config changes).
    /// Gated behind RUNAR_PASSIVE_LEARNING=true.
    Extract {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        silent: bool,
    },

    /// Enqueue raw PostToolUse payload onto the observation queue for later
    /// summarization by `runar summarize`. Runs on every Edit/Write/Bash hook
    /// (not rule-filtered like `extract`). Dedup + 30-sec SHA256 window.
    Enqueue {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        silent: bool,
    },

    /// SessionEnd hook: claim pending observations for this project, call
    /// the summarizer (Claude Haiku if `ANTHROPIC_API_KEY` is set, otherwise
    /// deterministic heuristic), write synthesized entries via `propose`,
    /// close the active session with a structured summary, and confirm the
    /// queue items. Best-effort — never fails the hook.
    Summarize {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        silent: bool,
        /// Maximum observations to claim per call (default 50)
        #[arg(long, default_value = "50")]
        max: usize,
    },

    /// Produce a multi-section onboarding report for a project
    Onboard {
        #[arg(short, long)]
        project: Option<String>,
        /// Emit JSON instead of rendered Markdown
        #[arg(long)]
        json: bool,
    },

    /// Export memory entries as JSONL (one JSON object per line).
    /// Filter by project and/or type. Writes to stdout or --output file.
    Export {
        /// Only export entries for this project (optional)
        #[arg(short, long)]
        project: Option<String>,
        /// Only export entries of this type (optional)
        #[arg(short = 't', long)]
        entry_type: Option<String>,
        /// Output path. Defaults to stdout.
        #[arg(short, long)]
        output: Option<String>,
        /// Cap on rows exported (safety valve; default 100_000)
        #[arg(long, default_value = "100000")]
        limit: usize,
    },

    /// Import memory entries from a JSONL file (one JSON object per line,
    /// each conforming to MemoryEntry). Existing ids are skipped.
    Import {
        /// Path to JSONL file
        input: String,
    },

    /// Phase 5.4 — run tier graduation + stale eviction for a project.
    /// Promotes entries based on age + access count + verified flag;
    /// soft-deletes archival + low-confidence + zero-access rows older
    /// than `RUNAR_TIER_EVICTION_AGE_DAYS` (default 90d). Verified
    /// entries are never evicted.
    /// Scan stored entries for secret patterns and redact them in place
    Scrub {
        /// Limit to one project namespace (default: all namespaces)
        #[arg(short, long)]
        project: Option<String>,
        /// Report what would change without rewriting anything
        #[arg(long)]
        dry_run: bool,
        /// Max entries to scan
        #[arg(long, default_value = "100000")]
        limit: usize,
    },

    /// Backfill content hashes and collapse exact-duplicate entries
    Dedup {
        /// Limit to one project namespace (default: all namespaces)
        #[arg(short, long)]
        project: Option<String>,
        /// Report clusters without soft-deleting anything
        #[arg(long)]
        dry_run: bool,
        /// Stop after the content-hash backfill pass
        #[arg(long)]
        backfill_only: bool,
    },

    /// Soft-delete captured prompts that were machine payloads, not user
    /// input (IDE selections, task notifications, agent templates). Judged
    /// by the same filter that guards the capture path.
    PurgeNoise {
        /// Limit to one project namespace (default: all namespaces)
        #[arg(short, long)]
        project: Option<String>,
        /// Report matches without soft-deleting anything
        #[arg(long)]
        dry_run: bool,
    },

    Gc {
        /// Project namespace to garbage-collect (auto-detected if omitted)
        #[arg(short, long)]
        project: Option<String>,
        /// Print planned transitions + eviction candidates without mutating
        #[arg(long)]
        dry_run: bool,
        /// Stage 1: soft-delete never-accessed crawl entries older than
        /// --stage1-age-days (reversible until --hard purges them)
        #[arg(long)]
        stage1: bool,
        /// Run stage 1 across every namespace instead of just one project
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "60")]
        stage1_age_days: i64,
        /// Stage 2: PERMANENTLY purge rows soft-deleted more than
        /// --hard-age-days ago (entries + FTS + edges + embeddings)
        #[arg(long)]
        hard: bool,
        #[arg(long, default_value = "30")]
        hard_age_days: i64,
        /// Required confirmation for --hard (no interactive prompt)
        #[arg(long)]
        yes: bool,
    },

    /// Phase 5.5 — manage `~/.runar-forge/.env` (storage backend, DB URL).
    /// Replaces hand-editing with atomic, masked, validated CLI ops.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Self-update the `runar` binary via the release manifest. Atomic
    /// install to `~/.runar-forge/bin/runar`; previous binary preserved
    /// for `--rollback`. Mirrors the `claude update` UX so users never
    /// need to touch `cargo install` again.
    Update {
        /// Print the latest version without downloading
        #[arg(long)]
        check: bool,
        /// Release channel: stable | beta (default: stable)
        #[arg(long, default_value = "stable")]
        channel: String,
        /// Skip the "active Claude Code session" guard
        #[arg(long)]
        force: bool,
        /// Restore the previous binary kept at `runar.previous`
        #[arg(long)]
        rollback: bool,
    },

    /// Phase 5.5 — read-only validation of config + storage. Exits non-zero
    /// on any failure; safe to call from CI or post-deploy hooks.
    Doctor {
        /// Run only DB-side checks (skip env file + breaker)
        #[arg(long)]
        db: bool,
        /// Emit JSON instead of human-readable output
        #[arg(long)]
        json: bool,
        /// Suppress stdout; rely on exit code (use with --json piped to file)
        #[arg(long)]
        quiet: bool,
        /// Override RUNAR_DB_CONNECT_TIMEOUT_MS for this run only
        #[arg(long)]
        timeout_ms: Option<u64>,
    },

    /// Phase 5.6 — hybrid local + remote sync. 5.6.1 ships handshake only.
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },
}

#[derive(Subcommand)]
enum SyncAction {
    /// Validate the local + remote pair (schema versions, embedding dim,
    /// connectivity). Writes `sync_state.initialized_at` on success.
    Init {
        /// Override schema-version mismatch (NOT recommended — fix the
        /// lagging side first).
        #[arg(long)]
        force: bool,
    },

    /// Phase 5.6.2 — drain the local outbox to remote. Idempotent;
    /// safe to re-run.
    Push {
        /// Max outbox rows to claim in this run
        #[arg(long, default_value = "500")]
        limit: usize,
        /// Plan only; do not call the remote
        #[arg(long)]
        dry_run: bool,
    },

    /// Phase 5.6.3 — incremental pull (remote → local). Refuses if
    /// `sync_state.last_pulled_updated_at` is NULL (run `bootstrap`
    /// first).
    Pull {
        /// Max rows per remote query batch
        #[arg(long, default_value = "1000")]
        limit: usize,
        /// Plan only; don't apply
        #[arg(long)]
        dry_run: bool,
        /// Override cursor for this run (ISO-8601, e.g. 2026-04-25T00:00:00Z)
        #[arg(long)]
        since: Option<String>,
    },

    /// Phase 5.6.3 — first-time / full-table pull. Use after
    /// `runar sync init` on an empty local DB.
    Bootstrap {
        /// Scope to a single project namespace
        #[arg(long)]
        project: Option<String>,
        /// Plan only; don't apply
        #[arg(long)]
        dry_run: bool,
        /// Page size for remote scans
        #[arg(long, default_value = "1000")]
        page_size: usize,
        /// Required when local is non-empty (LWW protects newer/verified
        /// rows but the flag confirms intent)
        #[arg(long)]
        yes_i_know: bool,
    },

    /// Phase 5.6.3 — read-only sync health summary
    Status {
        #[arg(long)]
        json: bool,
    },

    /// Phase 5.6.4 — enable background auto-sync (writes
    /// RUNAR_SYNC_AUTO=true to ~/.runar-forge/.env)
    Enable,

    /// Phase 5.6.4 — disable background auto-sync
    Disable,

    /// Phase 5.6.5 — outbox retention sweep. Deletes confirmed rows
    /// older than `RUNAR_SYNC_OUTBOX_RETENTION_DAYS` (default 7).
    /// Pending rows untouched.
    Gc {
        #[arg(long)]
        dry_run: bool,
    },

    /// Repair an outbox damaged by older binaries: release stranded
    /// claims and enqueue the delete ops that soft-deletes failed to
    /// write. Additive — never deletes an entry. Start with --dry-run.
    Repair {
        /// Report what would change; write nothing
        #[arg(long)]
        dry_run: bool,
        /// Max tombstones to enqueue in this run
        #[arg(long, default_value = "1000")]
        limit: usize,
        /// Release every claim, not only stale ones. Only safe when no
        /// push is running — it can yank rows from a live pusher.
        #[arg(long)]
        release_all: bool,
        /// Delete queued rows whose content exceeds the remote's limit.
        /// They can never be pushed. Memory entries are NOT touched.
        #[arg(long)]
        purge_unsendable: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print resolved `.env` path
    Path,

    /// Print all keys (secrets masked unless --unmask)
    Show {
        #[arg(long)]
        unmask: bool,
    },

    /// Print a single key's value (masked unless --unmask)
    Get {
        key: String,
        #[arg(long)]
        unmask: bool,
    },

    /// Set or update a key. Atomic write; preserves comments + ordering
    Set { key: String, value: String },

    /// Remove a key
    Unset { key: String },

    /// Interactive wizard for storage backend + connection details
    Wizard,
}

#[derive(Subcommand)]
enum GraphCmd {
    /// Full-text search over symbol names
    Search {
        /// Name or fragment to look for
        query: String,
        #[arg(short, long)]
        project: String,
        /// Restrict to one kind: function | method | class | interface |
        /// trait | struct | enum | type | const | module
        #[arg(long)]
        label: Option<String>,
        #[arg(long, default_value = "12")]
        limit: usize,
    },

    /// Show one definition with its immediate callers and callees
    Symbol {
        /// Bare name (`open`) or qualified name (`src/store.rs:Store.open`)
        name: String,
        #[arg(short, long)]
        project: String,
    },

    /// Walk the call graph outward from a definition
    Trace {
        /// Bare or qualified name of the definition to start from
        name: String,
        #[arg(short, long)]
        project: String,
        /// Which way to walk: callers | callees
        #[arg(long, default_value = "callers")]
        direction: String,
        /// Hops to follow (max 5)
        #[arg(long, default_value = "2")]
        depth: usize,
        /// Cap on definitions returned
        #[arg(long, default_value = "50")]
        limit: usize,
    },

    /// Coverage for a project's graph, plus the stored session summary
    Status {
        #[arg(short, long)]
        project: String,
    },

    /// Write the code view to one self-contained HTML file
    Export {
        #[arg(short, long)]
        project: String,
        /// Where to write it. Defaults to `<project>-graph.html` here.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Serve the code view on localhost, with live data and a project switcher
    Serve {
        /// Which project to open first. Defaults to the largest graph.
        #[arg(short, long)]
        project: Option<String>,
        /// Port to bind. 0 (the default) asks the OS for a free one.
        #[arg(long, default_value = "0")]
        port: u16,
        /// Print the URL instead of opening a browser.
        #[arg(long)]
        no_open: bool,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    /// Heartbeat the active session (called by PostToolUse hook)
    Ping {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        silent: bool,
    },

    /// List recent sessions
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },
}

fn default_db_path() -> String {
    setup::runar_dir()
        .join("memory.db")
        .to_string_lossy()
        .into_owned()
}

/// Resolve the project id for runtime commands that accept an optional
/// `--project` flag. Precedence: explicit CLI value > git remote > current
/// directory name > literal `"default"`. `setup::detect_project_id` already
/// implements the git-then-folder fallback, so this only layers a last-ditch
/// `"default"` if detection returns `"unknown"` (no git remote AND unreadable
/// cwd — rare). Keeps manual CLI use ergonomic while hook invocations still
/// win because `runar setup claude-code` bakes the explicit pid into every
/// hook command.
fn resolve_project_id(arg: Option<String>) -> String {
    if let Some(p) = arg.filter(|s| !s.is_empty()) {
        return p;
    }
    let detected = setup::detect_project_id();
    if detected == "unknown" {
        "default".into()
    } else {
        detected
    }
}

/// Timeout for `storage.initialize()` and pool acquires. Configurable via
/// `RUNAR_DB_CONNECT_TIMEOUT_MS`. Defaults to 8 s — enough for a cold pool
/// on a healthy PG, short enough that a dead PG doesn't freeze MCP clients
/// (Phase 4.8 item 4.8.17).
fn db_connect_timeout() -> std::time::Duration {
    let ms: u64 = std::env::var("RUNAR_DB_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8_000);
    std::time::Duration::from_millis(ms)
}

/// Hook-path entry: gates `create_librarian()` on the DB circuit breaker.
/// When the breaker is tripped, returns immediately with an explanatory
/// error so the caller can take the protocol-only / no-DB fallback. Logs
/// the short-circuit + any underlying failure to `~/.runar-forge/hook.log`.
async fn create_librarian_for_hook(project_id: &str) -> anyhow::Result<Arc<MemoryLibrarian>> {
    if breaker::is_db_tripped(project_id) {
        anyhow::bail!("db breaker tripped — skipping connect");
    }
    // Apply a budget tighter than the outer hook budget so the breaker
    // bookkeeping always runs to completion before the outer
    // `tokio::time::timeout` cancels us. 80% of the hook budget leaves
    // headroom for protocol-string assembly and writing the response.
    let inner_budget = hooks_runtime::hook_budget().mul_f32(0.8);
    let outcome = tokio::time::timeout(inner_budget, create_librarian()).await;
    match outcome {
        Ok(Ok(l)) => {
            breaker::db_record_success(project_id);
            Ok(l)
        }
        Ok(Err(e)) => {
            breaker::db_record_failure(project_id);
            hooks_runtime::append_hook_log(
                "create_librarian",
                &format!("project={project_id} err={e}"),
            );
            Err(e)
        }
        Err(_) => {
            breaker::db_record_failure(project_id);
            let msg = format!(
                "create_librarian timed out after {}ms (hook budget)",
                inner_budget.as_millis()
            );
            hooks_runtime::append_hook_log(
                "create_librarian",
                &format!("project={project_id} {msg}"),
            );
            anyhow::bail!(msg)
        }
    }
}

async fn create_librarian() -> anyhow::Result<Arc<MemoryLibrarian>> {
    let storage_type = std::env::var("RUNAR_STORAGE").unwrap_or_else(|_| "postgresql".into());
    let namespace = std::env::var("RUNAR_MEMORY_NAMESPACE").unwrap_or_else(|_| "default".into());

    let storage: Arc<dyn MemoryStorage> = match storage_type.as_str() {
        "sqlite" => {
            let db_path = std::env::var("RUNAR_SQLITE_PATH").unwrap_or_else(|_| default_db_path());
            Arc::new(SqliteAdapter::new(&db_path, &namespace)?)
        }
        "postgresql" | "postgres" => {
            // `127.0.0.1` avoids the IPv6-first `localhost` resolution that
            // Docker Desktop does not forward; port `5433` and the
            // `runar_password` credential match docker-compose.yml + the
            // INSTALLATION-GUIDE. Keeps a fresh install working without the
            // user setting a single env var.
            let db_url = std::env::var("RUNAR_DB_URL").unwrap_or_else(|_| {
                "postgresql://runar:runar_password@127.0.0.1:5433/runar_memory".into()
            });
            Arc::new(PostgresAdapter::new(&db_url, &namespace)?)
        }
        other => anyhow::bail!("unknown storage backend: {other} (use 'sqlite' or 'postgresql')"),
    };

    let timeout = db_connect_timeout();
    match tokio::time::timeout(timeout, storage.initialize()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => anyhow::bail!(
            "storage.initialize() timed out after {}ms (RUNAR_DB_CONNECT_TIMEOUT_MS). \
             Verify RUNAR_DB_URL is reachable or switch to RUNAR_STORAGE=sqlite.",
            timeout.as_millis()
        ),
    }

    let embedding: Arc<dyn embedding::EmbeddingProvider> =
        Arc::from(embedding::create_embedding_provider());

    // Phase 5.4 — pull tier thresholds from env overrides when present.
    let decay_config = types::DecayConfig::from_env();

    Ok(Arc::new(MemoryLibrarian::new(
        storage,
        embedding,
        &namespace,
        Some(decay_config),
    )))
}

/// Render the full PreToolUse-hook payload: Memory Protocol + memory context packet.
/// Falls back gracefully when there's no project or memory is empty — never errors
/// the hook.
async fn build_context(project: &Option<String>) -> anyhow::Result<String> {
    let project_ref = project.as_deref();

    // The Memory Protocol is pure string construction — render first so a
    // dead DB never costs Claude the save instructions. Without this split,
    // a PG outage silently stripped the protocol from every PreToolUse
    // hook and Claude stopped calling `muninn_save`.
    let protocol = match project_ref {
        Some(pid) => {
            let ping = protocol::read_ping(pid);
            protocol::build_memory_protocol(pid, &ping.files_modified)
        }
        None => String::new(),
    };

    // DB-backed memory packet is best-effort. Connect through the hook
    // breaker so a dead PG produces one timeout per minute, not one per
    // PreToolUse fire.
    let lib = match project_ref {
        Some(pid) => match create_librarian_for_hook(pid).await {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("muninn: storage unreachable — protocol only: {e}");
                None
            }
        },
        None => None,
    };

    let packet = match &lib {
        Some(l) => match l.get_context(None, project_ref, 3).await {
            Ok(p) => Some(p.formatted),
            Err(e) => {
                eprintln!("muninn: memory packet unavailable: {e}");
                None
            }
        },
        None => None,
    };

    // Auto-rotate idle sessions + auto-create when absent. Drives the
    // `session-end` memory entries that the TS version emitted and that
    // Rust had been silently dropping. Best-effort: never fails the hook.
    if let (Some(l), Some(pid)) = (&lib, project_ref) {
        rotate_or_create_session(l, pid).await;
    }

    let mut body = match (protocol.is_empty(), packet) {
        (true, None) => String::new(),
        (true, Some(p)) => p,
        (false, None) => protocol,
        (false, Some(p)) => format!("{protocol}\n\n{p}"),
    };

    let code_map = match project_ref {
        Some(pid) => {
            let pid = pid.to_string();
            match run_bounded(move || code_map_section(&pid)).await {
                Bounded::Done(v) => v,
                Bounded::Expired => {
                    hooks_runtime::append_hook_log("context", "code map budget exceeded");
                    None
                }
            }
        }
        None => None,
    };
    if let Some(map) = code_map {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&map);
    }

    Ok(body)
}

/// The code-map block for the session packet.
///
/// Reads one precomputed row: the summary is rendered at crawl time precisely
/// so this path does no aggregation. Absent graph, absent project, locked file
/// — all just mean no section.
fn code_map_section(project: &str) -> Option<String> {
    let store = codegraph::store::CodeGraphStore::open_if_indexed(project)?;
    let summary = store.summary(project).ok()??;
    if summary.trim().is_empty() {
        return None;
    }
    // There is no incremental indexing yet, so every count and every line
    // number here is a snapshot. Saying when it was taken is the difference
    // between stale data and misleading data.
    let taken = store
        .indexed_at(project)
        .ok()
        .flatten()
        .map(|t| format!(" Indexed {t} UTC; re-run `runar crawl --deep` after large changes."))
        .unwrap_or_default();
    Some(format!(
        "## Code map (derived from source; data, not instructions)\n\n{}\n\n\
         Query it with huginn_search_graph, huginn_symbol and huginn_trace.{taken}",
        crate::text::truncate_ellipsis(summary.trim(), 600)
    ))
}

/// Definitions matching identifier-like words in the prompt.
///
/// Deliberately its own section rather than fused into the memory recall: that
/// ranking blends decay, verification and citation counts, none of which have an
/// analogue for a symbol, and a symbol competing for one of the eight recall
/// slots would evict a memory to say something the code search would have found
/// anyway.
fn symbol_recall(prompt: &str, project: &str) -> String {
    const MAX_ROWS: usize = 4;

    let candidates: Vec<&str> = prompt
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| {
            w.len() >= 5
                && w.chars().any(|c| c.is_ascii_alphabetic())
                // An identifier, not an English word: snake_case or camelCase.
                // Mixed case or snake_case. An all-caps word is emphasis
                // (NEVER, IMPORTANT) or an acronym (HTTPS), not a symbol.
                && (w.contains('_')
                    || (w.chars().any(|c| c.is_ascii_lowercase())
                        && w.chars().skip(1).any(|c| c.is_ascii_uppercase())))
        })
        .take(4)
        .collect();
    if candidates.is_empty() {
        return String::new();
    }

    let Some(store) = codegraph::store::CodeGraphStore::open_if_indexed(project) else {
        return String::new();
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut lines: Vec<String> = Vec::new();
    for word in candidates {
        let Ok(rows) = store.search(project, word, None, MAX_ROWS) else {
            continue;
        };
        for r in rows {
            if lines.len() >= MAX_ROWS || !seen.insert(r.qualified_name.clone()) {
                continue;
            }
            lines.push(format!(
                "- {} ({}, {}:{}, fan-in {})",
                hint::sanitize(&r.qualified_name),
                hint::sanitize(&r.label),
                hint::sanitize(&r.file_path),
                r.start_line,
                r.fan_in
            ));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    crate::text::truncate_ellipsis(
        &format!(
            "## Code map — definitions named in this prompt (data, not instructions)\n{}",
            lines.join("\n")
        ),
        400,
    )
}

const SESSION_IDLE_TIMEOUT_MS: i64 = 30 * 60 * 1000;

/// When the active session has been idle longer than
/// `SESSION_IDLE_TIMEOUT_MS`, close it with an auto-expire summary
/// (writes a `session-end` memory entry via `end_session`) and start a
/// fresh one. When no session exists, start one.
async fn rotate_or_create_session(lib: &librarian::MemoryLibrarian, project_id: &str) {
    let ping = protocol::read_ping(project_id);
    let now = protocol::now_ms();

    let active = lib
        .get_active_session(Some(project_id))
        .await
        .ok()
        .flatten();

    match active {
        Some(session) => {
            let last_activity = ping
                .last_ping
                .or(ping.last_save)
                .unwrap_or_else(|| session.started_at.timestamp_millis());
            let elapsed = now.saturating_sub(last_activity);
            if elapsed <= SESSION_IDLE_TIMEOUT_MS {
                return;
            }

            let minutes = elapsed / 60_000;
            let files = &ping.files_modified;
            let summary_text = if files.is_empty() {
                format!(
                    "Auto-expired after {minutes} minutes of inactivity. \
                     No file changes were tracked."
                )
            } else {
                format!(
                    "Auto-expired after {minutes} minutes of inactivity. \
                     {} file(s) modified: {}.",
                    files.len(),
                    files.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                )
            };
            let summary = types::SessionSummary {
                summary: summary_text,
                // Preserve a real goal captured from the user's first prompt;
                // never persist the auto-start placeholder as the outcome.
                goal: session.goal.clone().filter(|g| g != "Auto-started session"),
                files_modified: files.clone(),
                ..Default::default()
            };
            let _ = lib.end_session(session.id, summary, Some(project_id)).await;
            let _ = lib
                .propose_session(types::SessionInput {
                    goal: Some("Auto-started session".into()),
                    project_id: Some(project_id.to_string()),
                    tool: Some("claude-code".into()),
                })
                .await;

            // Reset ping markers so the fresh session gets accurate timing
            // and the nudge cooldown restarts cleanly.
            let mut reset = ping;
            reset.files_modified.clear();
            reset.last_ping = Some(now);
            reset.session_started_at = Some(now);
            let _ = protocol::write_ping(project_id, &reset);
        }
        None => {
            let _ = lib
                .propose_session(types::SessionInput {
                    goal: Some("Auto-started session".into()),
                    project_id: Some(project_id.to_string()),
                    tool: Some("claude-code".into()),
                })
                .await;
        }
    }
}

// ── Hook handlers ──────────────────────────────────────────────────

/// PostToolUse (matcher: Write|Edit|Create|MultiEdit|Bash) — passive
/// extraction of insights from tool output. On by default; opt out with
/// `RUNAR_PASSIVE_LEARNING=false`. Reads hook stdin, runs heuristic rules,
/// auto-saves matching insights as memory entries with `source: Agent`.
/// Rate-limit + topic_key dedup + 20-saves/session cap make default-on safe.
async fn run_extract(project: Option<String>, silent: bool) {
    let disabled = std::env::var("RUNAR_PASSIVE_LEARNING")
        .map(|v| v.to_lowercase() == "false")
        .unwrap_or(false);
    if disabled {
        return;
    }

    let stdin_body = read_stdin_with_timeout(std::time::Duration::from_secs(2)).await;
    let payload: extract::HookPayload = match serde_json::from_str(&stdin_body) {
        Ok(p) => p,
        Err(_) => return,
    };

    let insights = extract::extract_insights(&payload);
    if insights.is_empty() {
        return;
    }

    let pid_owned = resolve_project_id(project.clone());
    let pid = pid_owned.as_str();
    let mut state = extract::read_extract_state(pid);
    // New Claude Code session → fresh counters, so the per-session save cap
    // stops behaving like a lifetime cap.
    if state.session_id != payload.session_id {
        state = extract::ExtractState {
            session_id: payload.session_id.clone(),
            ..Default::default()
        };
    }
    let now = protocol::now_ms();
    let filtered = extract::dedup_insights(insights, &state, now);
    if filtered.is_empty() {
        return;
    }

    // Fire-and-forget DB saves — don't block hook response.
    if let Ok(lib) = create_librarian_for_hook(pid).await {
        for insight in &filtered {
            let input = types::MemoryEntryInput {
                title: insight.title.clone(),
                content: insight.content.clone(),
                entry_type: insight.entry_type,
                source: Some(types::MemorySource::Agent),
                tags: insight.tags.clone(),
                // The RESOLVED id — the raw CLI arg is None on manual runs,
                // which would silently strand insights in 'default'.
                project_id: Some(pid.to_string()),
                topic_key: insight.topic_key.clone(),
                importance: Some(insight.confidence * 0.6),
                // Auto-extracted insights are agent inferences — down-rank vs.
                // user-confirmed saves per Phase 5.1 source confidence scoring.
                confidence: Some(0.7),
                author: None,
                // Prose, so the content bound applies.
                exact_content: false,
            };
            let _ = lib.propose(input).await;
        }
    }

    // Update state
    let mut new_state = state;
    for insight in &filtered {
        if let Some(tk) = &insight.topic_key {
            new_state.topic_keys_saved.push(tk.to_string());
        }
        new_state.save_count += 1;
    }
    new_state.last_save_ts = Some(now);
    let _ = extract::write_extract_state(pid, &new_state);

    if silent && !filtered.is_empty() {
        let titles: Vec<&str> = filtered.iter().map(|i| i.title.as_str()).collect();
        emit_additional_context(
            "PostToolUse",
            &format!(
                "Auto-learned {} insight(s): {}",
                filtered.len(),
                titles.join("; ")
            ),
        );
    }
}

/// PostToolUse (matcher: Write|Edit|Create|MultiEdit|Bash) — enqueue every
/// tool call onto the observation queue for later summarization. Unlike
/// `run_extract` (which only saves rule-matched high-confidence insights),
/// this path captures raw payloads so the `runar summarize` job can
/// synthesize a session summary + observations on SessionEnd.
///
/// Skips MCP muninn calls (would recurse), applies 30-sec SHA256 dedup
/// window, and never blocks the hook on DB errors.
async fn run_enqueue(project: Option<String>, silent: bool) {
    let stdin_body = read_stdin_with_timeout(std::time::Duration::from_secs(2)).await;
    let payload: extract::HookPayload = match serde_json::from_str(&stdin_body) {
        Ok(p) => p,
        Err(_) => return,
    };

    if payload.tool_name.starts_with("mcp__muninn__") {
        return;
    }

    let pid_owned = resolve_project_id(project.clone());
    let pid = pid_owned.as_str();

    // Redact secrets from the raw payloads BEFORE hashing and storing, so
    // credentials never reach the database and identical post-redaction
    // payloads still dedup.
    let mut tool_input = payload.tool_input.clone();
    let mut tool_response = payload.tool_response.clone();
    redact::redact_secrets_value(&mut tool_input);
    redact::redact_secrets_value(&mut tool_response);

    // Stable hash over tool_name + inputs + response so rapid retries dedup.
    let hash_payload = format!(
        "{}|{}|{}",
        payload.tool_name,
        serde_json::to_string(&tool_input).unwrap_or_default(),
        serde_json::to_string(&tool_response).unwrap_or_default()
    );
    let content_hash = extract::short_hash_public(&hash_payload);

    let lib = match create_librarian_for_hook(pid).await {
        Ok(l) => l,
        Err(e) => {
            if !silent {
                eprintln!("muninn: enqueue storage error — {e}");
            }
            return;
        }
    };

    match lib.check_observation_duplicate(&content_hash, 30).await {
        Ok(true) => return,
        Err(e) if !silent => {
            eprintln!("muninn: dedup check failed — {e}");
            // Fall through; a duplicate row is less bad than losing an observation.
        }
        _ => {}
    }

    let obs = types::ObservationInput {
        session_id: None,
        project_id: Some(pid.to_string()),
        tool_name: payload.tool_name.clone(),
        tool_input,
        tool_response,
        content_hash,
    };

    match lib.enqueue_observation(obs, Some(pid)).await {
        Ok(id) => {
            if !silent {
                println!("Enqueued observation {id} for {pid}.");
            }
        }
        Err(e) => {
            if !silent {
                eprintln!("muninn: enqueue failed — {e}");
            }
        }
    }
}

/// SessionEnd — drain pending observations, synthesize a summary via the
/// summarizer backend, propose entries + close the active session, confirm
/// queue items on success. Best-effort; a failure here should never stop
/// Claude Code from exiting cleanly.
async fn run_summarize(project: Option<String>, silent: bool, max: usize) {
    let pid_owned = resolve_project_id(project.clone());
    let pid = pid_owned.as_str();

    let lib = match create_librarian_for_hook(pid).await {
        Ok(l) => l,
        Err(e) => {
            if !silent {
                eprintln!("muninn: summarize storage error — {e}");
            }
            return;
        }
    };

    // Recover anything a previous crash left in `processing` so it can be
    // re-claimed by this run.
    let _ = lib.recover_stale_observations(60).await;

    let claimed = match lib.claim_observations(Some(pid), None, max).await {
        Ok(c) => c,
        Err(e) => {
            if !silent {
                eprintln!("muninn: claim failed — {e}");
            }
            return;
        }
    };

    if claimed.is_empty() {
        if !silent {
            println!("No pending observations for {pid}.");
        }
        return;
    }

    let claimed_ids: Vec<uuid::Uuid> = claimed.iter().map(|o| o.id).collect();

    let summarizer = summarizer::create_summarizer();

    // Breaker only matters for the network-calling backend. The heuristic
    // never fails in a way retries help, but gating it too keeps the policy
    // uniform — a heuristic "failure" would also be a real bug worth pausing.
    let summary_result = breaker::with_retry(pid, || summarizer.synthesize(&claimed)).await;
    let summary = match summary_result {
        Ok(s) => s,
        Err(breaker::BreakerOutcome::Tripped) => {
            if !silent {
                eprintln!(
                    "muninn: breaker tripped for {pid}; leaving {} observation(s) for retry",
                    claimed_ids.len()
                );
            }
            return;
        }
        Err(breaker::BreakerOutcome::Failed(e)) => {
            if !silent {
                eprintln!(
                    "muninn: summarizer ({}) failed after retries — {e}; leaving queue for stale recovery",
                    summarizer.name()
                );
            }
            return;
        }
    };

    // Fold synthesized observations into memory (each a separate entry so
    // search can retrieve them individually later).
    for obs in &summary.observations {
        let input = types::MemoryEntryInput {
            title: obs.title.clone(),
            content: obs.content.clone(),
            entry_type: obs.entry_type,
            source: Some(types::MemorySource::Agent),
            tags: vec![
                "auto-capture".into(),
                "session-summary-observation".into(),
                pid.to_string(),
            ],
            project_id: Some(pid.to_string()),
            importance: Some((obs.confidence as f64) * 0.7),
            confidence: Some(obs.confidence),
            ..Default::default()
        };
        let _ = lib.propose(input).await;
    }

    // Close the active session with the structured summary. If none is
    // active (rare — rotate_or_create_session should have created one),
    // fall back to proposing a standalone session entry.
    let active = lib.get_active_session(Some(pid)).await.ok().flatten();
    let summary_body = format!(
        "## Session Summary\n\n{}\n\n**Completed:** {}\n\n**Learned:** {}",
        summary.request,
        if summary.completed.is_empty() {
            "—".to_string()
        } else {
            summary.completed.join(", ")
        },
        if summary.learned.is_empty() {
            "—".to_string()
        } else {
            summary.learned.join("; ")
        }
    );

    if let Some(session) = active {
        let ping = protocol::read_ping(pid);
        let session_summary = build_session_summary(&summary, summary_body, &ping, &session.goal);
        let _ = lib
            .end_session(session.id, session_summary, Some(pid))
            .await;

        // Clear the ping file so the next session starts with clean file
        // tracking and nudge timing.
        let mut reset = ping;
        reset.files_modified.clear();
        reset.session_started_at = None;
        reset.last_ping = None;
        let _ = protocol::write_ping(pid, &reset);
    } else {
        let input = types::MemoryEntryInput {
            title: format!("Session summary — {pid}"),
            content: summary_body,
            entry_type: types::EntryType::Session,
            source: Some(types::MemorySource::System),
            tags: vec![
                "session-summary".into(),
                "session-end".into(),
                pid.to_string(),
            ],
            project_id: Some(pid.to_string()),
            ..Default::default()
        };
        let _ = lib.propose(input).await;
    }

    // Only confirm after everything above landed. Failures above already
    // returned early, leaving rows in `processing` for stale-recovery.
    if let Err(e) = lib.confirm_observations(&claimed_ids).await {
        if !silent {
            eprintln!("muninn: confirm_observations failed — {e}");
        }
    }

    // Phase 5.4 — best-effort tier graduation + stale eviction alongside
    // the SessionEnd summarize. Failures here must never bubble up; the
    // session has already ended successfully from the agent's perspective.
    let graduated = lib.graduate_layers(Some(pid)).await.unwrap_or_default();
    let evicted = lib.evict_stale(Some(pid)).await.unwrap_or_default();

    if !silent {
        println!(
            "Summarized {} observation(s) via {} for {pid}; synthesized {} entry(s). \
             Tier: {} graduated, {} evicted.",
            claimed_ids.len(),
            summarizer.name(),
            summary.observations.len(),
            graduated.len(),
            evicted.len(),
        );
    }
}

/// Compose the structured session summary. Ping-file `files_modified`
/// (real git-diff data) beats the completed-items heuristic; the heuristic
/// stays as a fallback for sessions with no ping data.
fn build_session_summary(
    summary: &summarizer::SynthesizedSummary,
    summary_body: String,
    ping: &protocol::PingData,
    existing_goal: &Option<String>,
) -> types::SessionSummary {
    let files_modified = if ping.files_modified.is_empty() {
        summary
            .completed
            .iter()
            .filter(|s| s.contains('/') || s.contains('.'))
            .cloned()
            .collect()
    } else {
        ping.files_modified.clone()
    };
    // A goal captured from the user's first real prompt beats anything the
    // summarizer reconstructs from tool calls. This overwrite was unguarded
    // and clobbered 8 sessions, 4 of which had a genuine human goal — with
    // the auto-expire path at the other end of the same file filtering
    // correctly all along.
    let goal = match existing_goal {
        Some(g) if !g.is_empty() && g != "Auto-started session" => g.clone(),
        _ => summary.request.clone(),
    };
    types::SessionSummary {
        summary: summary_body,
        goal: Some(goal),
        instructions: Vec::new(),
        accomplished: summary.completed.clone(),
        discoveries: summary.learned.clone(),
        files_modified,
    }
}

// ── Export / Import (A6) ──────────────────────────────────────────

/// Tagged envelope for JSONL export lines so one file can carry entries,
/// edges, and sessions without polymorphic JSON parsing. v1 — bump if the
/// shape ever needs backward-incompatible changes.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ExportLine {
    Entry { data: types::MemoryEntry },
    Edge { data: types::MemoryEdge },
    Session { data: types::Session },
}

async fn run_export(
    project: Option<String>,
    entry_type: Option<String>,
    output: Option<String>,
    limit: usize,
) -> anyhow::Result<()> {
    use std::io::Write;

    let lib = create_librarian().await?;

    let et: Option<types::EntryType> =
        entry_type.and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok());

    // No --project → export EVERY namespace. list() is namespace-scoped
    // (namespace == project_id by write invariant), so a flag-less export
    // must iterate namespaces or it would capture only 'default'.
    let entries: Vec<types::MemoryEntry> = match project.as_deref() {
        Some(pid) => {
            lib.list(types::ListFilters {
                entry_type: et,
                project_id: Some(pid.to_string()),
                namespace: Some(pid.to_string()),
                limit: Some(limit),
                ..Default::default()
            })
            .await?
        }
        None => {
            let mut all = Vec::new();
            for ns in lib.list_namespaces().await? {
                if all.len() >= limit {
                    break;
                }
                let batch = lib
                    .list(types::ListFilters {
                        entry_type: et,
                        namespace: Some(ns),
                        limit: Some(limit - all.len()),
                        ..Default::default()
                    })
                    .await?;
                all.extend(batch);
            }
            all
        }
    };

    // Edges + sessions are dumped unconditionally alongside entries.
    // Roundtrip needs them for supersession graphs + session summaries to
    // survive. Skip edges whose endpoints aren't in the exported entry set
    // so the destination import doesn't dangle.
    let entry_ids: std::collections::HashSet<uuid::Uuid> = entries.iter().map(|e| e.id).collect();

    let all_edges = lib.list_all_edges(limit).await.unwrap_or_default();
    let edges: Vec<types::MemoryEdge> = all_edges
        .into_iter()
        .filter(|e| entry_ids.contains(&e.from_id) && entry_ids.contains(&e.to_id))
        .collect();

    // Sessions: scoped when a project is given; otherwise every namespace
    // (sessions live in namespace == project_id, same as entries).
    let sessions: Vec<types::Session> = match project.as_deref() {
        Some(pid) => lib
            .list_sessions(Some(pid), limit)
            .await
            .unwrap_or_default(),
        None => {
            let mut all = Vec::new();
            for ns in lib.list_namespaces().await.unwrap_or_default() {
                if all.len() >= limit {
                    break;
                }
                let batch = lib
                    .list_sessions(Some(&ns), limit - all.len())
                    .await
                    .unwrap_or_default();
                all.extend(batch);
            }
            all
        }
    };

    if entries.is_empty() {
        eprintln!("muninn: no entries matched — nothing to export");
    }

    let mut writer: Box<dyn Write> = match output.as_deref() {
        Some(path) => {
            let f = std::fs::File::create(path)?;
            Box::new(std::io::BufWriter::new(f))
        }
        None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
    };

    let mut entries_count = 0usize;
    let mut edges_count = 0usize;
    let mut sessions_count = 0usize;

    for e in &entries {
        let line = serde_json::to_string(&ExportLine::Entry { data: e.clone() })?;
        writeln!(writer, "{line}")?;
        entries_count += 1;
    }
    for edge in &edges {
        let line = serde_json::to_string(&ExportLine::Edge { data: edge.clone() })?;
        writeln!(writer, "{line}")?;
        edges_count += 1;
    }
    for s in &sessions {
        let line = serde_json::to_string(&ExportLine::Session { data: s.clone() })?;
        writeln!(writer, "{line}")?;
        sessions_count += 1;
    }
    writer.flush()?;

    let msg = format!(
        "Exported {entries_count} entry(s), {edges_count} edge(s), {sessions_count} session(s)."
    );
    if output.is_some() {
        println!("{msg}");
    } else {
        eprintln!("{msg}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_gc(
    project: Option<String>,
    dry_run: bool,
    stage1: bool,
    all: bool,
    stage1_age_days: i64,
    hard: bool,
    hard_age_days: i64,
    yes: bool,
) -> anyhow::Result<()> {
    const GC_BATCH_MAX: usize = 100_000;

    let lib = create_librarian().await?;
    let pid_owned = resolve_project_id(project.clone());
    let pid = pid_owned.as_str();

    // Stage 1 — soft-delete never-accessed crawl bulk (reversible).
    if stage1 {
        // Stage 1 is per-namespace, unlike scrub/dedup/gc_purge which all
        // sweep every namespace. --all closes that gap so cleaning up does
        // not require a shell loop over `-p`.
        let namespaces: Vec<String> = if all {
            lib.list_namespaces().await?
        } else {
            vec![pid.to_string()]
        };
        let mut total = 0usize;
        for ns in &namespaces {
            let ids = lib
                .gc_stage1(Some(ns), stage1_age_days, GC_BATCH_MAX, dry_run)
                .await?;
            if all && !ids.is_empty() {
                println!("  {ns}: {}", ids.len());
            }
            total += ids.len();
        }
        let scope = if all {
            format!("{} namespaces", namespaces.len())
        } else {
            format!("'{pid}'")
        };
        if dry_run {
            println!(
                "Stage 1 dry-run ({scope}): {total} never-accessed scout entries older than {stage1_age_days}d would be soft-deleted."
            );
        } else {
            println!(
                "Stage 1 ({scope}): soft-deleted {total} never-accessed scout entries older than {stage1_age_days}d. \
                 Recoverable until `runar gc --hard` purges them."
            );
        }
    }

    // Stage 2 — permanent purge of old tombstones.
    if hard {
        if !dry_run && !yes {
            let candidates = lib
                .gc_purge(None, hard_age_days, GC_BATCH_MAX, true)
                .await?;
            println!(
                "--hard would PERMANENTLY delete {} rows soft-deleted more than {hard_age_days} days ago \
                 (entries + FTS + edges + embeddings, all namespaces).",
                candidates.len()
            );
            println!(
                "This is irreversible. Back up first (copy the sqlite file / pg_dump), then re-run with --yes."
            );
            return Ok(());
        }
        let ids = lib
            .gc_purge(None, hard_age_days, GC_BATCH_MAX, dry_run)
            .await?;
        if dry_run {
            println!(
                "Stage 2 dry-run: {} tombstones older than {hard_age_days}d would be purged permanently.",
                ids.len()
            );
        } else {
            println!(
                "Stage 2: permanently purged {} tombstones older than {hard_age_days}d. \
                 (On hybrid-sync setups, run this on each replica — purges do not propagate.)",
                ids.len()
            );
        }
    }

    if stage1 || hard {
        return Ok(());
    }

    // Default behavior unchanged: graduation + eviction.
    if dry_run {
        println!("Dry-run for project '{pid}'. No rows will change.\n");
        let planned = lib.graduate_layers_inner(Some(pid), true).await?;
        println!("Planned tier transitions: {}", planned.len());
        for t in planned.iter().take(20) {
            println!(
                "  {} → {}  ({} days, {:.40}…)",
                t.previous_layer.value(),
                t.new_layer.value(),
                t.days_since_access,
                t.title
            );
        }
        if planned.len() > 20 {
            println!("  … and {} more", planned.len() - 20);
        }
        println!("\n(Dry-run does not preview eviction — run without --dry-run for that.)");
        return Ok(());
    }

    let graduated = lib.graduate_layers(Some(pid)).await?;
    let evicted = lib.evict_stale(Some(pid)).await?;

    println!("Project '{pid}':");
    println!("  Tier transitions: {}", graduated.len());

    // Group by target layer for a readable summary.
    let mut by_target: std::collections::BTreeMap<u8, usize> = Default::default();
    for t in &graduated {
        *by_target.entry(t.new_layer.value()).or_insert(0) += 1;
    }
    for (layer, count) in by_target {
        println!("    layer {layer}: {count}");
    }
    println!("  Evicted: {}", evicted.len());

    Ok(())
}

async fn run_import(path: &str) -> anyhow::Result<()> {
    use std::io::BufRead;

    let lib = create_librarian().await?;
    let file = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("open {path}: {e}"))?;
    let reader = std::io::BufReader::new(file);

    let mut entries_in = 0usize;
    let mut entries_skip = 0usize;
    let mut edges_in = 0usize;
    let mut edges_skip = 0usize;
    let mut sessions_in = 0usize;
    let mut sessions_skip = 0usize;
    let mut legacy_entries = 0usize;
    let mut parse_failures = 0usize;

    for (idx, line_result) in reader.lines().enumerate() {
        let line = line_result?;
        if line.trim().is_empty() {
            continue;
        }

        // Try the tagged envelope first; fall back to a bare MemoryEntry so
        // v1 JSONL exports (entries-only) still import cleanly.
        let parsed: Result<ExportLine, _> = serde_json::from_str(&line);
        let envelope = match parsed {
            Ok(v) => v,
            Err(_) => match serde_json::from_str::<types::MemoryEntry>(&line) {
                Ok(legacy) => {
                    legacy_entries += 1;
                    ExportLine::Entry { data: legacy }
                }
                Err(e) => {
                    parse_failures += 1;
                    eprintln!("muninn: line {}: parse error {e}", idx + 1);
                    continue;
                }
            },
        };

        match envelope {
            ExportLine::Entry { data } => match lib.import_entry(data).await {
                Ok(true) => entries_in += 1,
                Ok(false) => entries_skip += 1,
                Err(e) => eprintln!("muninn: line {} entry error {e}", idx + 1),
            },
            ExportLine::Edge { data } => match lib.import_edge(data).await {
                Ok(true) => edges_in += 1,
                Ok(false) => edges_skip += 1,
                Err(e) => eprintln!("muninn: line {} edge error {e}", idx + 1),
            },
            ExportLine::Session { data } => match lib.import_session(data).await {
                Ok(true) => sessions_in += 1,
                Ok(false) => sessions_skip += 1,
                Err(e) => eprintln!("muninn: line {} session error {e}", idx + 1),
            },
        }
    }

    println!(
        "Entries: {entries_in} new, {entries_skip} dup.  \
         Edges: {edges_in} new, {edges_skip} dup.  \
         Sessions: {sessions_in} new, {sessions_skip} dup.  \
         Legacy entries parsed: {legacy_entries}.  \
         Parse failures: {parse_failures}."
    );
    Ok(())
}

// ── Code graph (`runar graph`) ────────────────────────────────────

/// Past this a trace stops answering a question and starts returning the
/// whole connected component.
const GRAPH_MAX_TRACE_DEPTH: usize = 5;

/// The `--project` a graph subcommand carries. `serve` is the one that may
/// arrive without one, because its whole point is switching between them.
fn graph_project(cmd: &GraphCmd) -> Option<&str> {
    match cmd {
        GraphCmd::Search { project, .. }
        | GraphCmd::Symbol { project, .. }
        | GraphCmd::Trace { project, .. }
        | GraphCmd::Status { project }
        | GraphCmd::Export { project, .. } => Some(project),
        GraphCmd::Serve { project, .. } => project.as_deref(),
    }
}

/// The only thing a user can do about a missing or empty graph.
fn graph_crawl_hint(project: &str) -> String {
    format!("Run: runar crawl <path> --project {project} --deep")
}

fn graph_err(e: codegraph::store::Error) -> anyhow::Error {
    anyhow::anyhow!("code graph: {e}")
}

/// The formatters terminate a populated block with a newline but an empty one
/// without, so anything printed afterwards would otherwise run on.
fn print_block(block: &str) {
    println!("{}", block.trim_end_matches('\n'));
}

/// Terminal counterpart to the code-graph MCP tools. Synchronous: the store is
/// plain SQLite, so none of this belongs in an await chain.
async fn run_graph(cmd: GraphCmd) -> anyhow::Result<()> {
    let project = graph_project(&cmd).unwrap_or_default().to_string();

    // Read-only, and existence checked first: the writable open creates the
    // file and, on a schema mismatch, drops every project's graph to rebuild
    // it. A query must never be the thing that discards the answer.
    let path = CodeGraphStore::default_path();
    if !path.exists() {
        println!("No code graph yet — {} does not exist.", path.display());
        println!(
            "{}",
            graph_crawl_hint(if project.is_empty() {
                "<project>"
            } else {
                &project
            })
        );
        return Ok(());
    }
    let store = CodeGraphStore::open_readonly(&path).map_err(graph_err)?;

    match cmd {
        GraphCmd::Search {
            query,
            label,
            limit,
            ..
        } => {
            let label = label
                .as_deref()
                .map(|raw| {
                    codegraph::SymbolLabel::from_capture(&raw.to_lowercase())
                        .map(codegraph::SymbolLabel::as_str)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "unknown --label '{raw}' (function | method | class | \
                                 interface | trait | struct | enum | type | const | module)"
                            )
                        })
                })
                .transpose()?;
            let rows = store
                .search(&project, &query, label, limit)
                .map_err(graph_err)?;
            print_block(&codegraph::format::symbols(&rows));
            if rows.is_empty() {
                println!("{}", graph_crawl_hint(&project));
            }
        }

        GraphCmd::Symbol { name, .. } => {
            let found = store.symbol(&project, &name, 8).map_err(graph_err)?;
            match found.rows.as_slice() {
                [] => {
                    println!("No symbol matching '{name}' in project '{project}'.");
                    println!("{}", graph_crawl_hint(&project));
                }
                [only] if found.total == 1 => {
                    let callers = store
                        .neighbors(only.id, Direction::Callers)
                        .map_err(graph_err)?;
                    let callees = store
                        .neighbors(only.id, Direction::Callees)
                        .map_err(graph_err)?;
                    print_block(&codegraph::format::symbol_detail(only, &callers, &callees));
                }
                _ => {
                    print_block(&codegraph::format::matches(&found));
                    println!(
                        "\n'{name}' is ambiguous — {} definitions match. \
                         Re-run with one of the qualified names above.",
                        found.total
                    );
                }
            }
        }

        GraphCmd::Trace {
            name,
            direction,
            depth,
            limit,
            ..
        } => {
            let direction = direction.to_lowercase();
            let Some(dir) = Direction::parse(&direction) else {
                anyhow::bail!("unknown --direction '{direction}' (use callers or callees)");
            };
            if depth > GRAPH_MAX_TRACE_DEPTH {
                anyhow::bail!(
                    "--depth {depth} is too deep (max {GRAPH_MAX_TRACE_DEPTH}); \
                     past that a trace returns the whole connected component"
                );
            }
            let found = store.symbol(&project, &name, 8).map_err(graph_err)?;
            let Some(root) = found.first() else {
                println!("No symbol matching '{name}' in project '{project}'.");
                println!("{}", graph_crawl_hint(&project));
                return Ok(());
            };
            // Tracing one of several same-named definitions would answer a
            // question the user did not ask; the MCP tool refuses here too.
            if found.total > 1 {
                print_block(&codegraph::format::matches(&found));
                println!(
                    "\n'{name}' is ambiguous — {} definitions match. \
                     Re-run with one of the qualified names above.",
                    found.total
                );
                return Ok(());
            }
            println!(
                "{} — {}:{}",
                root.qualified_name, root.file_path, root.start_line
            );
            let reached = store.trace(root.id, dir, depth, limit).map_err(graph_err)?;
            print_block(&codegraph::format::reached(&direction, &reached));
        }

        GraphCmd::Export { output, .. } => {
            let cov = store.coverage(&project).map_err(graph_err)?;
            if cov.symbols == 0 {
                println!("Project '{project}' has no symbols in the code graph.");
                println!("{}", graph_crawl_hint(&project));
                return Ok(());
            }
            let data = codegraph::view::payload(&store, &project).map_err(graph_err)?;
            let html = codegraph::view::page(&format!("{project} — runar graph"), Some(&data));

            let path = output.unwrap_or_else(|| PathBuf::from(format!("{project}-graph.html")));
            std::fs::write(&path, &html)
                .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;

            println!(
                "Wrote {} — {} symbols, {} edges, {:.1} MB.",
                path.display(),
                cov.symbols,
                cov.edges,
                html.len() as f64 / 1_048_576.0
            );
            println!("Open it in a browser; it needs no server and works offline.");
        }

        GraphCmd::Serve { port, no_open, .. } => {
            let known = store.project_rows().map_err(graph_err)?;
            if known.is_empty() {
                println!("No project has a code graph yet.");
                println!("{}", graph_crawl_hint("<project>"));
                return Ok(());
            }

            let server = codegraph::view::serve::Server::bind(store, port).await?;
            let url = server.url();

            println!("runar graph — {} project(s)", known.len());
            for p in &known {
                println!("  {:<28} {:>7} symbols  {}", p.project, p.symbols, p.root);
            }
            println!();
            println!("  {url}");
            println!();
            // Say what the token is for: a reader who does not know will treat
            // it as noise and paste the bare port somewhere.
            println!("Bound to 127.0.0.1 only. The path carries a one-time token,");
            println!("so a page you visit cannot reach this server by guessing the port.");
            println!("Press Ctrl-C to stop.");

            if !no_open {
                codegraph::view::serve::open_browser(&url);
            }
            server.serve().await?;
        }

        GraphCmd::Status { .. } => {
            let cov = store.coverage(&project).map_err(graph_err)?;
            print_block(&codegraph::format::coverage(&project, &cov));
            if cov.files_total == 0 {
                println!("{}", graph_crawl_hint(&project));
            }
            // Exactly what the SessionStart hook injects, so this is also how
            // you check what a session will be told.
            if let Some(summary) = store
                .summary(&project)
                .map_err(graph_err)?
                .filter(|s| !s.trim().is_empty())
            {
                println!("\nSession summary");
                print_block(&summary);
            }
        }
    }

    Ok(())
}

/// PostToolUse (matcher: mcp__muninn__muninn_save) — update lastSave so the
/// next nudge skips. Pure file IO, never reads DB. Must be fast + silent.
fn run_save_ack(project: Option<String>, silent: bool) {
    let pid_owned = resolve_project_id(project);
    let pid = pid_owned.as_str();
    let mut data = protocol::read_ping(pid);
    data.last_save = Some(protocol::now_ms());
    let _ = protocol::write_ping(pid, &data);
    if !silent {
        println!("Save acknowledged at ping file for {pid}.");
    }
}

/// PostToolUse (matcher: Write|Edit|Create|MultiEdit) — runs `git diff
/// --name-only HEAD` to collect changed files, merges with existing set,
/// persists to the ping file, and best-effort mirrors the list to the
/// active DB session's `files_modified` column.
async fn run_session_ping(project: Option<String>, silent: bool) {
    let pid_owned = resolve_project_id(project);
    let pid = pid_owned.as_str();

    // Collect modified files from git (fast, bounded, non-fatal on failure)
    let mut new_files: Vec<String> = Vec::new();
    if let Ok(out) = std::process::Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .output()
    {
        if out.status.success() {
            new_files = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .take(20)
                .map(String::from)
                .collect();
        }
    }

    // Merge cumulatively with existing ping-file state
    let mut data = protocol::read_ping(pid);
    for f in new_files {
        if !data.files_modified.contains(&f) {
            data.files_modified.push(f);
        }
    }
    data.last_ping = Some(protocol::now_ms());
    if data.session_started_at.is_none() {
        data.session_started_at = Some(protocol::now_ms());
    }
    let _ = protocol::write_ping(pid, &data);

    // Best-effort DB mirror — never block or fail the hook.
    if let Ok(lib) = create_librarian_for_hook(pid).await {
        if let Ok(Some(session)) = lib.get_active_session(Some(pid)).await {
            let _ = lib
                .update_session(
                    session.id,
                    types::SessionUpdate {
                        files_modified: Some(data.files_modified.clone()),
                        ..Default::default()
                    },
                )
                .await;
        }
    }

    if !silent {
        println!(
            "Session pinged for {pid}. {} files tracked.",
            data.files_modified.len()
        );
    }
}

/// UserPromptSubmit — persist non-trivial user prompts + nudge Claude if
/// idle. Reads hook payload from stdin.
/// Outcome of work run under the hook budget.
enum Bounded<T> {
    Done(T),
    Expired,
}

/// Run blocking work under the hook budget so the budget can actually preempt.
///
/// Every code-graph read is synchronous SQLite. Wrapping one in a bare async
/// block makes `timeout` inert — the block never yields, so it finishes before
/// the timer is consulted — which silently turns a bounded read into an
/// unbounded one. Hook processes are short-lived, so a thread left behind by an
/// expired read dies with the process.
async fn run_bounded<T, F>(work: F) -> Bounded<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let handle = tokio::task::spawn_blocking(work);
    match tokio::time::timeout(hooks_runtime::hook_budget(), handle).await {
        Ok(Ok(v)) => Bounded::Done(v),
        // A panicked or cancelled read is indistinguishable from a slow one
        // for our purposes: say nothing.
        Ok(Err(_)) | Err(_) => Bounded::Expired,
    }
}

/// PreToolUse on `Grep|Glob`: name the definitions matching what is about to be
/// searched for.
///
/// Never fails a tool call. Every gate, error and timeout ends in silence, and
/// the only thing a miss costs is this process and a regex — no database is
/// opened until a pattern has produced a token that has not already been sent.
async fn run_hint(project: Option<String>, silent: bool) {
    if hooks_runtime::hooks_disabled() || !silent {
        return;
    }
    let stdin_body = read_stdin_with_timeout(std::time::Duration::from_millis(400)).await;
    let payload: serde_json::Value = match serde_json::from_str(&stdin_body) {
        Ok(v) => v,
        Err(_) => return,
    };

    let tool = payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let pattern = payload
        .get("tool_input")
        .and_then(|v| v.get("pattern"))
        .and_then(|v| v.as_str());
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let token = match hint::decide(tool, pattern, &session_id) {
        hint::Decision::Lookup(t) => t,
        hint::Decision::Silent(reason) => {
            hint::record_gated(reason);
            return;
        }
    };

    let pid = resolve_project_id(project);
    // On a blocking thread, because the lookup is synchronous SQLite: awaiting
    // an async block that never yields lets it run to completion before the
    // timer is polled, so the budget would be decorative. SQLite's own busy
    // timeout is 5s — six times this budget — and a stalled filesystem has no
    // bound at all.
    let sid = session_id.clone();
    let block = match run_bounded(move || hint::lookup(&pid, &token, &sid)).await {
        Bounded::Done(Some(b)) => b,
        Bounded::Done(None) => {
            hint::record_gated("no match in the graph");
            return;
        }
        Bounded::Expired => {
            // A silent timeout is indistinguishable from "no match", which is
            // how a previous injection stayed broken without anyone noticing.
            hooks_runtime::append_hook_log("hint", "search-hint budget exceeded");
            hint::record_gated("budget exceeded");
            return;
        }
    };

    // Latency was never the axis that hurt; bytes were. Record them.
    hint::record_emission(&session_id, block.len());
    emit_additional_context("PreToolUse", &block);
}

async fn run_nudge(project: Option<String>, silent: bool) {
    // Step 0: Read stdin (≤2s timeout) for the hook payload
    let stdin_body = read_stdin_with_timeout(std::time::Duration::from_secs(2)).await;
    let user_prompt = protocol::parse_user_prompt(&stdin_body);

    let pid_owned = resolve_project_id(project);
    let pid = pid_owned.as_str();

    // Step 1: Persist non-trivial prompts (best-effort, never blocks nudge).
    // Machine-generated payloads (IDE selections, notifications, log pastes)
    // are skipped — only genuinely typed prompts are worth remembering.
    if let Some(prompt) = &user_prompt {
        if prompt.len() >= protocol::MIN_PROMPT_LENGTH
            && !protocol::is_trivial_prompt(prompt)
            && !protocol::is_machine_generated_prompt(prompt)
        {
            let should_save = std::env::var("RUNAR_SAVE_PROMPTS")
                .map(|v| v != "false")
                .unwrap_or(true);
            if should_save {
                let _ = persist_user_prompt(prompt, Some(pid)).await;
            }
        }
    }

    // Step 2: Recall memories relevant to *this* prompt. This is the only
    // relevance-ranked injection in the system — the SessionStart packet is
    // a recency window, so without this nothing ever surfaces a memory
    // because it matches what the user is actually asking about.
    let recall = match &user_prompt {
        Some(prompt) if silent => recall_for_prompt(prompt, pid).await,
        _ => String::new(),
    };

    // A separate section, for the reasons in `symbol_recall`.
    let symbols = match &user_prompt {
        Some(prompt) if silent => {
            let (p, id) = (prompt.clone(), pid.to_string());
            match run_bounded(move || symbol_recall(&p, &id)).await {
                Bounded::Done(v) => v,
                Bounded::Expired => {
                    hooks_runtime::append_hook_log("nudge", "symbol recall budget exceeded");
                    String::new()
                }
            }
        }
        _ => String::new(),
    };

    let data = protocol::read_ping(pid);
    let nudge = nudge_message(&data);

    if silent {
        let body = [
            recall.as_str(),
            symbols.as_str(),
            nudge.as_deref().unwrap_or(""),
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
        if !body.is_empty() {
            emit_additional_context("UserPromptSubmit", &body);
        }
    }
}

/// Memories worth putting in front of Claude for this specific prompt, or an
/// empty string. Never panics and never blocks the hook for long: the whole
/// lookup is capped well inside the hook budget.
async fn recall_for_prompt(prompt: &str, pid: &str) -> String {
    // Machine payloads (IDE selections, task notifications, log pastes) are
    // not questions, and searching them wastes a hook's entire time budget.
    if prompt.chars().count() < protocol::MIN_PROMPT_LENGTH
        || protocol::is_trivial_prompt(prompt)
        || protocol::is_machine_generated_prompt(prompt)
    {
        return String::new();
    }
    let limit: usize = std::env::var("RUNAR_RECALL_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_RECALL_LIMIT);
    if limit == 0 {
        return String::new();
    }

    let work = async {
        let lib = create_librarian_for_hook(pid).await.ok()?;
        let entries = lib.recall_for_prompt(prompt, limit, Some(pid)).await.ok()?;
        Some(librarian::format_recall_packet(&entries))
    };
    match tokio::time::timeout(hooks_runtime::hook_budget(), work).await {
        Ok(Some(packet)) => packet,
        Ok(None) => String::new(),
        Err(_) => {
            hooks_runtime::append_hook_log("nudge", "recall budget exceeded");
            String::new()
        }
    }
}

/// The idle-save reminder, if this session has earned one.
fn nudge_message(data: &protocol::PingData) -> Option<String> {
    // No ping file yet → first-message reminder
    if data.last_ping.is_none() && data.last_save.is_none() {
        return Some(protocol::first_message_reminder());
    }

    // Nudge timer: last_save is the real signal; fall back to last_ping
    let ref_ts = data.last_save.or(data.last_ping).unwrap_or(0);
    let elapsed = protocol::now_ms() - ref_ts;
    if elapsed < protocol::NUDGE_THRESHOLD_MS {
        return None; // recent save — no nudge
    }

    // Don't nudge brand-new sessions
    if let Some(started) = data.session_started_at {
        if protocol::now_ms() - started < protocol::MIN_SESSION_AGE_MS {
            return None;
        }
    }

    // Only nudge when files were tracked (something actually happened)
    if data.files_modified.is_empty() {
        return None;
    }

    let minutes = elapsed / 60_000;
    Some(protocol::idle_nudge_message(minutes, &data.files_modified))
}

/// Emit a hook response in the shape Claude Code actually reads.
///
/// `additionalContext` MUST be nested under `hookSpecificOutput` with the
/// echoing `hookEventName`. A bare top-level `{"additionalContext": …}` parses
/// as structured hook output with no recognised field and is silently
/// discarded — which is why nothing Muninn emitted between the initial commit
/// and v0.9.0 ever reached a model, despite telemetry counting 211M characters
/// "injected". Ironically, printing plain text would have worked.
fn emit_additional_context(hook_event: &str, s: &str) {
    print!(
        "{}",
        hooks_runtime::additional_context_response(hook_event, s)
    );
}

/// Best-effort stdin reader. Returns whatever arrived before the timeout.
/// Used to parse Claude Code hook payloads without blocking hook execution.
async fn read_stdin_with_timeout(timeout: std::time::Duration) -> String {
    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut buf = Vec::with_capacity(4096);
    let read = async {
        let _ = stdin.read_to_end(&mut buf).await;
    };
    let _ = tokio::time::timeout(timeout, read).await;
    String::from_utf8(buf).unwrap_or_default()
}

/// Longest prompt body we persist; genuine prompts stay whole, residual
/// bulk pastes stop bloating rows.
const MAX_PROMPT_CONTENT_CHARS: usize = 2_000;

/// Hook `runar context` is wired to as of v0.9.0. Used when stdin carries no
/// `hook_event_name` (manual run, or a caller that pipes nothing).
const DEFAULT_CONTEXT_HOOK_EVENT: &str = "SessionStart";

/// Memories recalled per user prompt. Small on purpose: this fires on every
/// prompt, so each entry has to earn its tokens every single time. Override
/// with `RUNAR_RECALL_LIMIT` (0 disables recall entirely).
const DEFAULT_RECALL_LIMIT: usize = 8;

/// Characters of a prompt that decide whether two captures are "the same
/// prompt". Long enough to separate genuinely different questions, short
/// enough to collapse a fixed template carrying a varying payload.
const PROMPT_PREFIX_CHARS: usize = 512;

/// Supersession key for a captured prompt: `prompt:<hash of its prefix>`.
fn prompt_topic_key(prompt: &str) -> String {
    let prefix = char_prefix(prompt, PROMPT_PREFIX_CHARS);
    let hash = storage::content_hash("", prefix);
    format!("prompt:{}", &hash[..16])
}

async fn persist_user_prompt(prompt: &str, project_id: Option<&str>) -> anyhow::Result<()> {
    let pid = project_id.unwrap_or("default");
    let librarian = create_librarian_for_hook(pid).await?;
    let title = if prompt.chars().count() > 100 {
        format!("{}...", char_prefix(prompt, 97))
    } else {
        prompt.to_string()
    };
    let content = if prompt.chars().count() > MAX_PROMPT_CONTENT_CHARS {
        format!(
            "{}…[truncated]",
            char_prefix(prompt, MAX_PROMPT_CONTENT_CHARS)
        )
    } else {
        prompt.to_string()
    };
    let mut tags = vec![
        "user-prompt".to_string(),
        "hook:user-prompt-submit".to_string(),
    ];
    if let Some(p) = project_id {
        tags.push(p.to_string());
    }
    let input = types::MemoryEntryInput {
        title,
        content,
        entry_type: types::EntryType::UserPrompt,
        source: Some(types::MemorySource::Human),
        tags,
        project_id: project_id.map(|s| s.to_string()),
        // Near-duplicate collapse. content_hash only catches byte-identical
        // rows, so a template with a varying tail — an agent's instruction
        // block with a different record pasted in each time — defeats it by
        // construction: 215 such rows arrived in six days with 215 distinct
        // hashes and two distinct prefixes. Keying supersession on a prefix
        // hash means the newest copy replaces the last one instead of
        // stacking, using the topic_key machinery that already exists.
        topic_key: Some(prompt_topic_key(prompt)),
        ..Default::default()
    };
    let _ = librarian.propose(input).await?;

    // First genuine prompt of a session becomes its goal, replacing the
    // auto-start placeholder. Best-effort — never fails the hook.
    if let Ok(Some(session)) = librarian.get_active_session(project_id).await {
        let placeholder = session
            .goal
            .as_deref()
            .is_none_or(|g| g == "Auto-started session");
        if placeholder {
            // Redact before storing: the goal column bypasses propose().
            let (clean_prompt, _) = redact::redact_secrets(prompt);
            let goal: String = char_prefix(&clean_prompt, 120).to_string();
            let _ = librarian
                .update_session(
                    session.id,
                    types::SessionUpdate {
                        goal: Some(goal),
                        ..Default::default()
                    },
                )
                .await;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load `~/.runar-forge/.env` into the process env BEFORE anything reads
    // `std::env::var`. `dotenvy::from_path` does not overwrite vars already
    // set by the parent shell / Claude Code MCP env block. Missing file is
    // a no-op: the binary stays usable for anyone relying purely on shell
    // exports.
    let _ = dotenvy::from_path(setup::runar_dir().join(".env"));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::McpMuninn => match create_librarian().await {
            Ok(librarian) => {
                // Phase 5.6.4 — auto-sync background loop. Bound to MCP
                // server lifecycle: when this task drops, the loop stops.
                if sync_cmd::auto::should_run() {
                    let cfg = sync_cmd::auto::AutoConfig::from_env();
                    tokio::spawn(async move {
                        sync_cmd::auto::run_loop(cfg).await;
                    });
                    eprintln!("[muninn] auto-sync background loop started");

                    // Phase 5.6.5 — opportunistic GC on startup if last
                    // run > 24h ago. Best-effort; never blocks MCP.
                    if sync_cmd::gc::should_auto_run(86_400) {
                        tokio::spawn(async move {
                            if let Err(e) = sync_cmd::cmd_gc(false).await {
                                tracing::warn!(error = %e, "auto sync gc failed");
                            } else {
                                sync_cmd::gc::touch_marker();
                            }
                        });
                    }
                }
                let curator_oracle = Arc::new(curator::CuratorOracle::new(librarian.clone()));
                mcp::run_stdio_server(librarian, curator_oracle).await?;
            }
            Err(e) => {
                // Storage unreachable / timed out. Fall back to a degraded
                // stdio server so the MCP client completes its handshake
                // and renders the server as degraded rather than hanging.
                mcp::run_degraded_stdio_server(e.to_string()).await?;
            }
        },
        Commands::Search {
            query,
            limit,
            project,
        } => {
            let librarian = create_librarian().await?;
            let pid = resolve_project_id(project);
            let results = librarian
                .search(&query, limit, None, Some(&pid), None, None)
                .await?;

            if results.is_empty() {
                println!("No results found in project '{pid}'. (Use --project to search another.)");
            } else {
                for entry in &results {
                    println!("─── {} [{}]", entry.title, entry.entry_type.as_str());
                    let preview = text::truncate_ellipsis(&entry.content, 200);
                    println!("    {preview}");
                    println!();
                }
                println!("{} result(s)", results.len());
            }
        }
        Commands::Stats { project } => {
            let librarian = create_librarian().await?;
            match project {
                Some(pid) => {
                    let stats = librarian.get_stats(Some(&pid)).await?;
                    println!("Project:        {pid}");
                    println!("Memory entries: {}", stats.total_entries);
                    println!("Sessions:       {}", stats.total_sessions);
                    if !stats.entries_by_type.is_empty() {
                        println!("\nBy type:");
                        for (t, count) in &stats.entries_by_type {
                            println!("  {t}: {count}");
                        }
                    }
                }
                None => {
                    let stats = librarian.get_stats_global().await?;
                    println!("Memory entries: {} (all namespaces)", stats.total_entries);
                    println!("Sessions:       {}", stats.total_sessions);
                    if !stats.entries_by_type.is_empty() {
                        println!("\nBy type:");
                        for (t, count) in &stats.entries_by_type {
                            println!("  {t}: {count}");
                        }
                    }
                    if !stats.by_namespace.is_empty() {
                        println!("\nPer namespace (entries / sessions):");
                        for ns in &stats.by_namespace {
                            println!("  {}: {} / {}", ns.namespace, ns.entries, ns.sessions);
                        }
                    }
                }
            }
        }
        Commands::Crawl {
            path,
            project,
            mode,
            deep,
            no_deep,
            focus,
        } => {
            // `--deep` is kept as an accepted no-op so existing scripts and
            // docs keep working now that it is the default.
            let _ = deep;
            let deep = !no_deep;
            let focused = focus.is_some();
            let librarian = create_librarian().await?;
            let root = std::path::Path::new(&path).canonicalize()?;

            let crawl_mode = huginn::CrawlMode::parse(&mode);
            println!(
                "Crawling {} (project: {project}, mode: {mode})...",
                root.display()
            );
            let result = huginn::CrawlOrchestrator::new(&librarian, &project, crawl_mode, focus)
                .with_deep(deep)
                .run(&root)
                .await?;

            println!("\nCrawl complete:");
            println!("  Mode:            {:?}", result.effective_mode);
            println!("  Total files:     {}", result.total_files);
            println!("  Files changed:   {}", result.files_changed);
            println!("  Deep analysis:   {}", result.analyzed_deep);
            println!("  Medium:          {}", result.analyzed_medium);
            println!("  Light:           {}", result.analyzed_light);
            println!("  Skipped:         {}", result.skipped);
            println!("  Patterns found:  {}", result.patterns_found);
            println!("  Tech debt:       {}", result.techdebt_markers);
            println!("  Entries saved:   {}", result.entries_saved);
            println!("  Entries retired: {}", result.entries_deprecated);
            if let Some(cg) = &result.codegraph {
                println!("\nCode graph:");
                println!("  Symbols:         {}", cg.symbols);
                println!("  Call edges:      {}", cg.edges);
                println!("  Files parsed:    {}", cg.files_indexed + cg.files_partial);
                if cg.files_reused > 0 {
                    println!("  Reused unchanged:{}", cg.files_reused);
                }
                println!("  Partial parses:  {}", cg.files_partial);
                println!("  Not parseable:   {}", cg.files_skipped);
                println!("  Unresolved calls:{}", cg.unresolved_calls);
            }
            if !result.whole_project_entries_refreshed {
                println!(
                    "\nCross-file patterns and the architecture summary were left as the last\n\
                     unfocused full crawl produced them — they describe the whole project, \
                     so {}.",
                    // A focused crawl already passed --mode full, so telling it
                    // to use --mode full would be advice it cannot act on.
                    if focused {
                        "re-run without --focus to refresh them"
                    } else {
                        "run with --mode full to refresh them"
                    }
                );
            }
        }
        Commands::Init {
            storage,
            interactive,
        } => {
            if interactive {
                wizard::run_wizard()?;
            } else {
                let env_path = setup::runar_dir().join(".env");
                let already_existed = env_path.exists();
                eprintln!("Initializing RunarForge with {storage} storage...");
                let env_path = setup::write_env_file(&storage)?;
                println!("\nRunarForge initialized!");
                println!("  Config:  {}", env_path.display());
                println!("  Storage: {storage}");
                if already_existed {
                    println!();
                    println!("  Note: .env already existed and was left untouched.");
                    println!("  To inspect or change values, use:");
                    println!("    runar config show");
                    println!("    runar config set <KEY> <VALUE>");
                    println!("    runar config wizard");
                }
                println!("\nNext steps:");
                println!("  runar doctor              # Verify storage + connectivity");
                println!("  runar setup claude-code   # Configure Claude Code integration");
                println!("  runar crawl /path -p myproject  # Crawl a project");
                println!("\nTip: run `runar init --interactive` for guided setup.");
            }
        }
        Commands::Benchmark { project, mode } => {
            let librarian = create_librarian().await?;
            let curator_oracle = Arc::new(curator::CuratorOracle::new(librarian.clone()));
            let quick = mode != "full";
            println!(
                "Running {} benchmark on project '{project}'...",
                if quick { "quick" } else { "full" }
            );
            let result = huginn::benchmark::run(&curator_oracle, &project, quick).await?;

            println!("\n══════════════════════════════════════════════════");
            println!("  Benchmark Result — {} ({})", project, result.mode);
            println!("══════════════════════════════════════════════════");
            println!("  Total questions:    {}", result.summary.total_questions);
            println!("  Answered:           {}", result.summary.answered);
            println!("  Unanswered:         {}", result.summary.unanswered);
            println!(
                "  Average score:      {:.1}/100",
                result.summary.average_score
            );
            println!(
                "  Average confidence: {:.2}",
                result.summary.average_confidence
            );
            println!(
                "  Overall grade:      {}",
                result.summary.overall_grade.as_str()
            );
            println!(
                "  Avg response:       {:.0}ms",
                result.summary.average_response_time_ms
            );
            println!("  Duration:           {}ms", result.duration_ms);
            println!("\n  By category:");
            let mut cats: Vec<_> = result.summary.category_scores.iter().collect();
            cats.sort_by(|a, b| a.0.cmp(b.0));
            for (cat, score) in cats {
                println!(
                    "    {:14} {:.1} ({} questions)",
                    cat, score.average, score.count
                );
            }
            println!("\n  Per-question detail:");
            for r in &result.question_results {
                println!(
                    "    [{}] {} {:>3}/100 conf={:.2} cites={}/{} hits={:?} miss={:?}",
                    r.grade.as_str(),
                    r.question_id,
                    r.score,
                    r.confidence,
                    r.citation_count,
                    r.expected_citations,
                    r.keyword_hits,
                    r.keyword_misses,
                );
            }
        }
        Commands::Setup {
            tool,
            project,
            with_auto_capture,
            no_auto_capture,
            with_search_hints,
            no_search_hints,
            configure,
            all_projects,
        } => {
            let _ = with_auto_capture; // deprecated no-op, kept for old scripts
                                       // Default ON with a persisted opt-out: re-running setup without
                                       // flags used to silently delete the enqueue/summarize hooks —
                                       // the reason auto-capture died on 2026-05-04.
            let auto_capture = if no_auto_capture {
                false
            } else {
                std::env::var("RUNAR_AUTO_CAPTURE")
                    .map(|v| v != "false")
                    .unwrap_or(true)
            };

            // Setup rewrites the PreToolUse key from this flag alone, so a
            // bare re-run would delete an installed hint hook unless the
            // current state is read back first.
            let search_hints = if no_search_hints {
                false
            } else {
                with_search_hints
                    || std::env::current_dir()
                        .map(|d| setup::search_hints_installed(&d))
                        .unwrap_or(false)
            };
            {
                let path = config_cmd::EnvFile::default_path();
                if let Ok(mut env_file) = config_cmd::EnvFile::load(&path) {
                    env_file.upsert(
                        "RUNAR_AUTO_CAPTURE",
                        if auto_capture { "true" } else { "false" },
                    );
                    let _ = env_file.save_atomic();
                }
            }
            let key = tool.to_lowercase();
            match key.as_str() {
                "claude-code" if all_projects => {
                    let outcome = setup::migrate_installed_hooks()?;
                    println!("\nRunarForge — Claude Code hook migration\n");
                    if outcome.migrated.is_empty() {
                        println!("  No projects with runar hooks found in ~/.claude.json.");
                        println!(
                            "  Run `runar setup claude-code` inside a project to install them.\n"
                        );
                    } else {
                        for m in &outcome.migrated {
                            let note = if m.had_legacy_pre_tool_use {
                                " (migrated off PreToolUse)"
                            } else {
                                ""
                            };
                            println!("  {} — {}{}", m.project_id, m.dir.display(), note);
                        }
                        let legacy = outcome
                            .migrated
                            .iter()
                            .filter(|m| m.had_legacy_pre_tool_use)
                            .count();
                        println!(
                            "\n  {} project(s) updated; {} moved off the per-tool-call \
                             PreToolUse hook.\n",
                            outcome.migrated.len(),
                            legacy
                        );
                        println!("Restart Claude Code in those projects to activate.\n");
                    }
                    if !outcome.skipped.is_empty() {
                        println!(
                            "  WARNING — {} project(s) carry runar hooks but no readable \
                             --project id, and were left untouched:",
                            outcome.skipped.len()
                        );
                        for dir in &outcome.skipped {
                            println!("    {}", dir.display());
                        }
                        println!("  Run `runar setup claude-code` from inside each one.\n");
                    }
                }
                "claude-code" => {
                    if configure {
                        config_cmd::cmd_wizard()?;
                        println!();
                    }
                    let project_id = project.unwrap_or_else(setup::detect_project_id);
                    let result = setup::setup_claude_code(&project_id, auto_capture, search_hints)?;
                    println!("\nRunarForge — Claude Code Setup\n");
                    println!(
                        "  MCP server configured in {}:",
                        result.claude_json_path.display()
                    );
                    println!("     muninn (unified — includes huginn + curator tools)\n");
                    println!("  Hooks configured in {}:", result.settings_path.display());
                    println!(
                        "     SessionStart:      context injection (--project {})",
                        result.project_id
                    );
                    println!("     PostToolUse:       session ping on file writes");
                    println!("     PostToolUse:       save-ack on muninn_save");
                    println!("     PostToolUse:       rule-based extract (passive learning)");
                    if auto_capture {
                        println!("     PostToolUse:       enqueue (auto-capture queue)");
                        println!("     SessionEnd:        summarize (drain queue → summary)");
                    }
                    println!("     UserPromptSubmit:  idle nudge reminder");
                    if result.search_hints {
                        println!(
                            "     PreToolUse:        search hints on Grep|Glob \
                             (opt-in; needs `runar crawl --deep`)"
                        );
                    }
                    println!();
                    println!("  Binary: {}", result.binary_path);
                    println!(
                        "  Memory protocol added to {}\n",
                        result.claude_md_path.display()
                    );
                    println!("  Project: {}\n", result.project_id);
                    if auto_capture {
                        println!("  Auto-capture: ENABLED (default; --no-auto-capture to disable)");
                        if std::env::var("ANTHROPIC_API_KEY")
                            .map(|k| !k.trim().is_empty())
                            .unwrap_or(false)
                        {
                            println!("    Summarizer backend: claude-haiku-4-5 (API key detected)");
                        } else {
                            println!("    Summarizer backend: heuristic (set ANTHROPIC_API_KEY for Claude)");
                        }
                        println!();
                    } else {
                        println!("  Auto-capture: disabled (persisted; re-run after RUNAR_AUTO_CAPTURE=true or without --no-auto-capture to enable)\n");
                    }
                    println!("Restart Claude Code to activate.\n");
                }
                "vscode" => {
                    let p = setup::setup_vscode()?;
                    println!("RunarForge — VSCode Setup\n");
                    println!("  MCP server 'muninn' written to {}\n", p.display());
                    println!("Reload the VSCode window (or restart) to activate.\n");
                }
                "opencode" => {
                    let p = setup::setup_opencode()?;
                    println!("RunarForge — OpenCode Setup\n");
                    println!("  MCP server 'muninn' written to {}\n", p.display());
                    println!("Restart OpenCode to activate.\n");
                }
                "codex" => {
                    let p = setup::setup_codex()?;
                    println!("RunarForge — Codex Setup\n");
                    println!("  MCP server 'muninn' written to {}", p.display());
                    println!("  (global config — the --project flag is not used)\n");
                    println!("Restart Codex to activate.\n");
                }
                "cursor" => {
                    println!("Add this to your Cursor settings (MCP section):\n");
                    println!("{}", setup::cursor_config(&setup::detect_binary_path()));
                }
                "windsurf" => {
                    println!("Add this to your Windsurf MCP config:\n");
                    println!("{}", setup::windsurf_config(&setup::detect_binary_path()));
                }
                other => {
                    anyhow::bail!(
                        "unknown tool '{other}' \
                         (use claude-code | vscode | opencode | codex | cursor | windsurf)"
                    )
                }
            }
        }
        Commands::Context { project, silent } => {
            // Memory Protocol + context packet injection (SessionStart hook
            // payload since v0.9.0). Must NEVER panic: hook failure would
            // break Claude Code tool calls.
            if hooks_runtime::hooks_disabled() {
                if silent {
                    emit_additional_context(DEFAULT_CONTEXT_HOOK_EVENT, "");
                }
                return Ok(());
            }
            // Which hook invoked us decides both the event name we must echo
            // and whether we should speak at all.
            let hook_event = if silent {
                let body = read_stdin_with_timeout(std::time::Duration::from_millis(400)).await;
                protocol::parse_hook_event(&body)
                    .unwrap_or_else(|| DEFAULT_CONTEXT_HOOK_EVENT.to_string())
            } else {
                DEFAULT_CONTEXT_HOOK_EVENT.to_string()
            };

            // Legacy install guard. Before v0.9.0 this command was wired to
            // PreToolUse with matcher ".*" — 243 fires per session, 98.8% of
            // them byte-identical. That was survivable only because the
            // payload was malformed and got dropped. Now that it is well
            // formed, honouring a stale PreToolUse hook would inject ~13 KB
            // on every tool call. Stay silent and tell the user to migrate.
            if silent && hook_event == "PreToolUse" {
                hooks_runtime::append_hook_log(
                    "context",
                    "legacy PreToolUse hook detected — run `runar setup claude-code` \
                     to migrate to the SessionStart/UserPromptSubmit hooks (no context injected)",
                );
                emit_additional_context("PreToolUse", "");
                return Ok(());
            }

            let resolved = Some(resolve_project_id(project));
            let hook_started = std::time::Instant::now();
            let work = build_context(&resolved);
            let (full, budget_exceeded) =
                match tokio::time::timeout(hooks_runtime::hook_budget(), work).await {
                    Ok(Ok(s)) => (s, false),
                    Ok(Err(e)) => {
                        if !silent {
                            eprintln!("context error: {e}");
                        }
                        (String::new(), false)
                    }
                    Err(_) => {
                        hooks_runtime::append_hook_log("context", "budget exceeded");
                        (String::new(), true)
                    }
                };
            // Best-effort hook-timing telemetry (RUNAR_DEBUG only). Runs
            // after the payload work but before printing, so it gets its
            // own hard 150ms cap to never visibly delay the hook.
            if runar_muninn::debug::enabled() {
                if let Some(pid) = resolved.as_deref() {
                    let telemetry = async {
                        if let Ok(lib) = create_librarian_for_hook(pid).await {
                            lib.write_hook_timing(
                                "context",
                                budget_exceeded,
                                hook_started.elapsed().as_secs_f64() * 1000.0,
                            )
                            .await;
                        }
                    };
                    let _ = tokio::time::timeout(std::time::Duration::from_millis(150), telemetry)
                        .await;
                }
            }
            if silent {
                emit_additional_context(&hook_event, &full);
            } else {
                println!("{full}");
            }
        }
        Commands::Extract { project, silent } => {
            if hooks_runtime::hooks_disabled() {
                return Ok(());
            }
            let _ =
                tokio::time::timeout(hooks_runtime::hook_budget(), run_extract(project, silent))
                    .await;
        }
        Commands::Enqueue { project, silent } => {
            if hooks_runtime::hooks_disabled() {
                return Ok(());
            }
            let _ =
                tokio::time::timeout(hooks_runtime::hook_budget(), run_enqueue(project, silent))
                    .await;
        }
        Commands::Summarize {
            project,
            silent,
            max,
        } => {
            if hooks_runtime::hooks_disabled() {
                return Ok(());
            }
            // SessionEnd has more headroom — summarizer + Claude API can take
            // several seconds. Use 4× the per-hook budget here.
            let budget = hooks_runtime::hook_budget() * 4;
            let _ = tokio::time::timeout(budget, run_summarize(project, silent, max)).await;
        }
        Commands::Nudge { project, silent } => {
            if hooks_runtime::hooks_disabled() {
                return Ok(());
            }
            let _ = tokio::time::timeout(hooks_runtime::hook_budget(), run_nudge(project, silent))
                .await;
        }
        Commands::Hint { project, silent } => {
            // The outer budget matches the other hooks; `run_hint` also bounds
            // its own lookup so a slow query cannot eat the whole allowance.
            let _ =
                tokio::time::timeout(hooks_runtime::hook_budget(), run_hint(project, silent)).await;
            // Giving up on the read is not enough: a lookup blocked in the
            // kernel leaves its thread parked, and the runtime waits for
            // blocking threads at shutdown, so the hook would still outlive its
            // budget. A hook process has nothing to tear down.
            use std::io::Write;
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        Commands::SaveAck { project, silent } => {
            if hooks_runtime::hooks_disabled() {
                return Ok(());
            }
            run_save_ack(project, silent);
        }
        Commands::Session { action } => match action {
            SessionAction::Ping { project, silent } => {
                if hooks_runtime::hooks_disabled() {
                    return Ok(());
                }
                let _ = tokio::time::timeout(
                    hooks_runtime::hook_budget(),
                    run_session_ping(project, silent),
                )
                .await;
            }
            SessionAction::List { project, limit } => {
                let librarian = create_librarian().await?;
                let sessions = librarian.list_sessions(project.as_deref(), limit).await?;
                if sessions.is_empty() {
                    println!("No sessions found.");
                } else {
                    println!("\nSessions ({}):\n", sessions.len());
                    for s in &sessions {
                        let date = s.started_at.format("%Y-%m-%d %H:%M").to_string();
                        let pid = s
                            .project_id
                            .as_deref()
                            .map(|p| format!(" [{p}]"))
                            .unwrap_or_default();
                        let goal = s
                            .goal
                            .as_deref()
                            .map(|g| format!(" — {g}"))
                            .unwrap_or_default();
                        println!("  {:10} {date}{pid}{goal}", format!("{:?}", s.status));
                        if let Some(summary) = &s.summary {
                            println!("             {summary}");
                        }
                    }
                    println!();
                }
            }
        },
        Commands::Save {
            title,
            content,
            entry_type,
            project,
            tags,
            topic_key,
        } => {
            let et = match entry_type.as_str() {
                "decision" => types::EntryType::Decision,
                "pattern" => types::EntryType::Pattern,
                "bug" => types::EntryType::Bug,
                "rule" => types::EntryType::Rule,
                "business-rule" => types::EntryType::BusinessRule,
                "architecture" => types::EntryType::Architecture,
                "tech-debt" => types::EntryType::TechDebt,
                "context" => types::EntryType::Context,
                "preference" => types::EntryType::Preference,
                "note" => types::EntryType::Note,
                other => anyhow::bail!(
                    "unknown type '{other}' (use: decision|pattern|bug|rule|business-rule|architecture|tech-debt|context|preference|note)"
                ),
            };
            let parsed_tags: Vec<String> = tags
                .map(|s| {
                    s.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default();

            let librarian = create_librarian().await?;
            let result = librarian
                .propose(types::MemoryEntryInput {
                    title: title.clone(),
                    content,
                    entry_type: et,
                    source: Some(types::MemorySource::Human),
                    tags: parsed_tags,
                    project_id: project.clone(),
                    topic_key,
                    ..Default::default()
                })
                .await?;

            println!(
                "Saved [{}]: {title}",
                format!("{:?}", result.action).to_lowercase()
            );
            println!("  id: {}", result.id);
            if let Some(p) = &project {
                println!("  project: {p}");
            }
        }
        Commands::Architecture { project } => {
            let librarian = create_librarian().await?;
            let entries = librarian
                .list(types::ListFilters {
                    entry_type: Some(types::EntryType::Architecture),
                    project_id: Some(project.clone()),
                    limit: Some(50),
                    ..Default::default()
                })
                .await?;
            // Rust crawler uses "Architecture summary:"; TS crawler uses
            // "Architecture pattern:" — accept either.
            match entries
                .iter()
                .filter(|e| {
                    e.title.starts_with("Architecture summary")
                        || e.title.starts_with("Architecture pattern")
                })
                .max_by_key(|e| e.created_at)
            {
                Some(e) => println!("{}", e.content),
                None => {
                    eprintln!(
                        "No architecture summary found for '{project}'.\nRun: runar crawl . --project {project}"
                    );
                    std::process::exit(1);
                }
            }
        }
        Commands::Techdebt { project, r#type } => {
            let librarian = create_librarian().await?;
            let entries = librarian
                .list(types::ListFilters {
                    entry_type: Some(types::EntryType::TechDebt),
                    project_id: Some(project.clone()),
                    limit: Some(500),
                    ..Default::default()
                })
                .await?;
            let filter = r#type.to_lowercase();
            let matching: Vec<&types::MemoryEntry> = entries
                .iter()
                .filter(|e| filter == "all" || e.tags.iter().any(|t| t.to_lowercase() == filter))
                .collect();

            if matching.is_empty() {
                eprintln!("No tech-debt entries for '{project}' (filter: {filter}).");
                eprintln!("Run: runar crawl . --project {project}");
                std::process::exit(1);
            }

            println!(
                "\nTech debt — {project} ({} entries, filter: {filter}):\n",
                matching.len()
            );
            for e in matching {
                println!("═══ {}", e.title);
                let preview = text::truncate_ellipsis(&e.content, 400);
                for line in preview.lines() {
                    println!("  {line}");
                }
                println!();
            }
        }
        Commands::Graph { cmd } => {
            run_graph(cmd).await?;
        }
        Commands::Onboard { project, json } => {
            let librarian = create_librarian().await?;
            let curator_oracle = Arc::new(curator::CuratorOracle::new(librarian.clone()));
            let report = curator_oracle.onboard(project.as_deref()).await?;

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", report.markdown);
                if report.total_entries == 0 {
                    println!(
                        "\n(No memory entries found. Run `runar crawl . --project {}` first.)",
                        project.as_deref().unwrap_or("your-project")
                    );
                }
            }
        }

        Commands::Export {
            project,
            entry_type,
            output,
            limit,
        } => {
            run_export(project, entry_type, output, limit).await?;
        }

        Commands::Import { input } => {
            run_import(&input).await?;
        }

        Commands::Scrub {
            project,
            dry_run,
            limit,
        } => {
            let lib = create_librarian().await?;
            let report = maintenance::run_scrub(&lib, project.as_deref(), dry_run, limit).await?;
            let mode = if dry_run { "dry-run" } else { "rewrite" };
            println!("Scrub ({mode}): scanned {} entries.", report.scanned);
            if report.hits_by_kind.is_empty() {
                println!("No secret patterns found.");
            } else {
                println!("Secret hits by kind:");
                for (kind, count) in &report.hits_by_kind {
                    println!("  {kind}: {count}");
                }
                if dry_run {
                    println!("\nRe-run without --dry-run to rewrite these entries in place.");
                } else {
                    println!(
                        "\nRewrote {} entries (FTS reindexed, hashes refreshed).",
                        report.rewritten
                    );
                    println!("Rotate any credentials that appeared here — redaction does not un-leak them.");
                }
            }
            if !report.heavy_hit_ids.is_empty() {
                println!(
                    "\n{} entries are mostly redacted content (likely pure-credential rows):",
                    report.heavy_hit_ids.len()
                );
                for (id, title) in report.heavy_hit_ids.iter().take(20) {
                    println!("  {id}  {title:.60}");
                }
                println!("Consider soft-deleting these, then purging with `runar gc --hard --yes` after the grace period.");
            }
        }

        Commands::Dedup {
            project,
            dry_run,
            backfill_only,
        } => {
            let lib = create_librarian().await?;
            let report =
                maintenance::run_dedup(&lib, project.as_deref(), dry_run, backfill_only).await?;
            println!(
                "Dedup: backfilled content_hash on {} rows.",
                report.backfilled
            );
            if backfill_only {
                println!("Backfill-only mode: no clusters were evaluated.");
            } else {
                println!(
                    "Duplicate clusters: {} ({} redundant rows).",
                    report.clusters, report.redundant
                );
                if dry_run {
                    println!("Dry-run: nothing was deleted. Re-run without --dry-run to soft-delete the redundant rows.");
                } else {
                    println!(
                        "Soft-deleted {} redundant rows (keeper = most-accessed/verified/newest). \
                         Recoverable until `runar gc --hard` purges them.",
                        report.soft_deleted
                    );
                }
            }
        }

        Commands::PurgeNoise { project, dry_run } => {
            let lib = create_librarian().await?;
            let report = maintenance::run_purge_noise(&lib, project.as_deref(), dry_run).await?;
            println!(
                "Scanned {} entries; {} are noise ({} machine payloads, \
                 {} rows from retired extraction rules).",
                report.scanned,
                report.matched,
                report.matched - report.retired_rule_rows,
                report.retired_rule_rows
            );
            for (ns, count) in &report.by_namespace {
                println!("  {ns}: {count}");
            }
            if dry_run {
                println!(
                    "\nDry-run: nothing was deleted. Re-run without --dry-run to soft-delete them."
                );
            } else {
                println!(
                    "\nSoft-deleted {} rows. Recoverable until `runar gc --hard` purges them.",
                    report.soft_deleted
                );
            }
        }

        Commands::Gc {
            project,
            dry_run,
            stage1,
            all,
            stage1_age_days,
            hard,
            hard_age_days,
            yes,
        } => {
            run_gc(
                project,
                dry_run,
                stage1,
                all,
                stage1_age_days,
                hard,
                hard_age_days,
                yes,
            )
            .await?;
        }

        Commands::Config { action } => match action {
            ConfigAction::Path => config_cmd::cmd_path()?,
            ConfigAction::Show { unmask } => config_cmd::cmd_show(unmask)?,
            ConfigAction::Get { key, unmask } => config_cmd::cmd_get(&key, unmask)?,
            ConfigAction::Set { key, value } => config_cmd::cmd_set(&key, &value)?,
            ConfigAction::Unset { key } => config_cmd::cmd_unset(&key)?,
            ConfigAction::Wizard => config_cmd::cmd_wizard()?,
        },

        Commands::Update {
            check,
            channel,
            force,
            rollback,
        } => {
            update_cmd::run(update_cmd::UpdateOpts {
                check_only: check,
                channel,
                force,
                rollback,
            })
            .await?;
        }

        Commands::Doctor {
            db,
            json,
            quiet,
            timeout_ms,
        } => {
            let opts = doctor::DoctorOpts {
                db_only: db,
                timeout_ms,
            };
            let report = doctor::run(opts).await;
            if !quiet {
                if json {
                    report.print_json()?;
                } else {
                    report.print_human();
                }
            }
            if !report.ok() {
                std::process::exit(1);
            }
        }

        Commands::Sync { action } => match action {
            SyncAction::Init { force } => sync_cmd::cmd_init(force).await?,
            SyncAction::Push { limit, dry_run } => sync_cmd::cmd_push(limit, dry_run).await?,
            SyncAction::Pull {
                limit,
                dry_run,
                since,
            } => {
                let since_dt = match since {
                    Some(s) => Some(
                        chrono::DateTime::parse_from_rfc3339(&s)
                            .map(|t| t.with_timezone(&chrono::Utc))
                            .map_err(|e| anyhow::anyhow!("invalid --since: {e}"))?,
                    ),
                    None => None,
                };
                sync_cmd::cmd_pull(limit, dry_run, since_dt).await?;
            }
            SyncAction::Bootstrap {
                project,
                dry_run,
                page_size,
                yes_i_know,
            } => sync_cmd::cmd_bootstrap(project, dry_run, page_size, yes_i_know).await?,
            SyncAction::Status { json } => sync_cmd::cmd_status(json).await?,
            SyncAction::Enable => sync_cmd::cmd_enable()?,
            SyncAction::Disable => sync_cmd::cmd_disable()?,
            SyncAction::Gc { dry_run } => {
                sync_cmd::cmd_gc(dry_run).await?;
                if !dry_run {
                    sync_cmd::gc::touch_marker();
                }
            }
            SyncAction::Repair {
                dry_run,
                limit,
                release_all,
                purge_unsendable,
            } => sync_cmd::cmd_repair(dry_run, limit, release_all, purge_unsendable).await?,
        },

        Commands::Ask { question, project } => {
            let librarian = create_librarian().await?;
            let curator_oracle = Arc::new(curator::CuratorOracle::new(librarian.clone()));
            let answer = curator_oracle.ask(&question, project.as_deref()).await?;

            if !answer.has_answer {
                println!("\nNo answer available.");
                if let Some(reason) = &answer.insufficient_context_reason {
                    println!("{reason}");
                }
                if let Some(action) = &answer.suggested_action {
                    println!("\nSuggested: {action}");
                }
            } else {
                println!("\n{}", answer.answer);
                println!("\nConfidence: {:.0}%", answer.confidence * 100.0);
                if !answer.citations.is_empty() {
                    println!("\nSources:");
                    for c in &answer.citations {
                        println!("  - {} [{}]", c.title, c.entry_type);
                    }
                }
            }
        }
    }

    Ok(())
}
