# Testing guidelines

How this project tests, and what a new feature has to bring with it. These
are not aspirations — every rule below exists because breaking it shipped a
bug.

Run the same three gates CI runs, before you push:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --release
```

---

## The checklist for a new feature

A change is not done until all of these are true.

- [ ] **A happy path and at least one failure path.** A tool that succeeds
      is half a tool; the half that matters is what it does with input it
      cannot honour.
- [ ] **Every new guard has a test that fails when the guard is removed.**
      See [mutation-checking](#mutation-check-your-own-tests) below. A test
      that passes with and without your fix is documentation, not a test.
- [ ] **Assertions are on the shape the consumer parses**, not the shape the
      producer wrote. See [assert through the consumer](#assert-through-the-consumer).
- [ ] **Anything touching `RUNAR_HOME` or env vars uses `test_support`.**
- [ ] **A new migration has a test for the invariant it establishes**, run
      against a replayed schema.
- [ ] **A behaviour implemented in both storage backends states its contract
      in a comment on both sides**, and has a test pinning the shared
      constant or shape.
- [ ] **A new matcher, extractor or retirement pass was run against a copy of
      a real corpus** before you believed it.
- [ ] **A new public surface** — an MCP tool, a CLI subcommand — is declared
      *and* dispatchable, with a test asserting both.

---

## Where tests live

Unit and integration tests are inline `#[cfg(test)] mod tests` blocks at the
bottom of the file they cover. Put the test next to the code; do not create a
parallel test tree.

`crates/muninn/tests/` is reserved for future binary-level smoke tests that
spawn `runar` as a subprocess. It is empty on purpose. If you need one, that
is where it goes — do not invent a third location.

`crates/muninn/examples/` holds runnable diagnostics (`outbox.rs`, `sexp.rs`,
`unresolved.rs`). They are compiled by `--all-targets` so they cannot rot,
but they assert nothing. They are for looking at real data, not for testing.

## Global state: always use `test_support`

Tests run in one process, in parallel, and share the environment. Two rules,
no exceptions:

```rust
// Anything that reads RUNAR_HOME or writes under it:
crate::test_support::with_runar_home(|| {
    // isolated temp dir, restored afterwards, serialised against other tests
});

// Anything that needs one env var, including across .await:
let _guard = crate::test_support::with_env("RUNAR_STORAGE_LOCAL", "1");
```

Never call `std::env::set_var` directly in a test. A test that mutates the
environment without the lock does not fail — it makes an *unrelated* test
fail, intermittently, later, on one platform. `breaker::tests` cost four
full release runs to confirm fixed for exactly this reason.

## Storage tests: real schema, replayed

Open an in-memory adapter and run the real migrations:

```rust
let adapter = SqliteAdapter::in_memory("test").unwrap();
adapter.initialize().await.unwrap();
```

Never hand-build a table in a test. The point is to exercise the schema that
ships, including its indexes, triggers and CHECK constraints. A test against
a hand-built table passes on a schema that does not exist.

Each migration that establishes an invariant gets a test naming it — see
`migration_012_backfills_namespace_from_project_id`. A migration with no test
is a migration nobody can safely change later.

## Cross-backend parity

SQLite and Postgres are two implementations of one trait, and they have
drifted silently more than once. `SqliteAdapter::update` bound `tags` through
`as_str()`, so a JSON array — the shape the Postgres adapter explicitly
accepts — bound the empty string and erased every tag on the row, reporting
success.

So: when a behaviour exists in both adapters,

1. State the contract in a comment **on both sides**. The model is the 0.65
   relevance floor in `semantic_search`, which says in both files that the
   number is shared.
2. Pin the shared constant or the accepted input shape in a test. Where the
   Postgres path cannot run in unit tests, still test the SQLite side against
   the documented contract, and reference it.

"The other backend probably does the same thing" is how the divergence got
in.

### Live-Postgres tests

The Postgres adapter's SQL cannot be reached by an in-memory test, so those
tests are `#[ignore]`d and run against a real server:

```sh
docker compose --profile postgresql up -d postgres
RUNAR_TEST_PG_URL=postgresql://runar:runar_password@localhost:5432/runar_memory \
  cargo test -p runar-muninn --lib -- --ignored
```

