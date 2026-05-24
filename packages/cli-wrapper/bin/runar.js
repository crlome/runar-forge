#!/usr/bin/env node
/**
 * @package @runar-forge/cli
 * @description Launcher: execs the runar binary shipped in the matching
 *              per-platform optional dependency (@runar-forge/cli-<os>-<cpu>).
 *              No install scripts, no install-time network — the binary is
 *              ordinary, npm-integrity-checked package content, so this works
 *              with `ignore-scripts=true`.
 */
'use strict'

const path = require('node:path')
const { spawnSync } = require('node:child_process')

const PLATFORM_PACKAGES = {
  'linux-x64': '@runar-forge/cli-linux-x64',
  'linux-arm64': '@runar-forge/cli-linux-arm64',
  'darwin-x64': '@runar-forge/cli-darwin-x64',
  'darwin-arm64': '@runar-forge/cli-darwin-arm64',
  'win32-x64': '@runar-forge/cli-win32-x64',
}

const key = `${process.platform}-${process.arch}`
const pkg = PLATFORM_PACKAGES[key]
if (!pkg) {
  console.error(
    `[runar] Unsupported platform: ${key}. `
    + 'Build from source: https://github.com/crlome/runar-forge',
  )
  process.exit(1)
}

const binName = process.platform === 'win32' ? 'runar.exe' : 'runar'

let binPath
try {
  // Resolve the platform package via its package.json (robust against any
  // `exports` map), then join the binary. Pure resolution — no network.
  binPath = path.join(path.dirname(require.resolve(`${pkg}/package.json`)), binName)
} catch {
  console.error(
    `[runar] Missing optional dependency ${pkg}. `
    + `Reinstall without \`--omit=optional\` (and ensure ${key} is supported).`,
  )
  process.exit(1)
}

const result = spawnSync(binPath, process.argv.slice(2), {
  stdio: 'inherit',
  windowsHide: true,
})
if (result.error) {
  console.error(`[runar] ${result.error.message}`)
  process.exit(1)
}
process.exit(result.status ?? 0)
