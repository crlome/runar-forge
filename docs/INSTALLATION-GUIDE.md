# RunarForge --- Installation & Configuration Guide

How to install Huginn & Muninn and configure it for any project.

> RunarForge ships as a single-binary Rust CLI (`runar`). There is no
> Node.js, pnpm, or build step. You download one file and go.

---

## Prerequisites

| Requirement | When you need it | Check |
|---|---|---|
| `runar` binary | Always | `runar --version` |
| Docker | Only for the PostgreSQL backend | `docker ps` |
| Git | Optional (for `session ping` file tracking in projects) | `git --version` |
| `git config user.name` | Optional — required for per-dev attribution on saves (Phase 5.7). Name only, never email — email would land in shared remote PG as PII. | `git config --global user.name` |

You do **not** need Node.js, pnpm, Python, or a compiler to run `runar`.

> **Multi-dev / shared remote PG?** Set `git config --global user.name "Your Name"` before
> first save. Without it entries land anonymously (`author = NULL`). `runar doctor`
> reports identity status. Existing rows stay valid — column is nullable.

---

## 1. Install the binary

> **Upgrading from a previous build?** See
> [INSTALL-UPGRADE-RUNAR.md](./INSTALL-UPGRADE-RUNAR.md) — covers the
> `mv`-not-`cp` rule, ad-hoc codesign, quarantine strip, and rollback
> hygiene. Skipping those steps causes the "frozen Claude Code" bug.

### From a release

Download the binary for your platform from the project releases page and put
it on your `PATH`:

```bash
# macOS / Linux
curl -L <release-url>/runar -o /usr/local/bin/runar
chmod +x /usr/local/bin/runar

# Windows — download runar.exe and add its directory to PATH
```

GitHub Actions (`.github/workflows/release.yml`) builds five targets
per `v*` tag (Linux x86_64, Linux aarch64, macOS aarch64, macOS x86_64,
Windows x86_64) and attaches the tarballs / zip plus sha256 checksums
to the GitHub Release. Both macOS binaries are ad-hoc codesigned so
Gatekeeper will not SIGKILL them on first run.

**macOS download (Apple Silicon):**

```bash
curl -L https://github.com/crlome/runar-forge/releases/download/v0.5.0/runar-aarch64-apple-darwin.tar.gz \
  | tar -xzf - -C /tmp
sudo mv /tmp/runar /usr/local/bin/runar
runar --version
```

### From source

If you have Rust installed:

```bash
git clone https://github.com/crlome/runar-forge.git
cd runar-forge
cargo build --release -p runar-muninn
# Use `mv`, not `cp`: cp overwrites in place and SIGKILLs any runar
# MCP process currently reading the old inode (freezes Claude Code).
# `mv` swaps the inode atomically so live processes keep running their
# original binary until exit.
sudo mv target/release/runar /usr/local/bin/runar
```

### Stable hook path (auto-managed)

Once `runar` is on PATH, `runar setup claude-code` mirrors the resolved
binary into `~/.runar-forge/bin/runar` via tempfile + atomic rename. All
hooks point at this stable path, never at `~/.cargo/bin/runar` or
`/usr/local/bin/runar`. This means:

- `cargo install` rewriting the cargo bin no longer races a running CC
  session — hooks keep using the stable copy until the next setup.
- `runar update` (see §8) installs new versions through the same atomic
  rename. The previous binary is preserved at `runar.previous` for
  one-syscall rollback.
- Recommended PATH (add to `~/.zshrc` or `~/.bashrc`):

  ```bash
  export PATH="$HOME/.runar-forge/bin:$PATH"
  ```

  Then you can `rm ~/.cargo/bin/runar` (or `/usr/local/bin/runar`) and
  the stable path takes over. Existing installations keep working
  because `runar setup claude-code` always writes hooks against
  `~/.runar-forge/bin/runar`.

**macOS only — avoid Gatekeeper SIGKILL on a freshly copied binary:**

```bash
# Ad-hoc codesign (preferred for local dev)
codesign --force --sign - /usr/local/bin/runar

# Or strip the quarantine xattr if the binary came from a download
xattr -d com.apple.quarantine /usr/local/bin/runar 2>/dev/null || true
```

### Embedding provider

Muninn reads `RUNAR_EMBEDDINGS` to pick how vectors are produced:

| Value | Behavior |
|---|---|
| `disabled` | FTS-only retrieval. No network, no API key. |
| `openai` | Calls the OpenAI embeddings API (`OPENAI_API_KEY` required). |
| `local` | Local ONNX model via fastembed (default: `all-MiniLM-L6-v2`, 384-dim). First run downloads weights to `$HOME/.runar-forge/models/` (override with `RUNAR_MODELS_DIR`). Requires the binary to be built with the `local-embeddings` Cargo feature. |

Unset: auto-pick `openai` if `OPENAI_API_KEY` is set, else `disabled`.

Source build with local embeddings:

```bash
cargo build --release --features local-embeddings
```

### Storage reachability

Muninn applies `RUNAR_DB_CONNECT_TIMEOUT_MS` (default **8000 ms**) to both
`storage.initialize()` and every PostgreSQL pool acquire. If the DB is
unreachable the MCP server boots in **degraded mode**: handshake succeeds,
`tools/list` returns empty, and `tools/call` returns a structured
`STORAGE_UNAVAILABLE` error. Claude Code renders the server as degraded
instead of hanging. Set a lower value (e.g. `500`) for CI smoke tests.

### Verify

```bash
runar --version
# runar 0.3.0
```

---

## Storage modes — pick one

`runar` runs in one of **5 storage modes**. Pick before initializing.

| # | Mode | Env config | When to use |
|---|---|---|---|
| 1 | **sqlite standalone** | `RUNAR_STORAGE=sqlite` | Solo dev. Zero infra. Single machine. |
| 2 | **local PG standalone** | `RUNAR_STORAGE=postgresql` + `RUNAR_DB_URL=...localhost...` | Solo dev who wants pgvector locally. Still single machine. |
| 3 | **remote PG standalone** | `RUNAR_STORAGE=postgresql` + `RUNAR_DB_URL=...remote...` | Team shares one PG. **Warning**: every hook crosses internet (~200 ms each). Painful. Use mode 4 or 5 instead. |
| 4 | **sqlite local + remote PG sync** ⭐ | `RUNAR_STORAGE_LOCAL=sqlite` + `RUNAR_STORAGE_REMOTE=postgresql://...remote...` | Recommended for laptops on a team. Hot path local-fast (~6 ms); central knowledge in remote PG. Phase 5.6. See §12. |
| 5 | **local PG + remote PG sync** | `RUNAR_STORAGE_LOCAL=postgresql` + `RUNAR_DB_URL=...local...` + `RUNAR_STORAGE_REMOTE=postgresql://...remote...` | Power users who want pgvector locally AND remote sync. Heavier infra than 4. |