CI runs the same thing in the `postgres` job. They share one database, so
each test scopes itself to a unique namespace rather than truncating tables
— they must be safe to run concurrently. They panic rather than skip when
the URL is absent, so a misconfigured job fails loudly instead of passing
with zero tests run.

## Mutation-check your own tests

Before you call a test done, break the thing it guards and confirm it fails:

```sh
# Flip the condition, delete the guard, or invert the comparison, then:
cargo test -p runar-muninn --lib <the_test_name>
# Put it back.
```

For a whole file, `cargo mutants -f <file>` does this systematically. Neither
is a CI gate — mutation runs over 600+ tests are far too slow for a PR, and
would produce more triage than signal. It is a habit, and it is the single
highest-value one in this list.

A mutation that deletes `exact_content: true` from `save_state` once passed
the entire suite, because the test built its own blob and exercised the
mechanism rather than the wiring. Which leads to the next rule.

## Assert through the consumer

Test the output the downstream caller actually reads.

PR #40 passed every enqueue-side test and still failed on the wire, because
`push_one` deserialises the outbox payload into a full `MemoryEntry` and
nothing tested that round trip. The producer wrote something the consumer
could not parse, and both sides' tests were green.

Concretely:

- An MCP tool is tested by parsing its JSON response and asserting on the
  fields a client reads — not by inspecting the store underneath it.
- A hook is tested by asserting on the consumer's state, not on what the
  hook printed. A packet the harness discards is not injected, however
  correct it looks.
- A payload is tested by deserialising it the way the reader does.

## Validate matchers against real data

Unit tests confirm what a matcher was written to do. They cannot tell you
what it does to text nobody imagined. Redaction rules, extraction rules and
retirement passes all shipped bugs that unit tests confirmed and a single
run over the live corpus refuted within minutes.

Before merging one, run it against a **copy** of a real database:

```sh
cp ~/.runar-forge/memory.db /tmp/corpus-copy.db
RUNAR_STORAGE=sqlite RUNAR_SQLITE_PATH=/tmp/corpus-copy.db cargo run --release -- <command>
```

Two shapes to be especially suspicious of, because both look correct in a
unit test: a rule that matches its own output, and an allowlist that appears
stricter and therefore safer.

## Retirement and reconciliation passes

A pass that deletes or retires entries by set difference must be able to
prove its set is complete. `deprecate_orphan_tables` retired every key absent
from "the keys this crawl wrote" — and a crawl writes none for `.sql` files,
so it soft-deleted every per-table entry in the project.

If you write one, the test has to establish the completeness premise
explicitly, not assume it. Where the premise genuinely holds, say so in a
comment naming why — as `PlanStore::create_plan` does, having just written
the plan's entire key set.

## Text handling: chars, not bytes

Two releases were patched for the same defect. `&s[..n]` and
`String::truncate(n)` take **byte** offsets and panic mid-character; a
`// ── Section ──` divider is enough to trigger it once unrelated edits shift
a file's offsets.

Use `text::char_prefix` for a character budget and `text::byte_prefix` for a
byte budget floored to a boundary. Any test for text handling includes a
multibyte fixture — `"café ☕ ".repeat(n)` is enough, and it is what catches
this.

When fixing a defect of this family, grep the **shape**, then grep again for
the shape under every plausible local name. The 0.9.1 miss searched by
variable name; the same six-line helper was copy-pasted under four names.

## Credential fixtures

Assemble fake credentials at runtime from parts. Do not write them as
literals: GitHub push protection rejects well-formed fabricated keys and the
push will fail. Every redaction pattern also needs a paired negative test, so
that prose merely *naming* a token prefix survives unredacted.

## What CI runs

`.github/workflows/ci.yml`, on every PR and every push to `main`:
`fmt --check`, `clippy --all-targets -D warnings`, and
`cargo test --workspace --release` on Ubuntu, macOS and Windows. All three
must be green before merge; `main` is protected.

Clippy runs with `--all-targets`, so test code is linted too — a warning in a
`#[cfg(test)]` block fails the build like any other.

## The test-count badge

The README `tests-NNN passing` badge is a hand-typed string. It rots. It is
re-checked at release time against the preflight run, not per PR. Do not
treat it as a source of truth, and do not spend a PR bumping it.
