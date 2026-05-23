use std::sync::Arc;

use clap::{Parser, Subcommand};

// Import the library crate rather than re-declaring every module, so that
// items used only by MCP tool dispatch aren't flagged as dead from the
// binary's perspective.
use runar_muninn::{
    breaker, config_cmd, curator, doctor, embedding, extract, hooks_runtime, huginn, librarian,
    mcp, protocol, setup, storage, summarizer, sync as sync_cmd, types, update as update_cmd,
    wizard,
};

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
    },

    /// Show memory system stats
    Stats,

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
        /// Install the auto-capture queue hooks (enqueue + SessionEnd summarize).
        /// Opt-in until the feature is stable.
        #[arg(long)]
        with_auto_capture: bool,
        /// Run `runar config wizard` first to (re)configure storage backend.
        /// Phase 5.5 — replaces the manual ".env edit then setup" flow.
        #[arg(long)]
        configure: bool,
    },

    // ── Hook-support commands (called by Claude Code hooks) ────────
    /// Print memory context for PreToolUse hook (stub until item 2)
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
    Gc {
        /// Project namespace to garbage-collect (auto-detected if omitted)
        #[arg(short, long)]
        project: Option<String>,
        /// Print planned transitions + eviction candidates without mutating
        #[arg(long)]
        dry_run: bool,
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

    Ok(match (protocol.is_empty(), packet) {
        (true, None) => String::new(),
        (true, Some(p)) => p,
        (false, None) => protocol,
        (false, Some(p)) => format!("{protocol}\n\n{p}"),
    })
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
            let summary = types::SessionSummary {
                summary: format!("Auto-expired after {minutes} minutes of inactivity."),
                files_modified: ping.files_modified.clone(),
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
    let state = extract::read_extract_state(pid);
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
                project_id: project.clone(),
                topic_key: insight.topic_key.clone(),
                importance: Some(insight.confidence * 0.6),
                // Auto-extracted insights are agent inferences — down-rank vs.
                // user-confirmed saves per Phase 5.1 source confidence scoring.
                confidence: Some(0.7),
                author: None,
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
        emit_additional_context(&format!(
            "Auto-learned {} insight(s): {}",
            filtered.len(),
            titles.join("; ")
        ));
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

    // Stable hash over tool_name + inputs + response so rapid retries dedup.
    let hash_payload = format!(
        "{}|{}|{}",
        payload.tool_name,
        serde_json::to_string(&payload.tool_input).unwrap_or_default(),
        serde_json::to_string(&payload.tool_response).unwrap_or_default()
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
        tool_input: payload.tool_input.clone(),
        tool_response: payload.tool_response.clone(),
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
        let session_summary = types::SessionSummary {
            summary: summary_body,
            goal: Some(summary.request.clone()),
            instructions: Vec::new(),
            accomplished: summary.completed.clone(),
            discoveries: summary.learned.clone(),
            files_modified: summary
                .completed
                .iter()
                .filter(|s| s.contains('/') || s.contains('.'))
                .cloned()
                .collect(),
        };
        let _ = lib
            .end_session(session.id, session_summary, Some(pid))
            .await;
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

    let filters = types::ListFilters {
        entry_type: et,
        project_id: project.clone(),
        tags: None,
        namespace: project.clone(),
        limit: Some(limit),
        offset: None,
    };

    let entries = lib.list(filters).await?;

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

    // Sessions scoped by project (or namespace) only when a project filter
    // is in play — full export otherwise.
    let sessions: Vec<types::Session> = match project.as_deref() {
        Some(pid) => lib
            .list_sessions(Some(pid), limit)
            .await
            .unwrap_or_default(),
        None => lib.list_sessions(None, limit).await.unwrap_or_default(),
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

async fn run_gc(project: Option<String>, dry_run: bool) -> anyhow::Result<()> {
    let lib = create_librarian().await?;
    let pid_owned = resolve_project_id(project.clone());
    let pid = pid_owned.as_str();

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
        if let Ok(Some(session)) = lib.get_active_session(None).await {
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
async fn run_nudge(project: Option<String>, silent: bool) {
    // Step 0: Read stdin (≤2s timeout) for the hook payload
    let stdin_body = read_stdin_with_timeout(std::time::Duration::from_secs(2)).await;
    let user_prompt = protocol::parse_user_prompt(&stdin_body);

    let pid_owned = resolve_project_id(project);
    let pid = pid_owned.as_str();

    // Step 1: Persist non-trivial prompts (best-effort, never blocks nudge)
    if let Some(prompt) = &user_prompt {
        if prompt.len() >= protocol::MIN_PROMPT_LENGTH && !protocol::is_trivial_prompt(prompt) {
            let should_save = std::env::var("RUNAR_SAVE_PROMPTS")
                .map(|v| v != "false")
                .unwrap_or(true);
            if should_save {
                let _ = persist_user_prompt(prompt, Some(pid)).await;
            }
        }
    }

    let data = protocol::read_ping(pid);

    // No ping file yet → first-message reminder
    if data.last_ping.is_none() && data.last_save.is_none() {
        if silent {
            emit_additional_context(&protocol::first_message_reminder());
        }
        return;
    }

    // Nudge timer: last_save is the real signal; fall back to last_ping
    let ref_ts = data.last_save.or(data.last_ping).unwrap_or(0);
    let elapsed = protocol::now_ms() - ref_ts;
    if elapsed < protocol::NUDGE_THRESHOLD_MS {
        return; // recent save — no nudge
    }

    // Don't nudge brand-new sessions
    if let Some(started) = data.session_started_at {
        if protocol::now_ms() - started < protocol::MIN_SESSION_AGE_MS {
            return;
        }
    }

    // Only nudge when files were tracked (something actually happened)
    if data.files_modified.is_empty() || !silent {
        return;
    }

    let minutes = elapsed / 60_000;
    emit_additional_context(&protocol::idle_nudge_message(minutes, &data.files_modified));
}

fn emit_additional_context(s: &str) {
    let payload = serde_json::json!({ "additionalContext": s });
    print!("{payload}");
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

async fn persist_user_prompt(prompt: &str, project_id: Option<&str>) -> anyhow::Result<()> {
    let pid = project_id.unwrap_or("default");
    let librarian = create_librarian_for_hook(pid).await?;
    let title = if prompt.len() > 100 {
        format!("{}...", &prompt[..97])
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
        content: prompt.to_string(),
        entry_type: types::EntryType::UserPrompt,
        source: Some(types::MemorySource::Human),
        tags,
        project_id: project_id.map(|s| s.to_string()),
        ..Default::default()
    };
    let _ = librarian.propose(input).await?;
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
        Commands::Search { query, limit } => {
            let librarian = create_librarian().await?;
            let results = librarian
                .search(&query, limit, None, None, None, None)
                .await?;

            if results.is_empty() {
                println!("No results found.");
            } else {
                for entry in &results {
                    println!("─── {} [{}]", entry.title, entry.entry_type.as_str());
                    let preview = if entry.content.len() > 200 {
                        format!("{}...", &entry.content[..200])
                    } else {
                        entry.content.clone()
                    };
                    println!("    {preview}");
                    println!();
                }
                println!("{} result(s)", results.len());
            }
        }
        Commands::Stats => {
            let librarian = create_librarian().await?;
            let stats = librarian.get_stats(None).await?;

            println!("Memory entries: {}", stats.total_entries);
            println!("Sessions:       {}", stats.total_sessions);
            if !stats.entries_by_type.is_empty() {
                println!("\nBy type:");
                for (t, count) in &stats.entries_by_type {
                    println!("  {t}: {count}");
                }
            }
            if !stats.namespaces.is_empty() {
                println!("\nNamespaces: {}", stats.namespaces.join(", "));
            }
        }
        Commands::Crawl {
            path,
            project,
            mode,
        } => {
            let librarian = create_librarian().await?;
            let root = std::path::Path::new(&path).canonicalize()?;

            let crawl_mode = huginn::CrawlMode::parse(&mode);
            println!(
                "Crawling {} (project: {project}, mode: {mode})...",
                root.display()
            );
            let result =
                huginn::crawl_project_with_mode(&root, &project, &librarian, crawl_mode).await?;

            println!("\nCrawl complete:");
            println!("  Total files:    {}", result.total_files);
            println!("  Deep analysis:  {}", result.analyzed_deep);
            println!("  Medium:         {}", result.analyzed_medium);
            println!("  Light:          {}", result.analyzed_light);
            println!("  Skipped:        {}", result.skipped);
            println!("  Patterns found: {}", result.patterns_found);
            println!("  Tech debt:      {}", result.techdebt_markers);
            println!("  Entries saved:  {}", result.entries_saved);
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
            configure,
        } => {
            let key = tool.to_lowercase();
            match key.as_str() {
                "claude-code" => {
                    if configure {
                        config_cmd::cmd_wizard()?;
                        println!();
                    }
                    let project_id = project.unwrap_or_else(setup::detect_project_id);
                    let result = setup::setup_claude_code(&project_id, with_auto_capture)?;
                    println!("\nRunarForge — Claude Code Setup\n");
                    println!(
                        "  MCP server configured in {}:",
                        result.claude_json_path.display()
                    );
                    println!("     muninn (unified — includes huginn + curator tools)\n");
                    println!("  Hooks configured in {}:", result.settings_path.display());
                    println!(
                        "     PreToolUse:        context injection (--project {})",
                        result.project_id
                    );
                    println!("     PostToolUse:       session ping on file writes");
                    println!("     PostToolUse:       save-ack on muninn_save");
                    println!("     PostToolUse:       rule-based extract (passive learning)");
                    if with_auto_capture {
                        println!("     PostToolUse:       enqueue (auto-capture queue)");
                        println!("     SessionEnd:        summarize (drain queue → summary)");
                    }
                    println!("     UserPromptSubmit:  idle nudge reminder\n");
                    println!("  Binary: {}", result.binary_path);
                    println!(
                        "  Memory protocol added to {}\n",
                        result.claude_md_path.display()
                    );
                    println!("  Project: {}\n", result.project_id);
                    if with_auto_capture {
                        println!("  Auto-capture: ENABLED");
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
                        println!("  Auto-capture: disabled (re-run with --with-auto-capture to enable)\n");
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
            // Memory Protocol + context packet injection (PreToolUse hook payload).
            // Must NEVER panic: hook failure would break Claude Code tool calls.
            if hooks_runtime::hooks_disabled() {
                if silent {
                    print!("{{\"additionalContext\":\"\"}}");
                }
                return Ok(());
            }
            let resolved = Some(resolve_project_id(project));
            let work = build_context(&resolved);
            let full = match tokio::time::timeout(hooks_runtime::hook_budget(), work).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    if !silent {
                        eprintln!("context error: {e}");
                    }
                    String::new()
                }
                Err(_) => {
                    hooks_runtime::append_hook_log("context", "budget exceeded");
                    String::new()
                }
            };
            if silent {
                let payload = serde_json::json!({ "additionalContext": full });
                print!("{payload}");
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
                let preview = if e.content.len() > 400 {
                    format!("{}...", &e.content[..400])
                } else {
                    e.content.clone()
                };
                for line in preview.lines() {
                    println!("  {line}");
                }
                println!();
            }
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

        Commands::Gc { project, dry_run } => {
            run_gc(project, dry_run).await?;
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
