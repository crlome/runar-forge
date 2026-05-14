#!/usr/bin/env node
/**
 * @package @runar-forge/cli
 * @description Postinstall: downloads the platform-appropriate `runar`
 *              Rust binary from GitHub Releases and places it at
 *              bin/runar (or runar.exe).
 */
'use strict'

const fs = require('node:fs')
const path = require('node:path')
const https = require('node:https')
const zlib = require('node:zlib')
const { pipeline } = require('node:stream/promises')
const { spawnSync } = require('node:child_process')

const pkg = require('../package.json')

const DEFAULT_REPO = 'crlome/runar-forge'
const REPO = process.env.RUNAR_RELEASE_REPO || DEFAULT_REPO
const TAG = process.env.RUNAR_RELEASE_TAG || `v${pkg.version}`
const BASE = process.env.RUNAR_RELEASE_BASE_URL
  || `https://github.com/${REPO}/releases/download/${TAG}`

const TARGETS = {
  'linux-x64':    { asset: 'runar-x86_64-unknown-linux-gnu.tar.gz',  archive: 'tar.gz' },
  'linux-arm64':  { asset: 'runar-aarch64-unknown-linux-gnu.tar.gz', archive: 'tar.gz' },
  'darwin-arm64': { asset: 'runar-aarch64-apple-darwin.tar.gz',      archive: 'tar.gz' },
  'win32-x64':    { asset: 'runar-x86_64-pc-windows-msvc.zip',       archive: 'zip'    },
}

function resolveTarget() {
  const key = `${process.platform}-${process.arch}`
  const target = TARGETS[key]
  if (!target) {
    throw new Error(
      `Unsupported platform: ${key}. `
      + `Supported: ${Object.keys(TARGETS).join(', ')}. `
      + `Build from source: https://github.com/${REPO}`,
    )
  }
  return target
}

function download(url, dest, redirects = 5) {
  return new Promise((resolve, reject) => {
    https
      .get(url, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          if (redirects <= 0) return reject(new Error('Too many redirects'))
          res.resume()
          return resolve(download(res.headers.location, dest, redirects - 1))
        }
        if (res.statusCode !== 200) {
          res.resume()
          return reject(new Error(`HTTP ${res.statusCode} fetching ${url}`))
        }
        pipeline(res, fs.createWriteStream(dest)).then(resolve, reject)
      })
      .on('error', reject)
  })
}

function extractTarGz(archivePath, destDir) {
  const r = spawnSync('tar', ['-xzf', archivePath, '-C', destDir], { stdio: 'inherit' })
  if (r.status !== 0) throw new Error(`tar exited with ${r.status}`)
}

function extractZip(archivePath, destDir) {
  if (process.platform === 'win32') {
    const r = spawnSync('powershell', [
      '-NoProfile', '-Command',
      `Expand-Archive -LiteralPath '${archivePath}' -DestinationPath '${destDir}' -Force`,
    ], { stdio: 'inherit' })
    if (r.status !== 0) throw new Error(`Expand-Archive exited with ${r.status}`)
  } else {
    const r = spawnSync('unzip', ['-o', archivePath, '-d', destDir], { stdio: 'inherit' })
    if (r.status !== 0) throw new Error(`unzip exited with ${r.status}`)
  }
}

async function main() {
  if (process.env.RUNAR_SKIP_DOWNLOAD === '1') {
    console.log('[runar] RUNAR_SKIP_DOWNLOAD=1, skipping binary download.')
    return
  }
  const { asset, archive } = resolveTarget()
  const url = `${BASE}/${asset}`
  const binDir = path.join(__dirname, '..', 'bin')
  fs.mkdirSync(binDir, { recursive: true })
  const archivePath = path.join(binDir, asset)

  console.log(`[runar] Downloading ${asset} from ${url}`)
  try {
    await download(url, archivePath)
  } catch (err) {
    console.error(`[runar] Download failed: ${err.message}`)
    console.error(
      '[runar] Override RUNAR_RELEASE_REPO, RUNAR_RELEASE_TAG, or '
      + 'RUNAR_RELEASE_BASE_URL to fetch from a different location.',
    )
    process.exit(1)
  }

  try {
    if (archive === 'tar.gz') extractTarGz(archivePath, binDir)
    else extractZip(archivePath, binDir)
  } catch (err) {
    console.error(`[runar] Extract failed: ${err.message}`)
    process.exit(1)
  } finally {
    try { fs.unlinkSync(archivePath) } catch (_) {}
  }

  const isWin = process.platform === 'win32'
  const finalName = isWin ? 'runar.exe' : 'runar'
  const finalPath = path.join(binDir, finalName)
  if (!fs.existsSync(finalPath)) {
    throw new Error(`Expected binary not found at ${finalPath} after extraction`)
  }
  if (!isWin) fs.chmodSync(finalPath, 0o755)
  console.log(`[runar] Installed ${finalName} at ${finalPath}`)
}

main().catch((err) => {
  console.error(`[runar] ${err.message}`)
  process.exit(1)
})
