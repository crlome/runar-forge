//! `runar update` — first-class self-update so the user never has to
//! `cargo install --path` again.
//!
//! Operating model
//! ---------------
//! The update flow reads a JSON manifest at `RUNAR_UPDATE_MANIFEST_URL`
//! (defaults to the latest GitHub release's `manifest.json`).
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
//! Distribution lives in `.github/workflows/release.yml`, which publishes
//! `manifest.json` as a release asset. The only integrity check is the
//! SHA-256 per artifact, read from that same manifest; the manifest is not
//! signed, so the check catches a corrupt or truncated download, not a
//! tampered release. `RUNAR_UPDATE_MANIFEST_URL` overrides the manifest URL.

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::setup;

// Stable redirect to the newest non-prerelease release's manifest. The
// release pipeline (.github/workflows/release.yml) publishes manifest.json as
// a release asset; `releases/latest/download/…` always resolves to the latest.
const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/crlome/runar-forge/releases/latest/download/manifest.json";

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
        // Must match the triple the release pipeline publishes (MSVC, not GNU)
        // — see RELEASE_TARGETS in tests + .github/workflows/release.yml.
        "x86_64-pc-windows-msvc"
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

    /// Targets the release pipeline actually publishes. Keep in lockstep with
    /// the `matrix.target` list in `.github/workflows/release.yml` — a binary
    /// whose host triple isn't here has no artifact for `runar update` to fetch.
    const RELEASE_TARGETS: &[&str] = &[
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ];

    #[test]
    fn target_triple_matches_a_release_artifact() {
        let t = target_triple();
        assert!(!t.is_empty());
        // Runs on every supported CI host (linux x86_64/aarch64, macOS
        // arm64/x86_64, windows x86_64-msvc), so the host triple must be one
        // the release pipeline ships — this catches a code/CI drift such as
        // the historical `x86_64-pc-windows-gnu` vs the published `-msvc`.
        assert!(
            RELEASE_TARGETS.contains(&t),
            "target_triple() returned `{t}`, which release.yml does not publish; \
             update target_triple() or the release matrix to match"
        );
    }

    #[test]
    fn current_version_matches_cargo() {
        assert_eq!(current_version(), env!("CARGO_PKG_VERSION"));
    }

    /// The README claimed "signed release manifests"; `run` checks a SHA-256
    /// taken from the same unsigned manifest. A security claim the code does
    /// not make must not come back.
    #[test]
    fn readme_describes_what_update_actually_verifies() {
        let readme = include_str!("../../../README.md");
        // The bullet: its first line and the indented lines that continue it.
        let mut lines = readme
            .lines()
            .skip_while(|l| !l.contains("**Self-update.**"));
        let first = lines.next().expect("README has a Self-update bullet");
        let claim = std::iter::once(first)
            .chain(lines.take_while(|l| l.starts_with("  ")))
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(claim.contains("SHA-256"), "{claim}");
        assert!(claim.contains("not signed"), "{claim}");
        assert!(!claim.contains("signed release"), "{claim}");
    }

    /// Targets whose binary the build runner can execute. aarch64-linux is
    /// cross-built, and x86_64-apple-darwin is built on an arm64 runner.
    const RUNNABLE_TARGETS: &[&str] = &[
        "x86_64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ];

    /// Each gate stops a release that would look green while shipping
    /// something broken: a tag whose version the binary does not report
    /// (`run` compares the manifest version with its own by strict
    /// equality, so `runar update` would re-offer the same binary forever),
    /// a binary that does not start, npm packages published before the
    /// GitHub release exists, or a missing npm token that leaves npm users
    /// with nothing.
    #[test]
    fn release_pipeline_keeps_its_gates() {
        let workflow = include_str!("../../../.github/workflows/release.yml");
        let mut lost: Vec<&str> = Vec::new();
        let mut gate = |kept: bool, what: &'static str| {
            if !kept {
                lost.push(what);
            }
        };

        let preflight = job_lines(workflow, "preflight");
        gate(
            !preflight.is_empty() && !preflight.iter().any(|l| l.starts_with("    if:")),
            "a preflight job with no job-level `if:` (a skipped need would skip \
             `build` on workflow_dispatch, the only dry run)",
        );
        let compare = step_lines(&preflight, "Tag matches the Cargo.toml version");
        gate(
            has(&compare, "if: startsWith(github.ref, 'refs/tags/v')")
                && has(&compare, "GITHUB_REF_NAME")
                && has(&compare, "Cargo.toml")
                && has(&compare, "exit 1"),
            "preflight fails a tag whose version differs from Cargo.toml",
        );

        let build = job_lines(workflow, "build");
        gate(has(&build, "needs: preflight"), "build needs preflight");

        let matrix = matrix_entries(&build);
        let mut shipped: Vec<&str> = matrix.iter().map(|(t, _)| t.as_str()).collect();
        let mut expected = RELEASE_TARGETS.to_vec();
        shipped.sort_unstable();
        expected.sort_unstable();
        gate(
            shipped == expected,
            "the build matrix ships exactly RELEASE_TARGETS",
        );
        gate(
            RUNNABLE_TARGETS
                .iter()
                .all(|t| matrix.iter().any(|(m, smoke)| m == t && *smoke)),
            "`smoke: true` on every target the runner can execute",
        );

        let smoke = step_lines(&build, "Smoke the built binary");
        gate(
            has(&smoke, "if: matrix.smoke")
                && has(&smoke, "--version")
                && has(&smoke, "GITHUB_REF_NAME")
                && has(&smoke, "exit 1"),
            "the smoke step runs the binary and, on tags, requires the tag's version",
        );
        let at = |name: &str| step_position(&build, name);
        gate(
            matches!(
                (at("Codesign (macOS, ad-hoc)"), at("Smoke the built binary"), at("Package")),
                (Some(sign), Some(run), Some(pack)) if sign < run && run < pack
            ),
            "the smoke step runs after codesign and before packaging",
        );

        let npm = job_lines(workflow, "npm-publish");
        gate(
            has(&npm, "needs: [build, publish]"),
            "npm-publish waits for the GitHub release",
        );
        let token_check: Vec<&str> = npm
            .iter()
            .skip_while(|l| !l.contains("-z \"${NODE_AUTH_TOKEN"))
            .take_while(|l| l.trim() != "fi")
            .copied()
            .collect();
        gate(
            has(&token_check, "exit 1") && !has(&token_check, "exit 0"),
            "a missing NPM_TOKEN fails npm-publish instead of skipping it",
        );

        assert!(
            lost.is_empty(),
            "release.yml lost its gates:\n- {}",
            lost.join("\n- ")
        );
    }

    fn has(lines: &[&str], needle: &str) -> bool {
        lines.iter().any(|l| l.contains(needle))
    }

    fn indent(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    /// One job of a workflow: its `  <name>:` key and every line up to the
    /// next key at job level or above. Empty when the job does not exist.
    fn job_lines<'a>(workflow: &'a str, name: &str) -> Vec<&'a str> {
        let key = format!("  {name}:");
        let mut lines = workflow.lines().skip_while(|l| l.trim_end() != key);
        let Some(first) = lines.next() else {
            return Vec::new();
        };
        let ends_job = |l: &&str| {
            let text = l.trim();
            !text.is_empty() && !text.starts_with('#') && indent(l) <= 2
        };
        std::iter::once(first)
            .chain(lines.take_while(|l| !ends_job(l)))
            .collect()
    }

    /// The step named `name` in a job: its `- name:` line and every line up
    /// to the next step. Empty when the step does not exist.
    fn step_lines<'a>(job: &[&'a str], name: &str) -> Vec<&'a str> {
        let header = format!("- name: {name}");
        let mut lines = job.iter().copied().skip_while(|l| l.trim() != header);
        let Some(first) = lines.next() else {
            return Vec::new();
        };
        let step_indent = indent(first);
        std::iter::once(first)
            .chain(lines.take_while(|l| l.trim().is_empty() || indent(l) > step_indent))
            .collect()
    }

    fn step_position(job: &[&str], name: &str) -> Option<usize> {
        let header = format!("- name: {name}");
        job.iter().position(|l| l.trim() == header)
    }

    /// `(target, smoke)` for each entry of the build matrix.
    fn matrix_entries(build: &[&str]) -> Vec<(String, bool)> {
        let mut entries: Vec<(String, bool)> = Vec::new();
        for line in build.iter().map(|l| l.trim()) {
            if line.starts_with("runs-on:") {
                break;
            }
            if line.starts_with("- runner:") {
                entries.push((String::new(), false));
                continue;
            }
            let Some(entry) = entries.last_mut() else {
                continue;
            };
            if let Some(target) = line.strip_prefix("target:") {
                entry.0 = target.trim().to_string();
            } else if line == "smoke: true" {
                entry.1 = true;
            }
        }
        entries
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn manifest_url_env_behavior() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Env unset → falls back to the published default manifest URL.
        std::env::remove_var("RUNAR_UPDATE_MANIFEST_URL");
        assert_eq!(manifest_url().unwrap(), DEFAULT_MANIFEST_URL);
        assert!(!DEFAULT_MANIFEST_URL.is_empty());

        // Env set → overrides the default.
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
