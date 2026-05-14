# Installing & Upgrading the `runar` Binary (macOS / Linux)

> Companion to `INSTALLATION-GUIDE.md`. This file documents the
> **install hygiene** that prevents the "frozen terminal / Claude Code /
> VS Code on first run" failure mode tracked as Phase 4.8.17 and
> re-encountered after the Phase 5.6.2 build (2026-04-25).

---

## TL;DR (every rebuild)

```bash
cd /path/to/runar-forge
cargo build --release -p runar-muninn
# Then run the manual reinstall steps below.
```

If you do not have the script yet, run the steps under
**[Manual reinstall](#manual-reinstall)** below. Either way the
five non-negotiable rules are:

1. **`mv` not `cp`** over `~/.runar-forge/bin/runar`. Inode swap is
   what keeps in-flight MCP processes (Claude Code, Cursor) from
   getting `SIGKILL`ed.
2. **Re-codesign adhoc** after install: `codesign --force --sign -`.
   Stale Mach-O signatures make Gatekeeper / `syspolicyd` hang on
   first invocation in each new shell.
3. **Strip `com.apple.quarantine`** defensively
   (`xattr -dr com.apple.quarantine`). Recent macOS adds this on
   downloaded artifacts and on some build outputs.
4. **Keep `.previous`** for rollback. Never overwrite without it.
5. **Recreate the `~/.cargo/bin/runar` symlink** if you nuked it.
   That is the path most users have on `$PATH`.
6. **Restart any long-running `runar mcp-muninn` after swap**
   (quit + relaunch Claude Code / VSCode / Cursor). Unix `mv` keeps
   running processes pinned to the **old inode**, so the in-process
   auto-sync loop and MCP handlers will still execute the **old
   code** until the process exits. Symptom seen on Phase 5.7
   upgrade: new saves got `author` stamped locally but landed on
   remote PG with `author=NULL` because the stale MCP server drained
   the outbox using pre-migration code. Verify cleanup with
   `ps aux | grep mcp-muninn` — kill any process older than your
   binary swap timestamp.

Skip any of these → freeze comes back, or stale-binary writes silently
diverge from the new schema.

---

## Why it freezes

Two distinct failure modes, often confused:

### (a) Hook freeze when DB is unreachable
**Symptom:** Claude Code / Cursor pauses for 8+ seconds on every
PreToolUse / PostToolUse hook fire.
**Root cause:** `create_librarian()` waited the full
`RUNAR_DB_CONNECT_TIMEOUT_MS` per fire because PostgreSQL was down or
the URL pointed at a stale host.
**Fix (already in code):** commit `81caf40` ("fixes freeze") +
Phase 4.8.17 — DB circuit breaker (`breaker::is_db_tripped`,
`db_record_failure`, `db_record_success`), `hook_budget()` timeout
on `build_context`, `mcp::run_degraded_stdio_server` for the MCP
entry-point, `RUNAR_DISABLE_HOOKS` kill switch.
**Recovery:** nothing — already in the binary. If a hook *still*
freezes, check `~/.runar-forge/hook.log` and the breaker state at
`~/.runar-forge/db-breaker-<project>.json`.

### (b) Cold-start freeze after binary upgrade (this doc)
**Symptom:** every fresh shell freezes on the *first* invocation of
`runar` (any subcommand, even `runar --version`). Subsequent runs in
the same shell are fast. Reproduces in Terminal, VS Code, Claude Code.
**Root cause:** macOS `syspolicyd` evaluates the new Mach-O on first
exec per `cs_blob` hash. With:
  - a stale / mismatched ad-hoc signature (binary was overwritten in
    place via `cp`), or
  - a `com.apple.quarantine` xattr (rare for `cargo build` output but
    happens on tarball-based releases), or
  - the binary mid-`cp` while a parent process still holds the old
    inode open,

…the kernel can `SIGKILL` the process or block in
`AMFI`/`syspolicyd` until the policy daemon decides. On a slow
machine this looks identical to a hang.
**Fix:** re-codesign adhoc + atomic `mv` + strip xattrs (this doc).

---

## Manual reinstall

Run these from anywhere; absolute paths throughout. Adjust
`SRC` if your build tree lives elsewhere.

```bash
SRC=/path/to/runar-forge/target/release/runar
DST=~/.runar-forge/bin/runar
SYMLINK=~/.cargo/bin/runar

# 0. Build
( cd /path/to/runar-forge && cargo build --release -p runar-muninn )

# 1. Drop the cargo-bin symlink so nothing tries to exec the old inode
rm -f "$SYMLINK"

# 2. Keep the previous binary for rollback
[ -f "$DST" ] && mv -f "$DST" "$DST.previous"

# 3. Stage the new binary next to the destination (same filesystem
#    so the final `mv` is atomic)
STAGE="$DST.new"
cp "$SRC" "$STAGE"
chmod +x "$STAGE"

# 4. Re-codesign adhoc — non-negotiable on macOS
codesign --force --sign - "$STAGE"
codesign -dv "$STAGE" 2>&1 | grep -E "Signature|Format"

# 5. Strip quarantine xattr defensively
xattr -dr com.apple.quarantine "$STAGE" 2>/dev/null || true

# 6. Atomic install
mv -f "$STAGE" "$DST"

# 7. Recreate the cargo-bin symlink
ln -s "$DST" "$SYMLINK"

# 8. Smoke test in a fresh shell (the *current* shell may still
#    have the old inode cached if any process was holding it open)
exec zsh -c 'runar --version'
```

If step 8 freezes on a brand-new shell, see
[Still frozen?](#still-frozen).

---

## Still frozen?

Run these in order. Stop as soon as one fixes it.

1. **Confirm signature is fresh.**
   ```bash
   codesign -dv ~/.runar-forge/bin/runar 2>&1 | grep -E "Signature|Format|TeamIdentifier"
   ```
   Expected: `Format=Mach-O thin (arm64)`, `Signature=adhoc`.
   If `Signature=not signed` or `Signature=invalid` → re-run step 4
   above.

2. **Check Gatekeeper assessment.**
   ```bash
   spctl --assess --verbose=4 ~/.runar-forge/bin/runar
   ```
   On adhoc binaries this often returns "rejected (the code is valid
   but does not seem to be an app)". Harmless for CLI tools; macOS
   still lets you run it. If it returns "the code signature is not
   valid", redo step 4.

3. **Allow the binary explicitly** (rarely needed):
   ```bash
   spctl --add ~/.runar-forge/bin/runar
   ```

4. **Look for held inodes.** A process that opened the old binary
   (long-running `claude code` session, `mcp-muninn`, `runar
   mcp-muninn`) keeps the deleted inode alive on disk and macOS
   may still route requests to it.
   ```bash
   lsof -p $(pgrep -f 'runar|mcp-muninn') 2>/dev/null | grep runar
   ```
   If the old inode shows up: stop the offending process, then
   re-run step 8 of the reinstall.

5. **Check the hook log.** If the freeze is hook-side (case (a)
   above), this surfaces it:
   ```bash
   tail -50 ~/.runar-forge/hook.log
   ```
   Look for `create_librarian`, `budget exceeded`, or
   `db breaker tripped`. Fix DB connectivity (`runar doctor`).

6. **Last resort — rebuild from clean.** Rare but happens after
   toolchain bumps:
   ```bash
   cd /path/to/runar-forge
   cargo clean
   cargo build --release -p runar-muninn
   ```
   Then redo the reinstall.

---

## Rollback

```bash
mv -f ~/.runar-forge/bin/runar          ~/.runar-forge/bin/runar.broken
mv -f ~/.runar-forge/bin/runar.previous ~/.runar-forge/bin/runar
codesign --force --sign - ~/.runar-forge/bin/runar
```

The cargo-bin symlink does not need to change — it points at
`~/.runar-forge/bin/runar`, which now resolves to the previous
build.

---

## Migrations on upgrade

The first run after an upgrade auto-applies any new SQL migrations
inside `storage.initialize()`. Migrations are numbered, additive,
and idempotent — re-runs are no-ops. No manual step required.

Recent additions:

| Phase | Migration | Effect |
|---|---|---|
| 5.6.1 | `010_add_sync_outbox` | `sync_outbox` + `sync_state` + `sync_conflicts` tables for hybrid sync. |
| 5.7 | `011_add_author` | Adds nullable `author` + `verified_by` to `memory_entries`. Existing rows stay valid (NULL = pre-attribution). No backfill needed. Set `git config --global user.name "Your Name"` so future saves get stamped. |

If `runar doctor` reports a migration check failure after upgrade,
rollback via the steps below and open an issue with the doctor JSON.

---

## When to use `runar update` instead

`runar update` (from `update.rs`) is the *intended* upgrade path
once the release manifest endpoint is live. It already does the
atomic-rename + `runar.previous` dance internally. Until the
release manifest URL is wired up, prefer the manual flow above —
`runar update --check` will just bail on the empty default.

Re-codesign is **not** built into `runar update` yet (TODO). If/when
the release pipeline ships, the binary it downloads will already be
adhoc-signed by CI; the local re-sign step becomes optional.

---

## Quick reference

| Path                                  | Purpose                            |
|---------------------------------------|------------------------------------|
| `~/.runar-forge/bin/runar`            | Canonical install location         |
| `~/.runar-forge/bin/runar.previous`   | Rollback target                    |
| `~/.cargo/bin/runar`                  | Symlink most users have on `$PATH` |
| `~/.runar-forge/.env`                 | Config (read at every start)       |
| `~/.runar-forge/hook.log`             | Hook freeze diagnostics            |
| `~/.runar-forge/db-breaker-*.json`    | Per-project DB breaker state       |

| Env var                               | Effect                             |
|---------------------------------------|------------------------------------|
| `RUNAR_DB_CONNECT_TIMEOUT_MS`         | DB connect budget (default 8000)   |
| `RUNAR_DISABLE_HOOKS=1`               | Bypass all CC hooks                |
| `RUNAR_LOG=trace`                     | Verbose tracing on stderr          |

---

## See also

- [INSTALLATION-GUIDE.md](./INSTALLATION-GUIDE.md) — first-time install walkthrough.
- `crates/muninn/src/update.rs` — in-binary self-update implementation.
- `crates/muninn/src/breaker.rs` — DB + summarizer circuit breakers.