**Detection logic** (auto-applied by `runar` at startup):

- Both `RUNAR_STORAGE_LOCAL` and `RUNAR_STORAGE_REMOTE` set → **hybrid (mode 4 or 5)**.
- Otherwise read `RUNAR_STORAGE` → **standalone (mode 1, 2, or 3)**.
- Neither set → defaults to mode 2 against `127.0.0.1:5433`.

**Not supported**:
- ❌ Remote sqlite — sqlite is always file-local.
- ❌ Multi-remote fanout — one remote PG per local install.
- ❌ Active-active pg↔pg — sync is async one-way reconcile, not multi-master.
- ❌ sqlite↔sqlite sync — remote must be PG.

The rest of this guide:
- **§2** — start PostgreSQL (modes 2-5)
- **§3** — initialize `.env` (any mode)
- **§4** — wire Claude Code (any mode)
- **§11** — migrating PostgreSQL between hosts
- **§12** — full hybrid (modes 4-5) setup, conflict matrix, troubleshooting

---

## 2. Start PostgreSQL (optional)

RunarForge supports two storage backends:

- **PostgreSQL** (default, recommended for teams) — via Docker, data shared across machines
- **SQLite** (zero-dependency) — data in `~/.runar-forge/memory.db`

For SQLite (`runar init --storage sqlite`), skip to step 3.

For PostgreSQL, start Docker Compose from the runar-forge repo root:

```bash
cd /path/to/runar-forge
docker compose --profile postgresql up -d
```

This starts a PostgreSQL 16 container with pgvector on port `5433`.

Default credentials (from `.env.example`):
```
User:     runar
Password: runar_password
Database: runar_memory
URL:      postgresql://runar:runar_password@localhost:5433/runar_memory
```

Verify:
```bash
docker compose ps   # postgres should show "running (healthy)"
```

---

## 3. Initialize RunarForge

```bash
runar init                         # postgresql (default)
runar init --storage sqlite        # SQLite (zero-dependency)
runar init --interactive           # TUI wizard (detects Docker, OPENAI_API_KEY, git remote)
```

This writes `~/.runar-forge/.env` with your storage configuration. The CLI
loads this file at process startup (via `dotenvy::from_path`), so every
`runar` invocation — including the MCP server and every PreToolUse /
PostToolUse hook — sees the same env. Shell exports and
`~/.claude.json`'s `mcpServers.muninn.env` still take precedence:
variables already present in the process env are not overwritten.

Manually edit the file if you need to change anything:

```bash
# ~/.runar-forge/.env
RUNAR_STORAGE=postgresql
RUNAR_DB_URL=postgresql://runar:runar_password@127.0.0.1:5433/runar_memory
```

> **Why `127.0.0.1` instead of `localhost`?** On macOS with Docker
> Desktop, `localhost` resolves to IPv6 `::1` first, but the published
> PostgreSQL port is forwarded only on IPv4. `127.0.0.1` avoids the
> silent connect failure that looks like `pool error: db error`.
> Linux and Windows Docker Desktop forward IPv4-only by default too,
> so `127.0.0.1` is the portable safe default.

### Optional: enable OpenAI embeddings

Add to `~/.runar-forge/.env`:

```bash
OPENAI_API_KEY=sk-...
```

When present, `runar` uses OpenAI embeddings for semantic search. When
absent, it falls back to SQLite FTS / PostgreSQL keyword search. For an
offline alternative, set `RUNAR_EMBEDDINGS=local` (requires a build with
the `local-embeddings` Cargo feature — see §1).

---

## 4. Configure Claude Code

Navigate to your project directory and run:

```bash
cd /path/to/your-project
runar setup claude-code
```

With a custom project ID:

```bash
runar setup claude-code --project my-project
```

Turn on Phase 5.3 full auto-capture (PostToolUse enqueue + SessionEnd
LLM summarizer) at the same time:

```bash
runar setup claude-code --project my-project --with-auto-capture
```

When the flag is present, two extra hooks are installed:
`PostToolUse:Write|Edit|Create|MultiEdit|Bash → runar enqueue` and
`SessionEnd:.* → runar summarize`. Leave it off and Muninn still runs
the Phase 5.3 rule-based extract + session rotation + user-prompt
persist paths — auto-capture is gradual, not all-or-nothing.

If `--project` is omitted, `runar` auto-detects from the git remote URL or
the current directory name.

### What `runar setup claude-code` does

**4a. MCP server → `~/.claude.json`**

Registers **one unified MCP server** globally (the Rust binary exposes all
23 tools — muninn, huginn, curator — from a single process):

```json
{
  "mcpServers": {
    "muninn": {
      "command": "/full/path/to/runar",
      "args": ["mcp-muninn"]
    }
  }
}
```

Legacy separate `huginn` / `curator` servers from prior TypeScript setups
are removed automatically.

**4b. Hooks → `.claude/settings.json` (per-project)**

The base setup installs **five** hook entries using the full binary
path (via `which runar`). With `--with-auto-capture` on, **two more**
are added (`PostToolUse:runar enqueue` + `SessionEnd:runar summarize`).

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": ".*",
        "hooks": [{ "type": "command",
                    "command": "'~/.runar-forge/bin/runar' context --silent --project '<id>' 2>>'~/.runar-forge/hook.log' ; exit 0" }] }
    ],
    "PostToolUse": [
      { "matcher": "Write|Edit|Create|MultiEdit",
        "hooks": [{ "type": "command",
                    "command": "'~/.runar-forge/bin/runar' session ping --silent --project '<id>' 2>>'~/.runar-forge/hook.log' ; exit 0" }] },
      { "matcher": "mcp__muninn__muninn_save",
        "hooks": [{ "type": "command",
                    "command": "'~/.runar-forge/bin/runar' save-ack --silent --project '<id>' 2>>'~/.runar-forge/hook.log' ; exit 0" }] },
      { "matcher": "Write|Edit|Create|MultiEdit|Bash",
        "hooks": [{ "type": "command",
                    "command": "'~/.runar-forge/bin/runar' extract --silent --project '<id>' 2>>'~/.runar-forge/hook.log' ; exit 0" }] }
    ],
    "UserPromptSubmit": [
      { "matcher": ".*",
        "hooks": [{ "type": "command",
                    "command": "'~/.runar-forge/bin/runar' nudge --silent --project '<id>' 2>>'~/.runar-forge/hook.log' ; exit 0" }] }
    ]
  }
}
```

**Hook safety contract** (Phase 5.5.4 hardening):

- Stderr redirected to `~/.runar-forge/hook.log` (rotated at 1 MiB) — no
  more `2>/dev/null` swallowing diagnostics.
- Trailing `; exit 0` — every hook returns zero so a missing/stale
  binary cannot block CC.
- Each invocation is wall-clock-capped via `RUNAR_HOOK_BUDGET_MS`
  (default **800 ms**, SessionEnd gets 4×) — a slow PG cannot freeze
  the editor.
- DB connect short-circuits via the per-project DB breaker after 2
  consecutive failures (`~/.runar-forge/db-breaker-<pid>.json`,
  60-s trip). Refused/unreachable PG: ~10 ms × N hooks instead of 8 s × N.
- Kill-switch: `RUNAR_DISABLE_HOOKS=1` env or
  `touch ~/.runar-forge/.disable-hooks` — every hook returns
  immediately. Use this if anything ever freezes again.

With `--with-auto-capture`, two additional entries land under
`PostToolUse` (another `Write|Edit|Create|MultiEdit|Bash` matcher running
`runar enqueue`) and a new `SessionEnd` section runs `runar summarize`
on `.*`. See § "Phase 5.3 auto-capture" below for how those two hooks
interact with the `pending_observations` queue + the summarizer backend.

Re-running `runar setup claude-code` is idempotent — existing runar hook
entries are filtered out before fresh ones are added, so re-runs never
duplicate.

**4c. CLAUDE.md → Memory section**

Appends a short `## Memory` section pointing to the dynamic hook system.
The actual save instructions are injected dynamically via the PreToolUse
hook — no manual CLAUDE.md editing needed.

