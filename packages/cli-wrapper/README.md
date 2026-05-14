# @runar-forge/cli

Thin npm wrapper that downloads the `runar` Rust binary appropriate
for your platform.

## Install

```bash
npm install -g @runar-forge/cli
```

On install, `scripts/install.js` downloads the matching release asset
from GitHub Releases, extracts it, and places `runar` (or `runar.exe`
on Windows) under `bin/`. The `bin` entry in `package.json` then links
`runar` into your global PATH.

## Supported platforms

- `linux-x64` → `runar-x86_64-unknown-linux-gnu`
- `linux-arm64` → `runar-aarch64-unknown-linux-gnu`
- `darwin-arm64` → `runar-aarch64-apple-darwin`
- `win32-x64` → `runar-x86_64-pc-windows-msvc`

For other platforms, build from source: see the workspace root README.

## Configuration

| Env var | Default | Purpose |
|---|---|---|
| `RUNAR_RELEASE_REPO` | `crlome/runar-forge` | GitHub `owner/repo` to fetch from |
| `RUNAR_RELEASE_TAG` | `v<pkg.version>` | Release tag to fetch |
| `RUNAR_RELEASE_BASE_URL` | computed from repo+tag | Full base URL override (mirrors / forks) |
| `RUNAR_SKIP_DOWNLOAD` | unset | Set to `1` to skip postinstall (CI / sandboxes) |

## Direct binary download

If you prefer not to use npm, grab the binary from the GitHub Releases
page and drop it on your PATH. The wrapper is pure convenience.
