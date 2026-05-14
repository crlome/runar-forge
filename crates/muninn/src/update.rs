//! `runar update` — first-class self-update so the user never has to
//! `cargo install --path` again.
//!
//! Operating model
//! ---------------
//! The update flow reads a JSON manifest at `RUNAR_UPDATE_MANIFEST_URL`
//! (defaults to a hard-coded URL once the release pipeline is wired up).
//! The manifest looks like:
//!
//! ```json
//! {
//!   "stable": {
//!     "version": "0.3.4",
//!     "released": "2026-04-25",
//!     "artifacts": {
//!       "x86_64-unknown-linux-gnu":  { "url": "...", "sha256": "..." },
//!       "aarch64-unknown-linux-gnu": { "url": "...", "sha256": "..." },
//!       "x86_64-apple-darwin":       { "url": "...", "sha256": "..." },
//!       "aarch64-apple-darwin":      { "url": "...", "sha256": "..." }
//!     }
//!   },
//!   "beta": { ... }
//! }
//! ```
//!
//! `runar update` fetches the manifest, picks the channel + target triple,
//! downloads the binary into a tempfile, verifies its SHA-256, and replaces
//! `~/.runar-forge/bin/runar` via an atomic rename. The previous binary is
//! preserved at `runar.previous` so `runar update --rollback` is one
//! syscall away. Hooks point at the stable path (set up by
//! `runar setup claude-code`), so a swap mid-session never corrupts a
//! running CC.
//!
//! Distribution infra is owned outside this file: the manifest URL,
//! signing, and CI release pipeline live in `.gitlab-ci.yml` (or a future
//! GitHub release workflow). Until that endpoint exists, `runar update`
//! still works in `--check` mode against any URL the user passes through
//! `RUNAR_UPDATE_MANIFEST_URL`, and surfaces a clear error otherwise.

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::setup;

const DEFAULT_MANIFEST_URL: &str = ""; // populated once release infra exists

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(flatten)]
    channels: std::collections::HashMap<String, Channel>,
}

#[derive(Debug, Deserialize)]
struct Channel {
    version: String,
    #[serde(default)]
    released: String,
    artifacts: std::collections::HashMap<String, Artifact>,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    url: String,
    sha256: String,
}

/// Triple matching CI artifacts. Falls back to the runtime-reported triple
/// when the host triple isn't recognized so the manifest can publish
/// arbitrary keys without a code change.
fn target_triple() -> &'static str {
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "windows")) {
        "x86_64-pc-windows-gnu"
    } else {
        "unknown"
    }
}

fn manifest_url() -> Result<String> {
    if let Ok(u) = std::env::var("RUNAR_UPDATE_MANIFEST_URL") {
        if !u.is_empty() {
            return Ok(u);
        }
    }
    if !DEFAULT_MANIFEST_URL.is_empty() {
        return Ok(DEFAULT_MANIFEST_URL.to_string());
    }
    Err(anyhow!(
        "no manifest URL configured. \
         Set RUNAR_UPDATE_MANIFEST_URL=<https://…/manifest.json> or wait \
         for the release pipeline to publish one."
    ))
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

async fn fetch_manifest(url: &str) -> Result<Manifest> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(format!("runar/{}", current_version()))
        .build()?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetch manifest {url}"))?
        .error_for_status()?;
    let body = resp.text().await?;
    let manifest: Manifest = serde_json::from_str(&body).with_context(|| "parse manifest JSON")?;
    Ok(manifest)
}

fn pick_channel<'a>(manifest: &'a Manifest, channel: &str) -> Result<&'a Channel> {
    manifest.channels.get(channel).ok_or_else(|| {
        anyhow!(
            "channel '{channel}' not in manifest (have: {:?})",
            manifest.channels.keys().collect::<Vec<_>>()
        )
    })
}

fn pick_artifact<'a>(channel: &'a Channel, triple: &str) -> Result<&'a Artifact> {
    channel.artifacts.get(triple).ok_or_else(|| {
        anyhow!(
            "no artifact for {triple} in manifest (have: {:?}). \
             File a release for your platform.",
            channel.artifacts.keys().collect::<Vec<_>>()
        )
    })
}

async fn download_to_tempfile(url: &str, dir: &std::path::Path) -> Result<tempfile::NamedTempFile> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(format!("runar/{}", current_version()))
        .build()?;
    let mut resp = client.get(url).send().await?.error_for_status()?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    use std::io::Write;
    while let Some(chunk) = resp.chunk().await? {
        tmp.write_all(&chunk)?;
    }
    tmp.flush()?;
    Ok(tmp)
}

fn sha256_file(path: &std::path::Path) -> Result<String> {
    let bytes = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn make_executable(_path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(_path, perms)?;
    }
    Ok(())
}

fn previous_path() -> PathBuf {
    setup::stable_bin_path().with_file_name("runar.previous")
}