### Verify the setup

```bash
cat ~/.claude.json | python3 -m json.tool | grep muninn
cat .claude/settings.json
# Should show 5 hook entries (1 PreToolUse, 3 PostToolUse, 1 UserPromptSubmit)

runar doctor
# Runs 13+ read-only checks: env file, storage, db reachable, auth,
# pgvector, schema, migrations, row counts, breaker state, kill-switch,
# hook log (tail of last 5 entries), project-local .env, sync state +
# heartbeat (hybrid mode), author identity (Phase 5.7 — warns when
# `git config user.name` is unset).
```

### Other editors (VSCode / OpenCode / Codex / Cursor / Windsurf)

These tools don't support per-project hooks (context injection and
auto-capture are Claude Code only), but they all consume the same
`runar mcp-muninn` stdio server. `vscode`, `opencode`, and `codex`
auto-write their config file; `cursor` and `windsurf` print it for you
to paste manually:

```bash
runar setup vscode       # writes .vscode/mcp.json   (servers.muninn)
runar setup opencode     # writes opencode.json      (mcp.muninn, type=local)
runar setup codex        # writes ~/.codex/config.toml ([mcp_servers.muninn])
runar setup cursor       # prints MCP config for manual paste
runar setup windsurf     # prints MCP config for manual paste
```

`vscode`/`opencode` write into the current directory (workspace-local);
`codex` writes the global `~/.codex/config.toml`. All merges are
idempotent and preserve existing servers, keys, and comments.

---

## 5. Crawl your project

Initial crawl analyzes the codebase and creates memory entries:

```bash
runar crawl /path/to/your-project --project your-project
```

### Crawl modes

| Mode | Flag | When to use |
|---|---|---|
| **auto** | default | First crawl or routine re-crawl. Picks `full` if no prior crawl state exists, `incremental` otherwise. |
| **incremental** | `--mode incremental` | After minor code changes. Only re-analyzes files that changed since the last crawl. Uses `git diff` or content hashes. |
| **full** | `--mode full` | Force re-analyze everything. Use after major refactors or dependency upgrades. |

Focus to a subdirectory:

```bash
runar crawl . --project my-project --focus src/api/
```

### What the crawler produces

- Per-file analysis entries (imports, exports, key symbols, tech-debt markers)
- Cross-file pattern entries (12 detectors: auth, data-access, DI,
  middleware, validation, logger, cache, queue, event-bus, error-handling,
  factory, config)
- Architecture summary (languages, frameworks, entry points)
- Tech-debt inventory (TODO / FIXME / HACK / XXX markers)
- Crawl state entry (drives incremental mode)

Verify:

```bash
runar stats
# (runar_status MCP tool for per-project breakdown — CLI wrapper coming in Phase B)
```

---

## 6. Verify everything works

### Test memory context injection (what the PreToolUse hook runs)

```bash
runar context --project your-project
# Should print "## Muninn Memory Protocol" + memory packet

runar context --silent --project your-project
# JSON — {"additionalContext": "..."} — exactly what the hook emits
```

### Test the nudge system

```bash
rm -f "$TMPDIR/runar-ping-your-project"    # force first-message branch
runar nudge --silent --project your-project
# → emits CRITICAL FIRST ACTION reminder
```

### Test save acknowledgment

```bash
runar save-ack --silent --project your-project
cat "$TMPDIR/runar-ping-your-project"
# Should show {"lastSave": <timestamp>}
```

### Test search

```bash
runar search "authentication" --limit 5
```

### Test benchmark

```bash
runar benchmark --project your-project --mode quick
# Runs 9 Curator questions, scores memory quality
```

### Test auto-capture pipeline (Phase 5.3, only if `--with-auto-capture` was used)

```bash
# 1. Enqueue a synthetic tool event
cat <<'EOF' | runar enqueue --silent --project your-project
{"tool_name":"Edit","tool_input":{"file_path":"/tmp/verify.ts","old_string":"a","new_string":"b"},"tool_response":{}}
EOF

# 2. Drain the queue + write a session summary
runar summarize --project your-project
# → "Summarized N observation(s) via heuristic for <id>; synthesized M entry(s)."
```

### Test tier GC (Phase 5.4)

```bash
runar gc --project your-project --dry-run    # preview planned transitions
runar gc --project your-project              # apply + evict
```

### Start Claude Code

Open Claude Code in your project directory. The PreToolUse hook fires on
every tool call, injecting the Memory Protocol + memory packet. Look for
"Muninn Memory Protocol" and "PROACTIVE SAVE" in system reminders.

---

## 7. How hooks work

Four hooks, all fired by Claude Code automatically:

### PreToolUse — Memory Protocol + Context Injection
**Fires:** Before every Claude Code tool call (`matcher: .*`).

**What it does:**
1. Reads the per-project ping file (tracked files, session timestamps)
2. Builds the **Memory Protocol** — proactive save instructions
3. Fetches the memory packet (recent sessions + top entries + stats)
4. Emits `{"additionalContext": "<protocol>\n\n<packet>"}` to stdout

**Why every tool call?** Context compaction compresses CLAUDE.md. The
PreToolUse hook re-injects on every call — the protocol always survives.

### PostToolUse (ping) — File Tracking
**Fires:** After Write / Edit / Create / MultiEdit operations.

**What it does:**
1. Runs `git diff --name-only HEAD` (max 20 files, 5s timeout)
2. Merges cumulatively with the existing tracked set
3. Updates `lastPing` + `sessionStartedAt` in the ping file

### PostToolUse (save-ack) — Save Tracking
**Fires:** After Claude calls `muninn_save` (matcher `mcp__muninn__muninn_save`).

**What it does:** Updates `lastSave` in the ping file. Resets the nudge timer.

