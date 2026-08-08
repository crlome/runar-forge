//! Interactive TUI wizard for `runar init --interactive`.
//!
//! Walks a first-time user through storage selection, embedding config,
//! project detection, Claude Code setup, and initial crawl — then prints
//! a "you're done" summary. Each step can be skipped; the wizard never
//! blocks on a question that has a safe default.

use std::io::IsTerminal;
use std::path::Path;

use dialoguer::{Confirm, Select};

use crate::setup;

/// Refuse to launch any `dialoguer::*::interact()` call when stdin or stdout
/// is not a terminal. Without this guard the wizard blocks forever inside
/// Claude Code (piped stdin) or any non-tty pane, which is the symptom that
/// freezes the editor when a user runs `runar config wizard` from CC.
fn require_tty(cmd: &str) -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "{cmd} requires an interactive terminal. \
             Run it from a real shell (not Claude Code, not a piped stdin), \
             or use `runar config set KEY VALUE` for non-interactive setup."
        );
    }
    Ok(())
}

pub struct WizardResult {
    pub storage: String,
    pub embeddings: String,
    pub project_id: String,
    pub ran_setup: bool,
    pub ran_crawl: bool,
}

pub fn run_wizard() -> anyhow::Result<WizardResult> {
    require_tty("runar init --interactive")?;
    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║   RunarForge — Interactive Setup Wizard  ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();

    // ── 1. Storage backend ────────────────────────────────────────
    let docker_available = is_docker_running();
    let storage_options = if docker_available {
        vec![
            "SQLite (zero dependencies, single file, recommended for solo dev)",
            "PostgreSQL (via Docker, recommended for teams / semantic search)",
        ]
    } else {
        vec![
            "SQLite (zero dependencies, single file)",
            "PostgreSQL (Docker not detected — install Docker first for this option)",
        ]
    };

    let storage_idx = Select::new()
        .with_prompt("Storage backend")
        .items(&storage_options)
        .default(0)
        .interact()?;

    let storage = if storage_idx == 0 {
        "sqlite".to_string()
    } else {
        "postgresql".to_string()
    };

    // ── 2. Embedding provider ─────────────────────────────────────
    let has_openai_key = std::env::var("OPENAI_API_KEY")
        .map(|k| !k.is_empty())
        .unwrap_or(false);

    let embed_options = vec![
        "Disabled (FTS-only retrieval, no API key needed)",
        "OpenAI (requires OPENAI_API_KEY, 1536-dim vectors)",
        "Local ONNX (downloads ~90 MB model on first run, 384-dim, no API key)",
    ];

    let embed_default = if has_openai_key { 1 } else { 0 };
    let embed_idx = Select::new()
        .with_prompt("Embedding provider")
        .items(&embed_options)
        .default(embed_default)
        .interact()?;

    let embeddings = match embed_idx {
        1 => "openai".to_string(),
        2 => "local".to_string(),
        _ => "disabled".to_string(),
    };

    // ── 3. Project detection ──────────────────────────────────────
    let detected = setup::detect_project_id();
    let project_id = if Confirm::new()
        .with_prompt(format!("Detected project: \"{detected}\". Use this?"))
        .default(true)
        .interact()?
    {
        detected
    } else {
        dialoguer::Input::<String>::new()
            .with_prompt("Project identifier")
            .interact_text()?
    };

    // ── 4. Write .env ─────────────────────────────────────────────
    let env_path = setup::write_env_file(&storage)?;
    println!("\n  Config written: {}", env_path.display());

    // Append embedding setting
    append_env_if_missing(&env_path, "RUNAR_EMBEDDINGS", &embeddings)?;

    // ── 5. Claude Code setup ──────────────────────────────────────
    let ran_setup = Confirm::new()
        .with_prompt("Configure Claude Code integration now? (hooks + MCP + CLAUDE.md)")
        .default(true)
        .interact()?;

    if ran_setup {
        match setup::setup_claude_code(
            &project_id,
            false,
            // Read the installed choices back rather than rewriting them:
            // setup regenerates each hook key from these flags, so passing a
            // bare false here would uninstall a hook the user opted into.
            std::env::current_dir()
                .map(|d| setup::search_hints_installed(&d))
                .unwrap_or(false),
            std::env::current_dir()
                .map(|d| setup::graph_autorefresh_installed(&d))
                .unwrap_or(false),
            // Skills are inert markdown and default-on, but honour a
            // persisted `--no-skills` the same way the Setup command does.
            std::env::var("RUNAR_SKILLS")
                .map(|v| v != "false")
                .unwrap_or(true),
        ) {
            Ok(result) => {
                println!("  MCP registered:  {}", result.claude_json_path.display());
                println!("  Hooks written:   {}", result.settings_path.display());
                println!("  CLAUDE.md:       {}", result.claude_md_path.display());
            }
            Err(e) => {
                eprintln!("  Setup error: {e} (you can run `runar setup claude-code` later)");
            }
        }
    }

    // ── 6. Initial crawl ──────────────────────────────────────────
    let cwd = std::env::current_dir().unwrap_or_default();
    let has_git = cwd.join(".git").exists();
    let ran_crawl = if has_git {
        Confirm::new()
            .with_prompt(format!(
                "Run initial crawl on {} now? (populates memory)",
                cwd.display()
            ))
            .default(true)
            .interact()?
    } else {
        false
    };

    if ran_crawl {
        println!("\n  Crawling {}...", cwd.display());
        let status = std::process::Command::new(setup::detect_binary_path())
            .args([
                "crawl",
                cwd.to_str().unwrap_or("."),
                "--project",
                &project_id,
            ])
            .status();
        match status {
            Ok(s) if s.success() => println!("  Crawl complete."),
            Ok(s) => eprintln!("  Crawl exited with status {s}"),
            Err(e) => eprintln!("  Crawl failed: {e}"),
        }
    }

    // ── 7. Summary ────────────────────────────────────────────────
    println!();
    println!("  ┌──────────────────────────────────────────┐");
    println!("  │           Setup Complete!                 │");
    println!("  └──────────────────────────────────────────┘");
    println!();
    println!("  Storage:    {storage}");
    println!("  Embeddings: {embeddings}");
    println!("  Project:    {project_id}");
    if ran_setup {
        println!("  Claude Code: configured");
    }
    if ran_crawl {
        println!("  Initial crawl: done");
    }
    println!();
    println!("  Next steps:");
    if !ran_setup {
        println!("    runar setup claude-code --project {project_id}");
    }
    if !ran_crawl {
        println!("    runar crawl . --project {project_id}");
    }
    println!("    Other editors: runar setup [vscode|opencode|codex|cursor|windsurf]");
    println!("    Restart your editor to pick up the MCP server (+ hooks on Claude Code)");
    println!();

    Ok(WizardResult {
        storage,
        embeddings,
        project_id,
        ran_setup,
        ran_crawl,
    })
}

fn is_docker_running() -> bool {
    std::process::Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn append_env_if_missing(env_path: &Path, key: &str, value: &str) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(env_path).unwrap_or_default();
    if content.contains(&format!("{key}=")) {
        return Ok(());
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(env_path)?;
    writeln!(f, "{key}={value}")?;
    Ok(())
}
