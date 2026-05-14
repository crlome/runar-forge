#!/usr/bin/env node
/**
 * @package @runar-forge/cli
 * @description Shim: execs the platform-native runar binary dropped
 *              into this directory by scripts/install.js.
 */
'use strict'

const { spawnSync } = require('node:child_process')
const path = require('node:path')
const fs = require('node:fs')

const binName = process.platform === 'win32' ? 'runar.exe' : 'runar'
const binPath = path.join(__dirname, binName)

if (!fs.existsSync(binPath)) {
  console.error(
    `[runar] Binary not found at ${binPath}. `
    + `Re-run \`npm install -g @runar-forge/cli\` or check postinstall output.`,
  )
  process.exit(127)
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