### UserPromptSubmit (nudge) — Idle Reminder
**Fires:** Before each user prompt.

**What it does:**
1. Reads stdin (hook payload with `user_prompt`, ≤ 2s timeout)
2. If prompt length ≥ 20 chars and non-trivial → persists it as a
   `user-prompt` memory entry (can disable with `RUNAR_SAVE_PROMPTS=false`)
3. If no ping file (first message): emits CRITICAL FIRST ACTION reminder
4. If `lastSave` > 15 min ago AND session > 5 min old AND files tracked:
   emits MEMORY REMINDER with file context
5. Otherwise: silent

### Flow diagram

```
[User sends prompt]
  → UserPromptSubmit hook: nudge
      → persist user prompt (non-trivial only)
      → emit reminder if idle
  → [Claude processes]
    → [Claude calls a tool]
      → PreToolUse hook: Memory Protocol + context injection
      → [Tool executes]
      → PostToolUse hook:
          Write/Edit/Create/MultiEdit → session ping (track files)
          mcp__muninn__muninn_save    → save-ack (reset nudge timer)
    → [Claude self-checks: "Did I make a decision?" → muninn_save]
      → PostToolUse → save-ack
```

---

## 8. CLI reference

### System

```bash
runar --version                          # show version
runar stats                              # memory counts + namespaces
runar init [--storage sqlite|postgresql] # write ~/.runar-forge/.env
runar setup claude-code [--project <id>] # configure MCP + hooks + CLAUDE.md
runar setup vscode                       # write .vscode/mcp.json
runar setup opencode                     # write opencode.json
runar setup codex                        # write ~/.codex/config.toml
runar setup cursor                       # print Cursor MCP config
runar setup windsurf                     # print Windsurf MCP config
runar doctor [--db] [--json] [--quiet] [--timeout-ms <n>]
  # Read-only validation of config + storage. Includes hook log tail
  # and kill-switch state. Exit non-zero on any failure.
runar update [--check] [--channel stable|beta] [--force] [--rollback]
  # Self-update via release manifest. Atomic install to
  # ~/.runar-forge/bin/runar; previous binary kept as runar.previous.
  # Refuses mid-CC session unless --force. Reads
  # RUNAR_UPDATE_MANIFEST_URL for the manifest endpoint.
```

### Updating

```bash
# 1. Check for a new version (no download)
runar update --check

# 2. Recommended pre-swap dance — neuters hooks while the binary swaps
touch ~/.runar-forge/.disable-hooks
runar update
rm ~/.runar-forge/.disable-hooks
runar doctor

# 3. Roll back if anything regresses
runar update --rollback        # swaps runar.previous back into place
```

The flow exists because `runar update` is atomic on its own (tempfile +
rename keeps any running MCP process alive on the old inode), but
disabling hooks during the swap window prevents partial-state weirdness
in CC sessions that fire hooks every ~100 ms.

### Hook safety knobs

| Env / file | Default | Effect |
|---|---|---|
| `RUNAR_DISABLE_HOOKS=1` | unset | Every hook subcommand exits 0 immediately |
| `~/.runar-forge/.disable-hooks` | absent | Same as above; survives shell restarts |
| `RUNAR_HOOK_BUDGET_MS` | `800` | Wall-clock cap per hook (SessionEnd × 4) |
| `RUNAR_DB_CONNECT_TIMEOUT_MS` | `8000` | Per pool acquire / `initialize()` |
| `RUNAR_UPDATE_MANIFEST_URL` | unset | Override release-manifest URL |
| `~/.runar-forge/hook.log` | rotated 1 MiB | Hook stderr lands here, not `/dev/null` |
| `~/.runar-forge/db-breaker-<pid>.json` | absent | Per-project DB connectivity breaker state (auto-managed) |

### Memory (Muninn)

```bash
runar search <query> [--limit <n>]       # keyword + semantic search
runar context [--project <id>] [--silent]
  # Full PreToolUse payload (Memory Protocol + packet).
  # --silent emits {"additionalContext": "..."} JSON for hooks.
```

### Scout (Huginn)

```bash
runar crawl <path> --project <id> [--mode full|incremental|auto] [--focus <dir>]
runar benchmark --project <id> [--mode quick|full]
runar architecture --project <id>
runar techdebt --project <id> [--type todo|fixme|hack|xxx|all]
```

### Curator

```bash
runar ask <question> [--project <id>]
runar onboard [--project <id>] [--json]
```

### Hook commands

```bash
runar nudge        [--project <id>] [--silent]   # UserPromptSubmit hook
runar save-ack     [--project <id>] [--silent]   # PostToolUse on muninn_save
runar session ping [--project <id>] [--silent]   # PostToolUse on file writes
runar extract      [--project <id>] [--silent]   # PostToolUse rule-based extract (Phase 5.3)
runar enqueue      [--project <id>] [--silent]   # PostToolUse auto-capture queue (Phase 5.3 opt-in)
runar summarize    [--project <id>] [--silent]   # SessionEnd drain + summarize (Phase 5.3 opt-in)
```

All accept `--silent` for hook consumption.

### Phase 5.3 — Auto-capture pipeline

When `--with-auto-capture` is on, Claude-Code hooks drive this flow:

1. **PostToolUse** on `Write|Edit|Create|MultiEdit|Bash` → `runar enqueue`
   appends each tool call (minus mcp__muninn__ calls) to
   `muninn.pending_observations` with a 30-sec SHA256 dedup window.
2. **SessionEnd** on `.*` → `runar summarize` claims all pending rows
   for the active session, asks the summarizer to synthesize a
   structured summary + 0-5 memory entries, and confirms the rows on
   success. If the summarizer fails, rows stay `processing` and are
   recovered by the next `run_summarize` call (60-sec stale window).
3. **Circuit breaker** (`~/.runar-forge/breaker-<project>.json`) wraps
   the summarizer call. 3 consecutive failures trip it for 60s; in that
   state the summarize hook returns without attempting the API.

The summarizer picks `claude-haiku-4-5` when `ANTHROPIC_API_KEY` is
present, otherwise runs a deterministic heuristic that summarizes tool
counts + file paths without synthesizing observations.

### Phase 5.4 — Tier GC

```bash
runar gc [--project <id>] [--dry-run]
```

Graduates entries across `WORKING → EPISODIC → SEMANTIC → ARCHIVAL`
based on (priority order):

1. **Verified fast-promote** — `verified=true` entries jump from
   `WORKING` straight to `SEMANTIC`.
2. **Hebbian citation bump** — `access_count ≥ RUNAR_TIER_CITATION_THRESHOLD`
   entries below `SEMANTIC` promote one layer.
3. **Low-confidence aggressive demote** — entries with
   `confidence < RUNAR_TIER_LOW_CONFIDENCE` older than 2× their normal
   graduation threshold fast-track to `ARCHIVAL`.
4. **Standard age ladder** — idle time since last access.

