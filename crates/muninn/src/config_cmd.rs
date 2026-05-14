//! `runar config` subcommand — manage `~/.runar-forge/.env`.
//!
//! Phase 5.5 Item 5.5.1. Replaces hand-editing `.env` with a first-class
//! CLI. Commands: path / show / get / set / unset / wizard.
//!
//! Atomic writes via `tempfile::NamedTempFile::persist` so a crashed `set`
//! never leaves a half-written `.env`. Comment + blank-line layout is
//! preserved across edits.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

use crate::setup;

/// Characters that MUST be percent-encoded inside the userinfo segment of a
/// URL (RFC 3986 §3.2.1). Anything not unreserved or sub-delim. We keep this
/// strict — easier to over-encode than to debug a broken URL.
const USERINFO: &AsciiSet = &CONTROLS
    .add(b' ').add(b'"').add(b'#').add(b'%').add(b'<').add(b'>')
    .add(b'?').add(b'`').add(b'{').add(b'}').add(b'/').add(b':')
    .add(b';').add(b'=').add(b'@').add(b'[').add(b'\\').add(b']')
    .add(b'^').add(b'|');

/// One physical line of `.env` content. We round-trip these so `set` does
/// not destroy the user's comments or blank-line spacing.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Comment(String),
    Blank,
    Kv {
        key: String,
        value: String,
        /// Original raw line — preserved when re-emitting unmodified rows.
        raw: String,
    },
}

#[derive(Debug, Clone)]
pub struct EnvFile {
    pub path: PathBuf,
    pub lines: Vec<Line>,
}

