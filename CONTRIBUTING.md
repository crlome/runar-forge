# Contributing to RunarForge

Thanks for your interest in RunarForge (Huginn & Muninn). This guide
covers how to set up a dev environment, the change workflow, and the
checks your pull request must pass.

## TL;DR

1. Fork the repo (external contributors) or create a branch (collaborators).
2. Make your change on a topic branch — **never commit to `main` directly**.
3. Run the CI gates locally (`fmt`, `clippy`, `test`) until green.
4. Open a pull request. CI runs on Linux, macOS, and Windows.
5. Once checks pass, the PR can be merged.

## Prerequisites

- **Rust** stable toolchain (edition 2021) with `rustfmt` and `clippy`:
  ```sh
  rustup toolchain install stable
  rustup component add rustfmt clippy
  ```
- A C toolchain (for the bundled SQLite via `rusqlite`). On macOS this
  comes with the Xcode command-line tools; on Linux install `build-essential`.

## Getting started

```sh
git clone https://github.com/<your-fork>/runar-forge.git
cd runar-forge
cargo build
cargo test --workspace
```

## Branching model

`main` is protected: direct pushes are blocked and all CI checks must
pass before a PR can merge. There are two paths in:

- **External contributors** (no write access): fork the repo, push your
  branch to your fork, and open a PR against `crlome/runar-forge:main`.
- **Collaborators** (write access): branch off `main`, push the branch
  to the repo, and open a PR. Same gates apply to everyone — including
  maintainers.

Use short, scoped branch names, e.g. `fix/windows-bin-path`,
`feat/sqlite-fts5`, `docs/contributing`.

## The CI gates

Every push and pull request runs the same matrix CI across
`ubuntu-latest`, `macos-latest`, and `windows-latest`. Reproduce it
locally before opening a PR:

```sh
# 1. Formatting — must produce no diff
cargo fmt --all -- --check

# 2. Lints — warnings are errors
cargo clippy --workspace --all-targets -- -D warnings

# 3. Tests
cargo test --workspace --release
```

A PR cannot merge until `test (ubuntu-latest)`, `test (macos-latest)`,
and `test (windows-latest)` are all green and the branch is up to date
with `main`.

## Testing

Green CI is the floor, not the bar. A change that adds behaviour brings
tests with it, and there is a checklist for what those have to cover:

**[docs/TESTING.md](docs/TESTING.md)** — read it before your first PR.

The short version: tests live inline in the file they cover; anything
touching `RUNAR_HOME` or env vars goes through `test_support`; storage
tests replay the real migrations rather than hand-building tables; and
before you call a test done, break the thing it guards and confirm it
fails. A test that passes with and without your fix is documentation.

## Commit messages

Follow [Conventional Commits](https://www.conventionalcommits.org/),
matching the existing history:

```
<type>(<scope>): <subject>
```

- **types:** `feat`, `fix`, `docs`, `chore`, `refactor`, `test`, `perf`
- **scope** is optional but encouraged, e.g. `feat(npm)`, `fix(windows)`,
  `docs(readme)`
- Keep the subject ≤ ~50 chars, imperative mood, no trailing period.
- Add a body explaining the *why* when it isn't obvious from the diff.

## Pull requests

- Keep PRs focused — one logical change per PR.
- Fill in what changed and why; link any related issue.
- Make sure CI is green before requesting merge.
- Platform-specific code (Windows/macOS/Linux paths, binaries) should
  note which platforms you tested on.

## License

By contributing, you agree that your contributions are licensed under
the [MIT License](LICENSE) that covers this project.