After graduation, `evict_stale` soft-deletes `ARCHIVAL` + unverified +
zero-access + low-confidence entries older than
`RUNAR_TIER_EVICTION_AGE_DAYS` (capped at
`RUNAR_TIER_EVICTION_MAX_PER_RUN` per pass). Verified entries are
**never** evicted. `--dry-run` previews planned transitions without
mutating the store. GC also fires automatically at the tail of
`runar summarize` when auto-capture is on.

### Portability — export/import

```bash
runar export [--project <id>] [--type <entry-type>] [--output <file>] [--limit 100000]
runar import <file>
```

Dumps memory entries + edges + sessions as JSONL envelopes
(`{"kind":"entry|edge|session","data":{…}}`). Import is id-preserving
(`ON CONFLICT DO NOTHING` / `INSERT OR IGNORE`) so supersession edges
survive a roundtrip. Legacy entries-only JSONL from Phase 5.3 A6.1
still imports.

### Human verification

The `muninn_verify` MCP tool flips `verified=true` + `verified_at=now`
on an entry and grants a 1.25× rank bonus in fused search. Use to
curate agent-generated entries that are particularly valuable — they
survive eviction and fast-promote through tiers. Phase 5.7 also
records `verified_by` (resolved from `git config user.name`) so teams
can see who endorsed each verified entry.

### Per-dev attribution (Phase 5.7)

Every save stamps `author` from `git config user.name` (name only —
email is PII and would land in shared remote PG). `mark_verified`
records `verified_by` independently. Both are nullable, both flow
through the sync outbox JSONB payload so attribution survives push/pull.
Unset `user.name` → entries land with `author=NULL` (still valid). To
backfill identity:

```bash
git config --global user.name "Your Name"
runar doctor   # check #11 confirms resolved author
```

`muninn_search` accepts an optional `author` arg (case-insensitive
substring match on the name). `muninn_save` accepts an optional
`author` override when an agent saves on behalf of a named user.

### `<private>` redaction

Any `<private>…</private>` block in `title` or `content` is stripped to
`[redacted]` before storage, and the entry is tagged `redacted` for
audit. Case-insensitive, multiple blocks supported, unterminated tags
redact through end-of-input. **Best-effort hygiene, not a security
boundary** — a motivated attacker with MCP access can still cause
unredacted saves.

### MCP server

```bash
runar mcp-muninn   # Single unified MCP stdio server.
                   # Exposes 25 tools: 13 muninn + 5 curator + 7 huginn.
                   # Called by AI tools via ~/.claude.json — not manually.
```

On consecutive storage-layer failures (3 in a row), MCP dispatch trips
a breaker and tool calls return `{"error":true,"code":"MUNINN_DEGRADED"}`
for 60s instead of failing per-tool. First successful call resets.

---

## 9. Setting up a new project (quick reference)

```bash
# 1. Navigate to your project
cd /path/to/my-project

# 2. Configure Claude Code hooks
runar setup claude-code --project my-project

# 3. Crawl the codebase
runar crawl . --project my-project

# 4. Verify
runar context --project my-project    # should show Memory Protocol + packet
runar stats

# 5. Restart Claude Code
#    Hooks fire automatically. Claude sees the Memory Protocol and saves
#    proactively.
```

---

## 10. Troubleshooting

### Claude Code or VSCode freezes / lags after install or update

Symptoms: typing a prompt or running a tool stalls for many seconds; the
issue recurs after every `cargo install` / reinstall.

**Immediate relief — kill-switch:**

```bash
touch ~/.runar-forge/.disable-hooks      # editor responsive again
```

Then diagnose:

```bash
runar doctor                             # 12+ checks; tail of hook.log
tail -50 ~/.runar-forge/hook.log
ls ~/.runar-forge/db-breaker-*.json      # any per-project DB breakers tripped?
```

Common root causes (all addressed since Phase 5.5.4):

- **Slow / unreachable PG** — hook budget caps each invocation at
  `RUNAR_HOOK_BUDGET_MS` (default 800 ms). If `hook.log` shows repeated
  `create_librarian timed out` entries, fix `RUNAR_DB_URL` or run
  `runar doctor` to confirm PG reachability.
- **Cargo install raced a running CC session** — hooks now point at the
  stable `~/.runar-forge/bin/runar` path. Re-run
  `runar setup claude-code --project <id>` after any `cargo install` to
  refresh the install. Future updates: prefer `runar update` over
  `cargo install`.
- **`runar config wizard` from inside CC** — the wizard requires a real
  TTY. Run it from a normal terminal, or use `runar config set KEY VALUE`
  for non-interactive edits.

When done diagnosing, re-enable hooks:

```bash
rm ~/.runar-forge/.disable-hooks
```

### `runar: command not found`

The binary isn't on `PATH`. Either:

```bash
# Option A — symlink the stable copy into an existing PATH dir
ln -sf ~/.runar-forge/bin/runar ~/.cargo/bin/runar

# Option B — add the stable bin dir to PATH (recommended; survives updates)
echo 'export PATH="$HOME/.runar-forge/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

### Hooks not firing in a project

```bash
cat /path/to/your-project/.claude/settings.json
```

If missing, run `runar setup claude-code --project <id>` from the project
directory. You should see 4 hook entries.

### `which runar` returns the wrong binary

The legacy TypeScript `runar` CLI was removed in the v0.2 cleanup, but a
stale `pnpm link` can still shadow the Rust binary. Check:

```bash
which -a runar
```

If the first hit is `~/.nvm/.../bin/runar`, drop the stale link
(`pnpm unlink --global @runar-forge/hm`) or put the Rust binary earlier
on `PATH`. The setup command embeds whatever `which runar` returns into
the hook config, so this matters.

### Memory count not growing

The Memory Protocol nudges Claude to save proactively. If entries aren't
growing:

1. Verify protocol injection: `runar context --project <id>` → should start
   with "## Muninn Memory Protocol". The protocol is rendered from pure
   strings and must appear even when the DB is unreachable — if it does
   not, the binary is older than the dotenvy fix; reinstall.
2. Check storage reachability. If `runar context --silent -p <id>` prints
   `muninn: storage unreachable — protocol only:` on stderr, PG is down
   or misconfigured. Fix: run `docker compose ps` (look for `healthy`),
   then verify `RUNAR_DB_URL` in `~/.runar-forge/.env` uses `127.0.0.1`
   and port `5433`. The protocol still reaches Claude in degraded mode,
   but without the memory packet (recent sessions + top entries).
3. Simulate stale save to test the nudge:
   ```bash
   python3 -c "
   import json, time, os
   f = os.path.join(os.environ.get('TMPDIR','/tmp'), 'runar-ping-<id>')
   d = json.load(open(f))
   d['lastSave'] = int(time.time()*1000) - 20*60*1000
   d['sessionStartedAt'] = int(time.time()*1000) - 30*60*1000
   d['filesModified'] = d.get('filesModified') or ['test.rs']
   open(f,'w').write(json.dumps(d))
   "
   runar nudge --silent --project <id>
   # Should emit MEMORY REMINDER
   ```
4. Restart Claude Code after updating hooks
5. Re-crawl: `runar crawl . -p <id>`

### PostgreSQL connection refused

```bash
cd /path/to/runar-forge
docker compose --profile postgresql up -d
docker compose ps  # check health status
```

Verify the port matches `RUNAR_DB_URL` (default compose uses `5433`, not
`5432`).

### Per-project entry counts

```sql
-- Run against your PG instance
SELECT project_id, COUNT(*)
FROM muninn.memory_entries
WHERE deleted_at IS NULL
GROUP BY project_id;
```

Re-crawling deprecates old non-file entries (patterns, stack facts) and
creates fresh ones. `deleted_at IS NULL` excludes deprecated entries.

---

## 11. Migrating PostgreSQL between hosts

Moving memory from a local PostgreSQL (e.g. the docker-compose default
on port `5433`) to a remote server, between major PG versions, or
between providers. Single source of truth in this guide; supersedes
ad-hoc `pg_dump | pg_restore` snippets.

### Prerequisites on the target

The target PostgreSQL host **must have `pgvector` installed at the
system level** before restore. Without it, `pg_restore` fails with:

```
ERROR:  extension "vector" is not available
HINT:  The extension must first be installed on the system where
       PostgreSQL is running.