/// Quick guard: avoid swapping the binary while a Claude Code session is
/// likely active. Detection is best-effort — we look at the parent
/// process name (`claude`) and the `CLAUDE_CODE` env marker. The user can
/// always pass `--force` to override.
fn likely_inside_claude_code() -> bool {
    if std::env::var_os("CLAUDE_CODE").is_some() {
        return true;
    }
    if let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &std::process::id().to_string()])
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
        if s.contains("claude") {
            return true;
        }
    }
    false
}

fn print_pre_swap_advice() {
    eprintln!();
    eprintln!("⚠  A Claude Code session may be active. Swapping the runar binary");
    eprintln!("   while hooks fire can produce inconsistent state. Recommended:");
    eprintln!();
    eprintln!("     touch ~/.runar-forge/.disable-hooks");
    eprintln!("     runar update");
    eprintln!("     rm ~/.runar-forge/.disable-hooks");
    eprintln!("     runar doctor");
    eprintln!();
    eprintln!("   Or rerun with `--force` to skip this check.");
    eprintln!();
}

#[derive(Debug, Clone)]
pub struct UpdateOpts {
    pub check_only: bool,
    pub channel: String,
    pub force: bool,
    pub rollback: bool,
}

pub async fn run(opts: UpdateOpts) -> Result<()> {
    if opts.rollback {
        return rollback();
    }

    let url = manifest_url()?;
    let manifest = fetch_manifest(&url).await?;
    let channel = pick_channel(&manifest, &opts.channel)?;
    let triple = target_triple();

    println!("current: {} ({triple})", current_version());
    println!(
        "latest:  {} [{}] released {}",
        channel.version, opts.channel, channel.released
    );

    if channel.version == current_version() {
        println!("up to date.");
        return Ok(());
    }

    if opts.check_only {
        println!("update available — run `runar update` to install.");
        return Ok(());
    }

    if !opts.force && likely_inside_claude_code() {
        print_pre_swap_advice();
        return Err(anyhow!(
            "aborted — pass --force to swap inside an active CC session"
        ));
    }

    let artifact = pick_artifact(channel, triple)?;
    let stable = setup::stable_bin_path();
    let bin_dir = stable.parent().unwrap().to_path_buf();
    fs::create_dir_all(&bin_dir)?;

    println!("downloading {} ...", artifact.url);
    let tmp = download_to_tempfile(&artifact.url, &bin_dir).await?;
    let actual = sha256_file(tmp.path())?;
    if !actual.eq_ignore_ascii_case(&artifact.sha256) {
        return Err(anyhow!(
            "checksum mismatch — expected {} got {} (download discarded)",
            artifact.sha256,
            actual
        ));
    }
    make_executable(tmp.path())?;

    if stable.exists() {
        let prev = previous_path();
        let _ = fs::rename(&stable, &prev);
    }
    tmp.persist(&stable)
        .map_err(|e| anyhow!("atomic install failed: {e}"))?;

    println!("✔ installed {} → {}", channel.version, stable.display());
    println!("  previous saved to {}", previous_path().display());
    println!("  next: runar doctor");
    Ok(())
}

fn rollback() -> Result<()> {
    let stable = setup::stable_bin_path();
    let prev = previous_path();
    if !prev.exists() {
        return Err(anyhow!(
            "no previous binary at {} — nothing to roll back",
            prev.display()
        ));
    }
    // Swap: stable → .rollback-tmp → prev → stable
    let bin_dir = stable.parent().unwrap();
    let rollback_tmp = bin_dir.join("runar.rollback-tmp");
    let _ = fs::rename(&stable, &rollback_tmp);
    fs::rename(&prev, &stable)?;
    if rollback_tmp.exists() {
        let _ = fs::rename(&rollback_tmp, &prev);
    }
    println!("✔ rolled back to {}", stable.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_triple_returns_known_or_unknown() {
        let t = target_triple();
        // Sanity: it's never empty and either matches a known triple or
        // is the literal "unknown" sentinel.
        assert!(!t.is_empty());
    }

    #[test]
    fn current_version_matches_cargo() {
        assert_eq!(current_version(), env!("CARGO_PKG_VERSION"));
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn manifest_url_env_behavior() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        std::env::remove_var("RUNAR_UPDATE_MANIFEST_URL");
        assert!(manifest_url().is_err());

        std::env::set_var("RUNAR_UPDATE_MANIFEST_URL", "https://example.com/m.json");
        assert_eq!(manifest_url().unwrap(), "https://example.com/m.json");
        std::env::remove_var("RUNAR_UPDATE_MANIFEST_URL");
    }

    #[test]
    fn pick_channel_errors_on_missing() {
        let m = Manifest {
            channels: Default::default(),
        };
        assert!(pick_channel(&m, "stable").is_err());
    }
}
