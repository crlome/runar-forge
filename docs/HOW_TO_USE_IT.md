# Huginn & Muninn — User Guide

Huginn & Muninn gives AI coding tools (Claude Code, Cursor, Windsurf, and
others) persistent memory that survives session closes, travels across AI
tools via MCP, and understands your codebase structure. Huginn (the Scout)
crawls your code; Muninn (the Memory) stores and retrieves what it learns.

> Ships as a single Rust binary (`runar`). No Node.js, no build step, no
> extra services besides PostgreSQL if you want the shared-team backend.
> For installation and first-time setup, see
> [INSTALLATION-GUIDE.md](./INSTALLATION-GUIDE.md).

---

## Quick Start

```bash
# 1. Initialize (writes ~/.runar-forge/.env)
runar init --storage postgresql

# 2. Start PostgreSQL (from runar-forge repo root)
cd /path/to/runar-forge
docker compose --profile postgresql up -d

# 3. Configure Claude Code (from your project directory)
cd /path/to/your-project
runar setup claude-code --project my-project

# 4. Crawl the codebase
runar crawl . --project my-project

# 5. Restart Claude Code — done!
```

---

## Storage modes

`runar` runs in one of 5 modes. The Quick Start above covers mode 2.
Full details + env config in
[INSTALLATION-GUIDE.md → Storage modes](./INSTALLATION-GUIDE.md#storage-modes--pick-one).

| # | Mode | When |
|---|---|---|
| 1 | sqlite standalone | Solo dev, zero infra |
| 2 | local PG standalone | Solo dev with pgvector |
| 3 | remote PG standalone | ⚠ slow — hooks cross internet |
| 4 | sqlite local + remote PG sync ⭐ | Laptops on a team (Phase 5.6) |
| 5 | local PG + remote PG sync | Power users with pgvector + sync |

Switch modes via `runar config wizard` (Phase 5.5) or `runar config set`.
Hybrid modes (4/5) require `runar sync init` + `runar sync bootstrap`
on first use — see [§12](./INSTALLATION-GUIDE.md#12-hybrid-local--remote-sync-phase-56).

> **Mode-4 / mode-5 gotcha (2026-04-26):** pin BOTH `RUNAR_STORAGE`
> AND `RUNAR_STORAGE_LOCAL` to the same backend (`sqlite` for mode 4,
> `postgresql` for mode 5). `runar stats` / `search` / `save` / `ask`
> still read the legacy `RUNAR_STORAGE`. Tracked as bug #2 in
> `phases/PHASE-5.6-RETROSPECTIVE.md`.

---

## How it works

### What happens automatically

Once set up, **five hooks** fire inside Claude Code without any action
from you. Turning on `--with-auto-capture` during `runar setup`
installs **two more** (the Phase 5.3 queue + SessionEnd summarizer).

| Hook | When | What it does |
|---|---|---|
| **PreToolUse** | Before every tool call | Injects the **Memory Protocol** (save instructions) + the memory packet (recent sessions, top entries, stats). Also auto-rotates idle sessions (> 30 min) with a `session-end` summary entry. Survives context compaction. |
| **PostToolUse** | After Write / Edit / Create / MultiEdit | `git diff --name-only HEAD` → merges with session file set → persists to the ping file |
| **PostToolUse** | After every `muninn_save` call | Updates `lastSave` in the ping file (resets the nudge timer) |
| **PostToolUse** | After Write / Edit / Create / MultiEdit / Bash | **Rule-based extract** — auto-saves insights matching null-check, error-handling, config-change, migration, CI-config, bash-failure, and env-var patterns. Capped at 20 saves / session. On by default since Phase 5.3 — disable with `RUNAR_PASSIVE_LEARNING=false`. |
| **UserPromptSubmit** | Before each user prompt | Persists non-trivial prompts (≥ 20 chars) as `user-prompt` memory entries; nudges Claude to save if > 15 min since last `muninn_save` |
| **PostToolUse** *(opt-in)* | After Write / Edit / Create / MultiEdit / Bash | **Auto-capture queue** — enqueues every tool call (minus `mcp__muninn__` calls) onto the `pending_observations` table with a 30-sec SHA256 dedup window. Installed only when `runar setup claude-code --with-auto-capture` is used. |
| **SessionEnd** *(opt-in)* | When a session terminates | **Summarize + drain queue** — claims pending observations, synthesizes a structured summary + 0-5 memory entries (Claude-Haiku if `ANTHROPIC_API_KEY` is set, deterministic heuristic otherwise), confirms the queue, and runs the Phase 5.4 tier GC + eviction pass. |

### Memory Protocol (injected every tool call)

Claude sees this before every action:
- **PROACTIVE SAVE** — don't wait for the user to ask
- Self-check: *"Did I fix a bug? Make a decision? Discover a pattern?"* →
  if yes, `muninn_save` now
- Save format guidance (title, content, `topicKey`, `projectId`, tags)
- The list of files modified so far in this session

Re-injected on every call → survives compaction without manual CLAUDE.md
management.

### Auto-session management

Sessions are created and expired by `runar context`:
- **Auto-create** — first `runar context` call creates an active session
- **Auto-expire** — no activity for 30 min → session ends with a summary
  entry, a new one starts
- **File tracking** — every file write is persisted cumulatively in the
  per-project ping file in the system temp dir (`$TMPDIR/runar-ping-<project>`
  on Unix; `%TEMP%\runar-ping-<project>` on Windows)

### What you do manually

| Action | How | When |
|---|---|---|
| Re-crawl | `runar crawl . -p my-project` | After significant code changes |
| Ask a question | `runar ask "<question>" -p my-project` or `curator_ask` MCP tool | Anytime you need codebase knowledge |
| Search | `runar search <query>` or `muninn_search` | Anytime |
| Run benchmark | `runar benchmark -p my-project` | Periodically, to check memory quality |
| Onboarding report | `runar onboard -p my-project` or `curator_onboard` MCP tool | First day on a new codebase |
| Tier GC (Phase 5.4) | `runar gc -p my-project [--dry-run]` | Ad hoc — SessionEnd already runs it when auto-capture is on |
| Export memory | `runar export -p my-project -o dump.jsonl` | Backup, share, or move between machines |
| Import memory | `runar import dump.jsonl` | Restore or seed a fresh DB |
| Verify an entry | `muninn_verify` MCP tool with the entry id | Endorse agent-generated entries that are especially valuable — grants a rank bonus + never-evicted status |
| Filter by author | `muninn_search` with `author: "alice"` (case-insensitive substring) | Multi-dev teams — "show me what Bob saved" |

You no longer need to manually tell Claude to save — the Memory Protocol
handles that. But you can still explicitly ask Claude to save something
when you want.

**Redact sensitive content:** wrap secrets in `<private>…</private>`
tags when pasting into chat; they are stripped to `[redacted]` before
any memory entry is saved, and the entry is tagged `redacted` for
audit. Best-effort hygiene, not a security boundary.

**Per-dev attribution (Phase 5.7):** every save stamps `author` from
`git config user.name` (name only — never email; email is PII and
shared remote PG would surface it). `mark_verified` records
`verified_by` independently. Both nullable; pre-existing rows stay
valid. To enable: `git config --global user.name "Your Name"`.
`runar doctor` reports identity status (check #11).

---

## Crawl modes

| Mode | Flag | Use when |
|---|---|---|
| **auto** | default | Normal. Picks `full` on first crawl, `incremental` after that. |
| **incremental** | `--mode incremental` | After minor code changes. Uses `git diff` against last crawl's commit, falls back to content hashes. Fast. |
| **full** | `--mode full` | Major refactors, dependency upgrades, or to re-analyze everything. |

```bash
runar crawl . --project my-project                       # auto
runar crawl . --project my-project --mode incremental    # after edits
runar crawl . --project my-project --mode full           # re-analyze all
runar crawl . --project my-project --focus src/api/      # subdirectory only
```

Re-crawling is safe — existing entries are updated in place, not
duplicated. The crawl orchestrator persists a state entry with the last
commit hash + per-file content hashes.

---

## CLI reference

### `--project` auto-detection

When a command accepts `--project` (or `-p`) and you omit it, the CLI
resolves it in this order:

1. Explicit flag value
2. `git remote get-url origin` (last path segment, `.git` stripped)
3. Current directory name
4. Literal `"default"` (only if git and cwd both fail — rare)

This applies to the optional-pid commands: `context`, `nudge`,
`save-ack`, `session ping`, `extract`, `search`, `stats`,
`session list`. Commands that *require* a pid (`crawl`, `benchmark`,
`onboard`, `ask`) still error out if you omit the flag — the explicit
value prevents a crawl from writing to the wrong project.

Hooks installed by `runar setup claude-code` bake the detected pid
into every hook command, so they always pass an explicit value.
Auto-detect only fires on manual CLI calls.

> **Typo recovery:** a wrong `--project foo` writes to a new
> `project_id='foo'`. Use `muninn_merge_projects(from, to)` to move
> entries back. No entries are lost.

### System

```bash
runar --version
runar stats                                   # entry counts + namespaces
runar init [--storage sqlite|postgresql]      # write ~/.runar-forge/.env
runar doctor                                  # validate config + storage
```

### Setup

```bash
runar setup claude-code [-p <project>] [--configure]  # MCP + hooks + CLAUDE.md
runar setup vscode                            # write .vscode/mcp.json
runar setup opencode                          # write opencode.json
runar setup codex                             # write ~/.codex/config.toml
runar setup cursor                            # print MCP config
runar setup windsurf                          # print MCP config
```

`--configure` (Phase 5.5) runs the storage wizard before writing the
Claude Code integration files — useful on first install or when
repointing at a new database.

### Config (Phase 5.5)

```bash
runar config path                             # print resolved .env path
runar config show [--unmask]                  # print all keys, mask secrets
runar config get <KEY> [--unmask]             # single value
runar config set <KEY> <VALUE>                # atomic upsert
runar config unset <KEY>                      # remove key
runar config wizard                           # interactive backend setup
```

Atomic writes preserve comments and ordering. Secrets (`*PASSWORD*`,
`*TOKEN*`, `*SECRET*`, `*API_KEY*`) and URL passwords are masked
unless `--unmask`. `runar config show` flags any process-env override
that diverges from the file value.

Single source of truth: `~/.runar-forge/.env`. Project-local `.env`
files are **not** read by `runar`; doctor flags one if found.

### Doctor (Phase 5.5)

```bash
runar doctor                                  # all checks, human output
runar doctor --db                             # skip env file + breaker
runar doctor --json                           # machine-readable, jq-able
runar doctor --quiet                          # exit code only
runar doctor --timeout-ms 1000                # fast-fail for CI
```

Read-only validation — never runs migrations. Exit `0` if no failures
(pass + skip OK), `1` if any check fails.

Checks (per backend):

| Check                | PostgreSQL          | SQLite                   |
|----------------------|---------------------|--------------------------|
| env file             | applies             | applies                  |
| storage              | applies             | applies                  |
| db reachable         | TCP + auth          | file stat + open RO      |
| auth                 | `SELECT 1`          | `SELECT 1`               |
| pgvector             | applies             | skip (no vector ext)     |
| schema muninn        | `muninn.*` tables   | flat tables              |
| migrations           | `muninn.schema_migrations` | `schema_migrations` |
| row counts           | applies             | applies                  |
| breaker state        | applies             | applies                  |
| project-local .env   | applies             | applies                  |
| kill-switch          | applies             | applies                  |
| hook log             | applies (tail 5)    | applies (tail 5)         |

Use it after `runar config set RUNAR_DB_URL <new>`, after migrating
PostgreSQL between hosts (see INSTALLATION-GUIDE.md §11), or in CI
gates after deploy.

### Sync (Phase 5.6)

Hybrid local + remote PG. Hot-path stays local-fast; outbox queues
writes; reconcile pushes/pulls in the background or on demand.

```bash
runar sync init                       # validate local+remote pair
runar sync bootstrap [--project P]    # first-time full pull from remote
runar sync push [--limit N] [--dry-run]
runar sync pull [--limit N] [--since ISO]
runar sync status [--json]            # depth, conflicts, loop liveness
runar sync enable | disable           # toggle auto-loop in mcp-muninn
runar sync gc [--dry-run]             # outbox retention sweep (7 days default)
```

Conflict resolver: LWW + verified tiebreaker + soft-delete propagation.
Audit rows in `muninn.sync_conflicts`. See INSTALLATION-GUIDE.md §12
for the full setup, conflict matrix, and troubleshooting.

`claude-code` is the only setup that auto-configures hooks — VSCode,
OpenCode, Codex, Cursor, and Windsurf get the MCP tools only (no
per-project hooks). On Windows, Claude Code hooks are written in exec
form (`command` + `args`, no shell) so they run under Git Bash or the
PowerShell fallback; Unix uses shell form.

### Memory (Muninn)

```bash
runar search <query> [-l <limit>]
  # Keyword + semantic search. --limit default 10.

runar context [-p <project>] [--silent]
  # Full PreToolUse payload: Memory Protocol + memory packet.
  # --silent emits {"additionalContext": "..."} JSON (what the hook produces).
```

### Scout (Huginn)

```bash
runar crawl <path> -p <project> [-m full|incremental|auto] [-f <dir>]
runar benchmark -p <project> [-m quick|full]
  # quick = 9 questions, full = 30. Deterministic grading (no LLM call).
```

### Hook commands

```bash
runar nudge    [-p <project>] [--silent]     # UserPromptSubmit
runar save-ack [-p <project>] [--silent]     # PostToolUse on muninn_save
runar session ping [-p <project>] [--silent] # PostToolUse on file writes
```

All accept `--silent` for hook consumption (JSON output or exit 0 on error,
never noisy).

### MCP server

```bash
runar mcp-muninn
  # One unified stdio server. Exposes all 23 tools:
  # 11 muninn + 5 curator + 7 huginn.
  # Called by ~/.claude.json, not manually.
```

### Additional CLI commands

```bash
runar save <title> <content> [-t <type>] [-p <project>] [--tags a,b] [--topic-key scope:key]
runar architecture -p <project>
runar techdebt    -p <project> [--type todo|fixme|hack|xxx|all]
runar ask <question> [-p <project>]
runar onboard [-p <project>] [--json]
runar session list [-p <project>] [-l <limit>]
runar export [-p <project>] [-t <type>] [-o <file>] [--limit 100000]
runar import <file>
runar gc [-p <project>] [--dry-run]
```

`muninn_verify` + `muninn_capture_passive` + `muninn_session_end` (with
structured fields) are MCP-only — call them from inside Claude Code.

---

## MCP tools reference

Called by your AI tool automatically via the `muninn` MCP server.

### Muninn (Memory) — 13 tools

| Tool | What it does |
|---|---|
| `muninn_save` | Save an entry. Supports `topicKey` for upserts. `<private>…</private>` content is redacted automatically. Stamps `author` from `git config user.name` (Phase 5.7) — pass `author` arg to override. |
| `muninn_capture_passive` | Parse markdown `## Key Learnings:` / `## Takeaways` sections and fan out each bullet as its own entry. One call → many entries. Stable dedup key so re-saves upsert. (Phase 5.3 A2) |
| `muninn_verify` | Owner-endorse an entry — sets `verified=true` + grants a 1.25× rank bonus and makes the row never-evicted. Phase 5.7 also records `verified_by` from `git config user.name`. |
| `muninn_search` | Fused retrieval (semantic + keyword + decay + recency + verified + tier). Every result carries `tokensEstimate` (snippet cost) + `fullTokensEstimate` (cost if fetched via `muninn_get`), plus `author` + `verifiedBy`. Optional `author` arg = case-insensitive substring filter. Compact by default. |
| `muninn_context` | Memory packet for the current project |
| `muninn_get` | Retrieve entry by ID. Response includes `tokensEstimate`, `author`, `verifiedBy`. |
| `muninn_timeline` | Recent activity |
| `muninn_session_start` / `_end` | Explicit session lifecycle. `_end` now accepts structured fields (`goal`, `instructions`, `accomplished`, `discoveries`, `filesModified`). (Phase 5.3 A7) |
| `muninn_stats` | Entry counts, DB size |
| `muninn_related` | Linked entries via edges |
| `muninn_merge_projects` | Merge entries from one project id into another (Phase 5.2) |
| `muninn_debug` | Query the debug log (requires `RUNAR_DEBUG=true`) |

### Huginn (Scout) — 7 tools

| Tool | What it does |
|---|---|
| `huginn_crawl` | Trigger a codebase crawl |
| `huginn_status` | Crawl status + project coverage |
| `huginn_architecture` | Architecture summary |
| `huginn_techdebt` | Tech-debt inventory |
| `huginn_recrawl_file` | Re-analyze a single file |
| `huginn_benchmark` | Run memory-quality benchmark |
| `huginn_skills` | List Huginn capabilities |

### Curator (Knowledge) — 5 tools

| Tool | What it does |
|---|---|
| `curator_ask` | Natural-language Q&A with citations |
| `curator_topics` | Topics / areas covered in memory |
| `curator_decisions` | Architectural decisions recorded |
| `curator_techdebt` | Curated tech-debt analysis |
| `curator_onboard` | First-time onboarding report (architecture, decisions, patterns, rules, tech debt) |

---

## Growing memory over time

Memory grows through three channels:

### 1. Automatic (via Memory Protocol)

The protocol injected on every tool call instructs Claude to save
proactively. Claude self-checks: *"Did I fix a bug? Make a decision?
Discover a pattern?"* and calls `muninn_save` without being asked. If it
hasn't saved in 15+ minutes, the UserPromptSubmit nudge reminds it.

### 2. Crawling

Re-crawl after code changes. The scout updates per-file entries and
re-detects cross-file patterns.

### 3. `topicKey` upserts

For evolving decisions, use `topicKey` in `muninn_save`. Same key + same
project updates the existing entry instead of creating a new one:

```
muninn_save(
  topicKey: "auth:approach",
  title:    "Auth decision: JWT with refresh tokens",
  content:  "Decided to use JWT because...",
  type:     "decision",
  projectId: "my-project"
)
```

First call creates. Subsequent calls with the same `topicKey` update in
place — decisions evolve without history duplication.

---

## Updating RunarForge

Preferred path — atomic self-update via the release manifest:

```bash
runar update --check                     # what's available?
touch ~/.runar-forge/.disable-hooks      # neuter hooks during the swap
runar update                              # download → SHA-256 verify → atomic rename
rm ~/.runar-forge/.disable-hooks
runar doctor                              # confirm everything still green
```

`runar update` writes the new binary to `~/.runar-forge/bin/runar`
through a tempfile + rename, preserving the previous build at
`runar.previous`. Roll back instantly:

```bash
runar update --rollback                   # swap runar.previous back
```

Reads `RUNAR_UPDATE_MANIFEST_URL` for the manifest endpoint. Until the
release pipeline publishes one, set this env var to your own manifest
or build from source as a fallback.

### Source / manual fallback

```bash
# 1. Rebuild
cd /path/to/runar-forge
cargo build --release -p runar-muninn

# 2. Run setup — `runar setup claude-code` mirrors the new binary into
#    ~/.runar-forge/bin/runar atomically + rewrites hook commands to
#    point at the stable path
cd /path/to/your-project
/path/to/runar-forge/target/release/runar setup claude-code -p <id>

# 3. Restart Claude Code
```

The stable bin path means `cargo install` overwriting `~/.cargo/bin/runar`
mid-session no longer affects running CC hooks. Add the stable dir to
PATH for the cleanest setup:

```bash
echo 'export PATH="$HOME/.runar-forge/bin:$PATH"' >> ~/.zshrc
```

---

## Troubleshooting

### Claude Code / VSCode freezes after install

Kill-switch is the first lever — every hook subcommand returns
immediately while it's active:

```bash
touch ~/.runar-forge/.disable-hooks      # editor responsive again
runar doctor                             # diagnose; reads hook.log tail
tail -50 ~/.runar-forge/hook.log         # any timeouts / breaker trips?
```

Each hook is wall-clock-capped via `RUNAR_HOOK_BUDGET_MS` (default
**800 ms**). The DB connectivity breaker trips after 2 consecutive
failures and stays open for 60 s, so a slow / unreachable PG produces
one timeout per minute, not one per hook fire. `runar doctor` reports
both the kill-switch state and the last 5 hook-log entries.

When done diagnosing, `rm ~/.runar-forge/.disable-hooks` to re-enable.

### `runar: command not found`

Make sure the binary is on `PATH`:

```bash
which runar
echo $PATH
```

The legacy TypeScript CLI was removed in the v0.2 cleanup. If a stale
`@runar-forge/hm` symlink lingers in your global pnpm/npm bin, drop it:

```bash
pnpm unlink --global @runar-forge/hm 2>/dev/null || true
npm uninstall -g @runar-forge/hm 2>/dev/null || true
```

### Hooks not firing

```bash
cat .claude/settings.json
```

Should show **five** hook entries (base setup):

- 1 `PreToolUse` with matcher `.*` → `runar context`
- 3 `PostToolUse`:
  - `Write|Edit|Create|MultiEdit` → `runar session ping`
  - `mcp__muninn__muninn_save` → `runar save-ack`
  - `Write|Edit|Create|MultiEdit|Bash` → `runar extract` (Phase 5.3 rule-based)
- 1 `UserPromptSubmit` with matcher `.*` → `runar nudge`

With `--with-auto-capture`, **two more** land:

- 1 additional `PostToolUse` (`Write|Edit|Create|MultiEdit|Bash` → `runar enqueue`)
- 1 `SessionEnd` with matcher `.*` → `runar summarize`

Missing? Run `runar setup claude-code -p <id>` (add `--with-auto-capture`
if you want the queue + SessionEnd summarizer) from the project directory.

### Memory count not growing

1. Verify the Memory Protocol injects: `runar context --project <id>`
   should start with `## Muninn Memory Protocol`. If the output is
   empty, the binary is older than the dotenvy fix — reinstall.
   If `runar context --silent -p <id>` writes `muninn: storage
   unreachable — protocol only:` to stderr, PG is down; the protocol
   still reaches Claude but the memory packet does not. Bring PG back
   with `docker compose ps` / `docker compose --profile postgresql
   up -d`.
2. Force a nudge to test:
   ```bash
   python3 -c "
   import json, time, os
   f = os.path.join(os.environ.get('TMPDIR','/tmp'), 'runar-ping-<id>')
   d = json.load(open(f))
   d['lastSave'] = int(time.time()*1000) - 20*60*1000
   d['filesModified'] = d.get('filesModified') or ['test.rs']
   open(f,'w').write(json.dumps(d))
   "
   runar nudge --silent --project <id>
   # → MEMORY REMINDER JSON
   ```
3. Restart Claude Code after updating hooks

### PostgreSQL connection refused

```bash
docker compose --profile postgresql up -d
docker compose ps    # "running (healthy)"
```

Verify the port in `~/.runar-forge/.env` matches compose (default `5433`).

### Embeddings disabled

By default, if `OPENAI_API_KEY` is not set and `RUNAR_EMBEDDINGS` is unset,
RunarForge uses keyword / FTS search only — no vector search. Search quality
is still good for most queries. Two ways to enable semantic search:

- **OpenAI:** set `OPENAI_API_KEY` in `~/.runar-forge/.env` (auto-picked,
  or force with `RUNAR_EMBEDDINGS=openai`).
- **Local ONNX (offline):** `RUNAR_EMBEDDINGS=local`. Requires a binary
  built with the `local-embeddings` Cargo feature. First run downloads a
  384-dim model (`all-MiniLM-L6-v2`) to `~/.runar-forge/models/`.

> More troubleshooting: [INSTALLATION-GUIDE.md § 10](./INSTALLATION-GUIDE.md#10-troubleshooting).