```

Options to satisfy this:

- **Self-hosted target:** run the `pgvector/pgvector:pg18` image (or
  `pg16`/`pg17` for matching versions). Do not use stock `postgres:18`.
- **Managed (Supabase / Neon / Crunchy / RDS):** enable `vector`
  extension in the provider dashboard before running the migration.
- **Self-hosted custom build:** install `pgvector` per
  [pgvector docs](https://github.com/pgvector/pgvector#installation),
  then `CREATE EXTENSION vector;`.

### Version mismatch — use the newer client

If local and target run different major versions (e.g. local `pg16`,
target `pg18`), **use the newer version's `pg_dump` against the older
source**. `pg_dump` is forward-compatible. Stock `postgres` Docker
images contain matching client + server, so use the target-version
image for both dump and restore.

### End-to-end script

```bash
#!/usr/bin/env bash
set -euo pipefail

# --- source (local) ---
LOCAL_CONTAINER=<container-id>           # docker ps | grep runar-postgres
LOCAL_DB=runar_memory
LOCAL_USER=runar
LOCAL_PASSWORD=runar_password            # docker-compose default

# --- target (remote) ---
REMOTE_HOST=db.example.com
REMOTE_PORT=8102
REMOTE_DB=runar_memory
REMOTE_USER=runar
export PGPASSWORD="<remote-password>"

DUMP_IMAGE=pgvector/pgvector:pg18        # match remote major version

# 1. Ensure pgvector exists on remote
docker run --rm -e PGPASSWORD="$PGPASSWORD" "$DUMP_IMAGE" \
  psql -h "$REMOTE_HOST" -p "$REMOTE_PORT" \
       -U "$REMOTE_USER" -d "$REMOTE_DB" \
       -c "CREATE EXTENSION IF NOT EXISTS vector;"

# 2. Dump local + restore remote (single pipe, no temp file)
docker run --rm \
  --link "$LOCAL_CONTAINER":pgsrc \
  -e PGPASSWORD="$LOCAL_PASSWORD" \
  "$DUMP_IMAGE" \
  pg_dump -h pgsrc -U "$LOCAL_USER" -Fc "$LOCAL_DB" \
| docker run --rm -i \
    -e PGPASSWORD="$PGPASSWORD" \
    "$DUMP_IMAGE" \
    pg_restore -h "$REMOTE_HOST" -p "$REMOTE_PORT" \
               -U "$REMOTE_USER" -d "$REMOTE_DB" \
               --no-owner --no-acl --clean --if-exists
```

Notes:
- `--clean --if-exists` drops existing objects before restore. Drop
  the flags to append to a non-empty target.
- `--no-owner --no-acl` skips ownership / GRANT statements that
  would require matching roles on the target.
- `--link` is deprecated; if the local container runs on a custom
  Docker network, use `--network <net>` and pass the container name
  as the host instead.

### If pgvector is unavailable on the target

You can still migrate metadata, dropping the embedding column. Memory
search will fall back to FTS until you re-embed:

```bash
docker exec "$LOCAL_CONTAINER" pg_dump -U "$LOCAL_USER" -Fc \
  "$LOCAL_DB" > /tmp/dump.bin
pg_restore -l /tmp/dump.bin | grep -v -i vector > /tmp/restore.list
pg_restore -L /tmp/restore.list \
  -h "$REMOTE_HOST" -p "$REMOTE_PORT" \
  -U "$REMOTE_USER" -d "$REMOTE_DB" /tmp/dump.bin
```

After this, `embedding` columns are NULL. Re-embed by re-crawling
projects with `RUNAR_EMBEDDINGS=openai` (or your provider) once
`pgvector` is added to the target.

### Verify the restore

```bash
PGPASSWORD="$PGPASSWORD" psql -h "$REMOTE_HOST" -p "$REMOTE_PORT" \
  -U "$REMOTE_USER" -d "$REMOTE_DB" -c "
    SELECT schemaname, relname, n_live_tup
    FROM pg_stat_user_tables
    WHERE schemaname IN ('muninn','huginn','curator')
    ORDER BY n_live_tup DESC;"

PGPASSWORD="$PGPASSWORD" psql -h "$REMOTE_HOST" -p "$REMOTE_PORT" \
  -U "$REMOTE_USER" -d "$REMOTE_DB" -c "
    SELECT
      count(*) FILTER (WHERE embedding IS NOT NULL) AS with_embedding,
      count(*) AS total
    FROM muninn.memory_entries;"
```

Cross-check row counts vs. the source. Confirm `vector` extension is
present:

```bash
PGPASSWORD="$PGPASSWORD" psql ... -c \
  "SELECT extversion FROM pg_extension WHERE extname='vector';"
```

### Repoint the binary at the new database

After verification, update `RUNAR_DB_URL` in `~/.runar-forge/.env`:

```bash
# backup
cp ~/.runar-forge/.env ~/.runar-forge/.env.bak

# edit (macOS sed)
sed -i '' \
  's|^RUNAR_DB_URL=.*|RUNAR_DB_URL=postgresql://USER:PASSWORD@HOST:PORT/DB|' \
  ~/.runar-forge/.env

grep RUNAR_DB_URL ~/.runar-forge/.env
```

URL-encode special characters (`@ : / # ? &`) in the password.
Alphanumeric tokens are safe as-is.

Test:

```bash
runar stats
runar search "test query" --limit 1
```

Restart any running MCP server (Claude Code: reconnect via `/mcp` or
restart the session) so the new env loads.