impl EnvFile {
    pub fn default_path() -> PathBuf {
        setup::runar_dir().join(".env")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = if path.exists() {
            fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?
        } else {
            String::new()
        };
        let lines = parse(&raw);
        Ok(Self { path: path.to_path_buf(), lines })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.lines.iter().rev().find_map(|l| match l {
            Line::Kv { key: k, value, .. } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    /// All `Kv` entries in encounter order. Later duplicates win on read,
    /// but `show` lists each occurrence so a duplicated key is visible.
    pub fn entries(&self) -> Vec<(&str, &str)> {
        self.lines.iter().filter_map(|l| match l {
            Line::Kv { key, value, .. } => Some((key.as_str(), value.as_str())),
            _ => None,
        }).collect()
    }

    /// Insert or update `key=value`. Updates the *first* occurrence in place
    /// (keeping its position), ignores any later duplicates. New keys are
    /// appended with a leading blank line if the file is non-empty.
    pub fn upsert(&mut self, key: &str, value: &str) {
        let new_raw = format!("{key}={value}");
        let mut updated = false;
        for line in self.lines.iter_mut() {
            if let Line::Kv { key: k, value: v, raw } = line {
                if k == key {
                    *v = value.to_string();
                    *raw = new_raw.clone();
                    updated = true;
                    break;
                }
            }
        }
        if !updated {
            if !self.lines.is_empty()
                && !matches!(self.lines.last(), Some(Line::Blank))
            {
                self.lines.push(Line::Blank);
            }
            self.lines.push(Line::Kv {
                key: key.to_string(),
                value: value.to_string(),
                raw: new_raw,
            });
        }
    }

    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.lines.len();
        self.lines.retain(|l| !matches!(l, Line::Kv { key: k, .. } if k == key));
        self.lines.len() != before
    }

    /// Atomic write: tempfile in same parent dir, then rename. Same-FS so
    /// the rename never crosses a device boundary (which would fall back
    /// to a non-atomic copy).
    pub fn save_atomic(&self) -> Result<()> {
        let parent = self.path.parent()
            .ok_or_else(|| anyhow!("path has no parent: {}", self.path.display()))?;
        fs::create_dir_all(parent)?;

        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        for line in &self.lines {
            match line {
                Line::Comment(s) => writeln!(tmp, "{s}")?,
                Line::Blank => writeln!(tmp)?,
                Line::Kv { raw, .. } => writeln!(tmp, "{raw}")?,
            }
        }
        tmp.flush()?;
        tmp.persist(&self.path)
            .map_err(|e| anyhow!("persist .env: {}", e.error))?;
        Ok(())
    }
}

fn parse(text: &str) -> Vec<Line> {
    text.lines().map(parse_line).collect()
}

fn parse_line(raw: &str) -> Line {
    let trimmed = raw.trim_start();
    if trimmed.is_empty() {
        return Line::Blank;
    }
    if trimmed.starts_with('#') {
        return Line::Comment(raw.to_string());
    }
    if let Some(eq) = trimmed.find('=') {
        let key = trimmed[..eq].trim().to_string();
        let value = trimmed[eq + 1..].to_string();
        if !key.is_empty() {
            return Line::Kv { key, value, raw: raw.to_string() };
        }
    }
    // Lines that look invalid are kept as comments so we never silently
    // drop user content. They still round-trip via raw match in save().
    Line::Comment(raw.to_string())
}

// ── masking ────────────────────────────────────────────────────────

/// True if a key name carries a secret. Loose match because env-var
/// conventions vary; better to over-mask than leak.
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    ["PASSWORD", "TOKEN", "SECRET", "API_KEY", "APIKEY"].iter()
        .any(|needle| k.contains(needle))
}

/// Mask a userinfo password inside a URL while leaving host/port/path
/// visible. `postgresql://u:p@h/db` → `postgresql://u:***@h/db`. Falls
/// back to the original string when no password is detected.
pub fn mask_url(url: &str) -> String {
    // Cheap parser — avoid pulling `url` crate just for this.
    if let Some(scheme_end) = url.find("://") {
        let after = &url[scheme_end + 3..];
        if let Some(at) = after.find('@') {
            let userinfo = &after[..at];
            if let Some(colon) = userinfo.find(':') {
                let user = &userinfo[..colon];
                let rest = &after[at..];
                return format!("{}://{}:***{}", &url[..scheme_end], user, rest);
            }
        }
    }
    url.to_string()
}

pub fn mask_value(key: &str, value: &str) -> String {
    if is_secret_key(key) {
        return "***".to_string();
    }
    if key.to_ascii_uppercase().contains("URL") {
        return mask_url(value);
    }
    value.to_string()
}

// ── url assembly (wizard) ──────────────────────────────────────────

pub fn build_db_url(host: &str, port: u16, db: &str, user: &str, password: &str) -> String {
    let user_enc = utf8_percent_encode(user, USERINFO);
    let pw_enc = utf8_percent_encode(password, USERINFO);
    format!("postgresql://{user_enc}:{pw_enc}@{host}:{port}/{db}")
}

// ── command dispatch ───────────────────────────────────────────────

/// Print resolved `.env` path. Always the global one — project-local
/// `.env` is intentionally not read by the binary.
pub fn cmd_path() -> Result<()> {
    println!("{}", EnvFile::default_path().display());
    Ok(())
}

pub fn cmd_show(unmask: bool) -> Result<()> {
    let env = EnvFile::load(&EnvFile::default_path())?;
    println!("# {}", env.path.display());

    for (key, value) in env.entries() {
        let file_val = if unmask { value.to_string() } else { mask_value(key, value) };
        let process_val = std::env::var(key).ok();
        let process_display = process_val.as_deref().map(|v| {
            if unmask { v.to_string() } else { mask_value(key, v) }
        });

        match process_display {
            Some(eff) if eff != file_val => {
                // Process env overrides file — surface the divergence.
                println!("{key}={file_val}    # effective: {eff}");
            }
            _ => println!("{key}={file_val}"),
        }
    }
    Ok(())
}

pub fn cmd_get(key: &str, unmask: bool) -> Result<()> {
    let env = EnvFile::load(&EnvFile::default_path())?;
    match env.get(key) {
        Some(v) => {
            let out = if unmask { v.to_string() } else { mask_value(key, v) };
            println!("{out}");
            Ok(())
        }
        None => Err(anyhow!("key not found in .env: {key}")),
    }
}

pub fn cmd_set(key: &str, value: &str) -> Result<()> {
    let path = EnvFile::default_path();
    let mut env = EnvFile::load(&path)?;
    env.upsert(key, value);
    env.save_atomic()?;
    println!("✔ updated {key} in {}", path.display());
    println!("  echo: {key}={}", mask_value(key, value));
    println!("  next: runar doctor");
    Ok(())
}

pub fn cmd_unset(key: &str) -> Result<()> {
    let path = EnvFile::default_path();
    let mut env = EnvFile::load(&path)?;
    if !env.remove(key) {
        return Err(anyhow!("key not found in .env: {key}"));
    }
    env.save_atomic()?;
    println!("✔ removed {key} from {}", path.display());
    Ok(())
}

pub fn cmd_wizard() -> Result<()> {
    use dialoguer::{Confirm, Input, Password, Select};
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "runar config wizard requires an interactive terminal. \
             Run it from a real shell (not Claude Code, not a piped stdin), \
             or use `runar config set KEY VALUE` for non-interactive setup."
        );
    }

    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║   runar config — storage configuration   ║");
    println!("  ╚══════════════════════════════════════════╝");
    println!();

    let backends = vec!["postgresql", "sqlite"];
    let backend_idx = Select::new()
        .with_prompt("Storage backend")
        .items(&backends)
        .default(0)
        .interact()?;
    let backend = backends[backend_idx];

    let path = EnvFile::default_path();
    let mut env = EnvFile::load(&path)?;
    env.upsert("RUNAR_STORAGE", backend);

    if backend == "postgresql" {
        let host: String = Input::new()
            .with_prompt("Host")
            .default("127.0.0.1".into())
            .interact_text()?;
        let port: u16 = Input::new()
            .with_prompt("Port")
            .default(5432_u16)
            .interact_text()?;
        let db: String = Input::new()
            .with_prompt("Database")
            .default("runar_memory".into())
            .interact_text()?;
        let user: String = Input::new()
            .with_prompt("User")
            .default("runar".into())
            .interact_text()?;
        let password: String = Password::new()
            .with_prompt("Password")
            .interact()?;

        let url = build_db_url(&host, port, &db, &user, &password);
        env.upsert("RUNAR_DB_URL", &url);

        println!();
        println!("Computed RUNAR_DB_URL: {}", mask_url(&url));
    } else {
        let default = setup::runar_dir().join("memory.db");
        let path_str: String = Input::new()
            .with_prompt("SQLite file")
            .default(default.display().to_string())
            .interact_text()?;
        env.upsert("RUNAR_SQLITE_PATH", &path_str);
    }

    let save = Confirm::new()
        .with_prompt("Write changes to .env?")
        .default(true)
        .interact()?;
    if !save {
        println!("aborted — no changes written");
        return Ok(());
    }

    env.save_atomic()?;
    println!("✔ wrote {}", path.display());
    println!("  next: runar doctor    # verify connectivity");
    println!("        restart MCP server (Claude Code: /mcp reconnect)");
    Ok(())
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(text: &str) -> EnvFile {
        EnvFile {
            path: PathBuf::from("/tmp/test.env"),
            lines: parse(text),
        }
    }

    #[test]
    fn parse_round_trip_preserves_layout() {
        let text = "# header comment\n\
                    KEY1=value1\n\
                    \n\
                    # section\n\
                    KEY2=value2\n";
        let env = fixture(text);
        assert_eq!(env.entries(), vec![("KEY1", "value1"), ("KEY2", "value2")]);
        assert!(matches!(env.lines[0], Line::Comment(_)));
        assert!(matches!(env.lines[2], Line::Blank));
    }

    #[test]
    fn upsert_updates_existing_in_place() {
        let mut env = fixture("# top\nKEY=old\nOTHER=stay\n");
        env.upsert("KEY", "new");
        assert_eq!(env.get("KEY"), Some("new"));
        // OTHER is still after KEY (position preserved).
        let keys: Vec<_> = env.entries().iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec!["KEY", "OTHER"]);
    }

    #[test]
    fn upsert_appends_new_key_with_blank_separator() {
        let mut env = fixture("KEY=val\n");
        env.upsert("NEW", "x");
        // …KEY, blank, NEW
        assert_eq!(env.lines.len(), 3);
        assert!(matches!(env.lines[1], Line::Blank));
        assert_eq!(env.get("NEW"), Some("x"));
    }

    #[test]
    fn upsert_preserves_comments() {
        let mut env = fixture("# explain\nKEY=v\n");
        env.upsert("KEY", "v2");
        assert!(matches!(env.lines[0], Line::Comment(ref s) if s == "# explain"));
        assert_eq!(env.get("KEY"), Some("v2"));
    }

    #[test]
    fn remove_returns_false_when_absent() {
        let mut env = fixture("A=1\n");
        assert!(!env.remove("MISSING"));
        assert!(env.remove("A"));
        assert_eq!(env.entries().len(), 0);
    }

    #[test]
    fn mask_url_strips_password() {
        assert_eq!(
            mask_url("postgresql://runar:secret@host:5432/db"),
            "postgresql://runar:***@host:5432/db"
        );
    }

    #[test]
    fn mask_url_passthrough_when_no_password() {
        assert_eq!(mask_url("postgresql://host/db"), "postgresql://host/db");
        assert_eq!(mask_url("not-a-url"), "not-a-url");
    }

    #[test]
    fn mask_value_masks_secret_keys() {
        assert_eq!(mask_value("POSTGRES_PASSWORD", "hunter2"), "***");
        assert_eq!(mask_value("RUNAR_API_KEY", "sk-abc"), "***");
        assert_eq!(mask_value("OPENAI_API_KEY", "sk-xyz"), "***");
        assert_eq!(mask_value("ANTHROPIC_TOKEN", "tok"), "***");
    }

    #[test]
    fn mask_value_masks_url_passwords() {
        assert_eq!(
            mask_value("RUNAR_DB_URL", "postgresql://u:p@h/d"),
            "postgresql://u:***@h/d"
        );
    }

    #[test]
    fn mask_value_passes_through_non_secret() {
        assert_eq!(mask_value("RUNAR_STORAGE", "postgresql"), "postgresql");
        assert_eq!(mask_value("RUNAR_DB_POOL_MAX", "10"), "10");
    }

    #[test]
    fn build_db_url_percent_encodes_special_chars() {
        let url = build_db_url("h", 5432, "db", "user", "p@ss:w/rd");
        assert_eq!(url, "postgresql://user:p%40ss%3Aw%2Frd@h:5432/db");
    }

    #[test]
    fn save_atomic_round_trips_via_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "# hi\nA=1\n").unwrap();

        let mut env = EnvFile::load(&path).unwrap();
        env.upsert("A", "2");
        env.upsert("B", "new");
        env.save_atomic().unwrap();

        let reloaded = EnvFile::load(&path).unwrap();
        assert_eq!(reloaded.get("A"), Some("2"));
        assert_eq!(reloaded.get("B"), Some("new"));
        // First line is still the comment.
        assert!(matches!(reloaded.lines[0], Line::Comment(_)));
    }

    #[test]
    fn load_missing_file_yields_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.env");
        let env = EnvFile::load(&path).unwrap();
        assert!(env.lines.is_empty());
    }

    #[test]
    fn entries_show_duplicates_in_order_get_returns_last() {
        let env = fixture("DUP=a\nDUP=b\n");
        assert_eq!(env.entries().len(), 2);
        assert_eq!(env.get("DUP"), Some("b"));
    }
}
