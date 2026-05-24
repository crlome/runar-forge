# @runar-forge/cli

Cross-platform launcher for the `runar` Rust binary.

## Install

```bash
npm install -g @runar-forge/cli
```

## No install scripts

This package has **no `postinstall` script** and performs **no network access
at install time**. The native binary ships as ordinary, npm-integrity-checked
package content via per-platform optional dependencies, so it installs cleanly
even with `ignore-scripts=true`:

```bash
npm install -g @runar-forge/cli --ignore-scripts   # still works
```

npm installs only the optional dependency matching your `os` + `cpu`; the rest
are skipped by npm's platform check. The `runar` launcher (plain JS, run only
when you invoke it — never at install) resolves the binary from whichever
platform package was installed and execs it.

## Supported platforms

| Optional dependency           | Host           |
|-------------------------------|----------------|
| `@runar-forge/cli-linux-x64`  | linux / x64    |
| `@runar-forge/cli-linux-arm64`| linux / arm64  |
| `@runar-forge/cli-darwin-x64` | macOS / Intel  |
| `@runar-forge/cli-darwin-arm64`| macOS / Apple Silicon |
| `@runar-forge/cli-win32-x64`  | Windows / x64  |

For other platforms, build from source — see the workspace root README.

## Direct binary download

If you prefer not to use npm, grab the binary from the
[GitHub Releases](https://github.com/crlome/runar-forge/releases) page and drop
it on your PATH, or use `runar update` once installed. The wrapper is pure
convenience.