> A future `runar config` + `runar doctor` CLI (Phase 5.5) will
> replace the manual `sed` and verification steps with first-class
> commands. Until then, this manual flow is the supported path.

---

## 12. Hybrid local + remote sync (Phase 5.6)

Run a local-fast backend (sqlite or local PG) alongside a remote PG
"central knowledge hub". Hot path stays local (~6ms hooks); writes
queue to a local outbox; a reconcile process pushes to remote and
pulls deltas back. Conflicts resolve by **LWW + verified tiebreaker**
with audit rows in `sync_conflicts`.

### When to use it

- Network latency to your remote PG is painful (>50ms RTT).
- You want one shared knowledge base across machines or teammates,
  but each developer wants local-fast hooks.
- You want offline writes that durably catch up when reconnected.

If you only use one machine and your PG is on localhost, single-
backend mode (`RUNAR_STORAGE` + `RUNAR_DB_URL`) is simpler. Skip this
section.

### One-time setup

```bash
# 1. Configure both backends — pin BOTH RUNAR_STORAGE and
#    RUNAR_STORAGE_LOCAL until follow-up bug #2 lands (single-backend
#    CLI commands like `runar stats` only read RUNAR_STORAGE today).
runar config set RUNAR_STORAGE_LOCAL sqlite                 # sync layer reads this
runar config set RUNAR_STORAGE sqlite                       # stats/search/save read this
runar config set RUNAR_SQLITE_PATH /Users/me/.runar-forge/memory.db
runar config set RUNAR_STORAGE_REMOTE \
  postgresql://runar:PW@db.example.com:8102/runar_memory

# 2. Remove now-redundant mode-3 leftovers (only if migrating from
#    mode 3). Skip this step on a fresh install.
runar config unset RUNAR_DB_URL                             # only if previously mode 3

# 3. Verify connectivity + schema/dim handshake
runar doctor                # both backends reachable
runar sync init             # records handshake into sync_state
```

> **Why two RUNAR_STORAGE keys?** Phase 5.6 dogfood (2026-04-26)
> revealed that `runar stats` / `search` / `save` / `ask` still
> read the legacy `RUNAR_STORAGE` env var, NOT `RUNAR_STORAGE_LOCAL`.
> Pinning both makes mode 4 work end-to-end. Tracked as bug #2 in
> `phases/PHASE-5.6-RETROSPECTIVE.md` — a one-line fallback at
> `create_librarian()` will retire this requirement.

`runar sync init` refuses if local and remote schema-version differ
(prevents silent data loss). Pass `--force` to override after a
deliberate upgrade window.

### New-team-member bootstrap

Empty local DB, joining an existing team's remote:

```bash
runar sync init
runar sync bootstrap        # paged full pull from remote → local
```

`bootstrap` is paged (default 1000 rows/batch) and idempotent — the
LWW resolver runs per row. On a populated local you must pass
`--yes-i-know` to confirm intent.

### Day-to-day

Two modes — pick one:

**Manual** (default; `RUNAR_SYNC_AUTO=false`):

```bash
runar sync push            # drain local outbox to remote
runar sync pull            # incremental delta from remote
runar sync status          # health summary, conflict count
```

**Auto** (background loop inside `mcp-muninn`):

```bash
runar sync enable          # writes RUNAR_SYNC_AUTO=true
# Restart Claude Code (or any running mcp-muninn) to start the loop.
```

The loop is bound to the `mcp-muninn` lifecycle. Close Claude Code →
loop stops. Idle backoff: 30s → 60s → 2m → 5m cap. Activity resets to
30s. Manual `sync push|pull` always works regardless of auto state.

`runar sync disable` flips it off. Manual mode resumes; outbox keeps
collecting writes until you next `sync push`.

### Outbox retention

`sync_outbox` rows keep their `confirmed_at` for forensics. Run:

```bash
runar sync gc              # delete confirmed rows > 7 days
runar sync gc --dry-run    # report only
```

Auto-triggered on `mcp-muninn` startup if last run > 24h ago. Threshold
configurable via `RUNAR_SYNC_OUTBOX_RETENTION_DAYS` (default 7).
**Pending rows are never deleted** regardless of age — they still need
to push.

### Conflict policy

When local and remote both edit the same row:

| existing | incoming | result |
|---|---|---|
| soft-deleted | not deleted | Skip (resurrect blocked, audit row) |
| not deleted | soft-deleted | Update (delete propagates) |
| verified | unverified | Skip (verified beats unverified) |
| unverified | verified | Update (verified wins) |
| same state, newer incoming | Update (LWW) |
| same state, older incoming | Skip (LWW) |

Skipped writes write a row to `muninn.sync_conflicts` for audit. View
recent conflicts with `runar sync status` or query the table directly.

### Troubleshooting

**Schema-version mismatch on `sync init`** — upgrade the lagging side
(usually the older runar binary). Both ends must apply the same
migration set.

**Embedding-dim mismatch** — set `RUNAR_VECTOR_DIMENSIONS` to the
same value on both ends. Mixing 384-dim local fastembed with
1536-dim remote OpenAI silently breaks semantic search.

**Stale heartbeat reported by `runar doctor`** — auto-sync is
enabled but the loop crashed or `mcp-muninn` exited. Restart Claude
Code.

**Conflicts piling up in `sync_conflicts`** — usually means two
machines touched the same entry quickly. Inspect with:
```sql
SELECT created_at, entry_id, policy, winner_side
FROM muninn.sync_conflicts
ORDER BY created_at DESC LIMIT 20;
```

**`runar stats` / `runar search` errors with `pool error: error
connecting to server`** after switching to mode 4. Fix: pin
`RUNAR_STORAGE=sqlite` (and `RUNAR_SQLITE_PATH=...`) in addition to
the `_LOCAL`/`_REMOTE` pair. Documented in `One-time setup` above.

### Dogfood reference (2026-04-26)

End-to-end mode 3 → mode 4 migration verified on the original
author's install:

| | Mode 3 (remote PG only) | Mode 4 (sqlite + remote-PG sync) |
|---|---|---|
| Avg hook response | 449 ms | **4 ms** (112× faster) |
| Quick benchmark | 13 477 ms | **39 ms** (345× faster) |
| Score (memory quality) | 82.6 / 100 | **82.2 / 100** (within noise) |
| Confidence | 0.87 | **0.89** |
| Bootstrap (9 150 rows) | n/a | clean, 0 conflicts |
| Auto-loop heartbeat | n/a | base 30 s, healthy |

Use this as the expected baseline when validating your own setup.

---

## Environment variables reference

All variables can be set in `~/.runar-forge/.env`. This table is the
canonical list — every env variable the Rust binary reads is listed here.

### Storage

| Variable | Default | Description |
|---|---|---|
| `RUNAR_STORAGE` | `postgresql` | Storage backend: `postgresql` or `sqlite` |
| `RUNAR_DB_URL` | --- | PostgreSQL connection string |
| `RUNAR_DB_CONNECT_TIMEOUT_MS` | `8000` | Timeout for `storage.initialize()` + pool acquires. On timeout the MCP server boots in degraded mode instead of hanging. |
| `RUNAR_SQLITE_PATH` | `~/.runar-forge/memory.db` | SQLite database path |
| `RUNAR_MEMORY_NAMESPACE` | `default` | Default namespace for memory entries |

### Embeddings

| Variable | Default | Description |
|---|---|---|
| `RUNAR_EMBEDDINGS` | auto | `disabled` \| `openai` \| `local`. Unset auto-picks `openai` if `OPENAI_API_KEY` present, else `disabled`. `local` requires `local-embeddings` feature build. |
| `RUNAR_MODELS_DIR` | `~/.runar-forge/models/` | Local ONNX model cache directory |
| `RUNAR_EMBEDDING_MODEL` | `all-MiniLM-L6-v2` | Local model id (also supports `bge-small-en-v1.5`) |
| `OPENAI_API_KEY` | --- | Enables OpenAI embeddings when `RUNAR_EMBEDDINGS` is unset or `openai` |

### Auto-capture (Phase 5.3)

| Variable | Default | Description |
|---|---|---|
| `RUNAR_SAVE_PROMPTS` | `true` | Set `false` to disable UserPromptSubmit prompt persistence |
| `RUNAR_PASSIVE_LEARNING` | `true` | Set `false` to disable PostToolUse rule-based extraction. Default flipped in Phase 5.3 — rule-based capture fires on every Edit/Write/Bash by default. |
| `ANTHROPIC_API_KEY` | --- | When set, the SessionEnd `summarize` hook uses the Claude API (`claude-haiku-4-5`) to synthesize structured summaries. When unset, a deterministic heuristic summarizer runs instead. |

### Memory tiers (Phase 5.4)

All tier thresholds are optional. Defaults are tuned for active single-
developer projects. Increase the day thresholds for archival projects
that should age more slowly.

| Variable | Default | Description |
|---|---|---|
| `RUNAR_TIER_WORKING_DAYS` | `7` | Days since last access before `WORKING` graduates to `EPISODIC` |
| `RUNAR_TIER_EPISODIC_DAYS` | `14` | Days before `EPISODIC` → `SEMANTIC` |
| `RUNAR_TIER_SEMANTIC_DAYS` | `30` | Days before `SEMANTIC` → `ARCHIVAL` |
| `RUNAR_TIER_CITATION_THRESHOLD` | `5` | Access count that triggers Hebbian tier promotion (bump one layer when `access_count >=` this, capped at `SEMANTIC`) |
| `RUNAR_TIER_LOW_CONFIDENCE` | `0.5` | Confidence below this floor marks an entry as low-quality; combined with age > 2× current graduation threshold, fast-tracks to `ARCHIVAL` |
| `RUNAR_TIER_EVICTION_AGE_DAYS` | `90` | `ARCHIVAL` age threshold for `evict_stale` soft-delete. Verified entries are never evicted regardless of age. |
| `RUNAR_TIER_EVICTION_MAX_PER_RUN` | `100` | Maximum rows soft-deleted per `runar gc` / SessionEnd eviction pass |

### Hybrid sync (Phase 5.6)

All optional. Set `_LOCAL` + `_REMOTE` together to enable hybrid mode;
otherwise the binary runs in single-backend mode (the historical
`RUNAR_STORAGE` / `RUNAR_DB_URL` path).

| Variable | Default | Description |
|---|---|---|
| `RUNAR_STORAGE_LOCAL` | unset | `sqlite` or `postgresql`. Hot-path backend for hooks. |
| `RUNAR_STORAGE_REMOTE` | unset | Remote PG `postgresql://...` URL. Central knowledge hub. |
| `RUNAR_SYNC_AUTO` | `false` | `true` enables the background reconcile loop inside `mcp-muninn`. Toggle via `runar sync enable | disable`. |
| `RUNAR_SYNC_INTERVAL_MS` | `30000` | Base interval between auto-sync ticks. |
| `RUNAR_SYNC_MAX_BACKOFF_MS` | `300000` | Idle backoff cap (5 min). |
| `RUNAR_SYNC_OUTBOX_RETENTION_DAYS` | `7` | Days a confirmed outbox row is kept for audit before `runar sync gc` deletes it. Pending rows never deleted. |
| `~/.runar-forge/sync-heartbeat` | absent | Auto-loop liveness file. `runar doctor` flags as stale when older than 2× max-backoff. |
| `~/.runar-forge/sync-gc-last-run` | absent | Marker file. `mcp-muninn` startup auto-triggers GC if older than 24 h. |

### Hook safety (Phase 5.5.4)

| Variable / file | Default | Description |
|---|---|---|
| `RUNAR_DISABLE_HOOKS` | unset | Set to `1` to make every hook subcommand exit 0 immediately. Emergency lever for editor freezes. |
| `~/.runar-forge/.disable-hooks` | absent | `touch` to disable hooks without setting an env var. Persists across shell restarts. |
| `RUNAR_HOOK_BUDGET_MS` | `800` | Per-hook wall-clock cap. SessionEnd summarize gets 4× this. |
| `RUNAR_UPDATE_MANIFEST_URL` | unset | Override the release-manifest endpoint used by `runar update` |
| `~/.runar-forge/hook.log` | rotated 1 MiB | Hook stderr log. Replaces the historical `2>/dev/null`. `runar doctor` tails the last 5 lines. |
| `~/.runar-forge/db-breaker-<pid>.json` | absent | Per-project DB-connectivity breaker state. Trips after 2 consecutive connect failures, stays open 60 s, then retries. Auto-managed. |

### Debug + logging

| Variable | Default | Description |
|---|---|---|
| `RUNAR_DEBUG` | `false` | Enable debug-event logging (`muninn_debug` MCP tool reads this) |
| `RUST_LOG` | `warn` | Log verbosity (`error`, `warn`, `info`, `debug`, `trace`) |

---

## Migrating from the TypeScript CLI

The TypeScript packages were removed in the v0.2 cleanup; the Rust binary
is now the only implementation. If you previously installed the
TypeScript-based `runar` via `pnpm link --global`:

1. Unlink the old CLI:
   ```bash
   pnpm unlink --global @runar-forge/hm 2>/dev/null || true
   npm uninstall -g @runar-forge/hm 2>/dev/null || true
   ```
2. Install the Rust binary (step 1 above).
3. Re-run `runar setup claude-code --project <id>` from each project
   directory. This will:
   - Rewrite `~/.claude.json` with the single unified `muninn` server
     (removing the old separate `huginn` / `curator` entries)
   - Rewrite `.claude/settings.json` hooks with the new binary path

Your existing memory data in PostgreSQL / SQLite is preserved across the
migration — the schema is identical.
