//! Memory Protocol — mandatory save instructions injected via PreToolUse hook.
//!
//! Re-injected on every tool call so it survives context compaction.

use std::fs;

use serde::Deserialize;

use crate::setup::ping_file_path;

const MAX_FILES: usize = 10;

pub fn build_memory_protocol(project_id: &str, files_modified: &[String]) -> String {
    let files_list = if files_modified.is_empty() {
        "_none yet_".to_string()
    } else {
        files_modified
            .iter()
            .take(MAX_FILES)
            .map(|f| format!("- `{f}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        r#"## Muninn Memory Protocol — MANDATORY and ALWAYS ACTIVE

Call `muninn_save` IMMEDIATELY after ANY of these:
- Decision made (architecture, convention, workflow, tool choice)
- Bug fixed (include root cause + fix)
- Convention or workflow documented/updated
- Non-obvious discovery, gotcha, or edge case found
- Pattern established (naming, structure, approach)
- User preference or constraint learned
- Feature implemented with non-obvious approach
- User confirms a recommendation ("yes", "go with that", "sounds good", "dale", "sí")
- User rejects an approach or expresses preference ("no, better X", "I prefer X")
- Discussion concludes with a clear direction chosen
- Question answered that reveals a business rule or config requirement
- Error diagnosed — root cause identified

**Self-check after EVERY response**: "Did I or the user just make a decision, confirm a direction, express a preference, fix a bug, learn something, or discover a gotcha? If yes → `muninn_save` NOW."

### Save format:
- **title:** Short, searchable (5-200 chars)
- **content:** What, why, where. Include file paths.
- **type:** decision | bug | pattern | business-rule | architecture | preference | note
- **topicKey:** For evolving decisions use "scope:key" e.g. "auth:approach"
- **projectId:** "{project_id}"

### Examples of saves people miss:
- "CONEKTA_API_KEY should be private key" → type='business-rule', save it
- "Cart requires auth middleware on /checkout" → type='architecture', save it
- "User said always use snake_case for DB columns" → type='preference', save it
- "Fixed 500 error: missing null check in order service" → type='bug', save it

### SEARCH MEMORY when:
- User asks about a topic you have no context on → `muninn_search` first
- User's message references a feature, problem, or past work → search before responding
- Starting work on something that might have prior decisions → check memory
- User asks to recall anything ("remember", "what did we do", "how did we handle")
- Beginning a planning/design task → search for related past decisions

### During planning/design:
- Search memory first for related past decisions
- After planning, save each key decision as a separate `muninn_save` (not the full plan)
- Use topicKey for evolving decisions: "plan:feature-name"

### After completing a multi-step task:
- Save a summary: what was built, key decisions made, files changed
- Use type='note' with topicKey: "summary:feature-name"

### What NOT to save:
- Trivial reads/searches with no new insight
- Intermediate steps that didn't produce a result

### Files modified this session:
{files_list}"#
    )
}

#[derive(Debug, Default, Clone, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PingData {
    /// Files modified in the current session (cumulative).
    #[serde(default)]
    pub files_modified: Vec<String>,
    /// Unix ms timestamp of last `session ping`.
    #[serde(default)]
    pub last_ping: Option<i64>,
    /// Unix ms timestamp of last `save-ack` (after muninn_save).
    #[serde(default)]
    pub last_save: Option<i64>,
    /// Unix ms timestamp of session start (used for nudge age check).
    #[serde(default)]
    pub session_started_at: Option<i64>,
}

/// Read the per-project ping file, returning defaults if it doesn't exist
/// or is malformed. Hook flow must never fail on missing ping state.
pub fn read_ping(project_id: &str) -> PingData {
    let path = ping_file_path(project_id);
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => PingData::default(),
    }
}

/// Write the per-project ping file atomically. Failure is non-fatal — hooks
/// must never block on disk IO errors.
pub fn write_ping(project_id: &str, data: &PingData) -> std::io::Result<()> {
    let path = ping_file_path(project_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let json = serde_json::to_string(data).unwrap_or_default();
    fs::write(&path, json)
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── Nudge ──────────────────────────────────────────────────────────

pub const NUDGE_THRESHOLD_MS: i64 = 15 * 60 * 1000;
pub const MIN_SESSION_AGE_MS: i64 = 5 * 60 * 1000;
pub const MIN_PROMPT_LENGTH: usize = 20;

pub fn is_trivial_prompt(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return true;
    }
    // Slash commands
    if trimmed.starts_with('/') {
        return true;
    }
    // One-word acknowledgements
    let lower = trimmed.to_lowercase();
    matches!(
        lower.as_str(),
        "ok" | "yes"
            | "no"
            | "y"
            | "n"
            | "sure"
            | "thanks"
            | "ty"
            | "thx"
            | "done"
            | "go"
            | "continue"
            | "proceed"
            | "please"
            | "dale"
            | "sí"
    )
}

/// True when a UserPromptSubmit payload is machine-generated (IDE selection
/// dumps, slash-command expansions, task notifications) or a bulk log paste
/// rather than a genuinely typed user prompt. Claude Code sends everything
/// through the same flat `prompt` field, so detection is textual.
pub fn is_machine_generated_prompt(s: &str) -> bool {
    let lower = s.to_lowercase();
    const MARKERS: &[&str] = &[
        "<system-reminder>",
        "<task-notification>",
        "<task_notification>",
        "<command-name>",
        "<command-message>",
        "<local-command-stdout>",
        "<ide_selection>",
        "the user selected the following lines",
        "the user opened the file",
        "[pasted text #",
    ];
    if MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }

    // Bulk log-paste heuristic: mostly log-shaped lines. A typed question
    // with a pasted error stays under the ratio bar.
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() > 30 {
        let log_like = lines
            .iter()
            .filter(|l| {
                let t = l.trim_start();
                t.len() >= 10
                    && (t.starts_with("at ")
                        || t.starts_with('[')
                        || t.chars()
                            .take(10)
                            .collect::<String>()
                            .matches(|c: char| c.is_ascii_digit())
                            .count()
                            >= 6
                        || ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"]
                            .iter()
                            .any(|lvl| t.starts_with(lvl) || t.get(1..1 + lvl.len()) == Some(lvl)))
            })
            .count();
        if log_like * 100 > lines.len() * 80 {
            return true;
        }
    }
    false
}

/// First-message payload when no ping file exists (Claude's first prompt).
pub fn first_message_reminder() -> String {
    "CRITICAL FIRST ACTION: Before responding to the user, \
call muninn_search with keywords from their message \
to check for prior decisions, patterns, and context. \
Then respond using what you found. Do NOT answer blind. \
Also available: muninn_save, muninn_context, curator_ask."
        .to_string()
}

/// Idle-save reminder payload (>15 min since last muninn_save).
pub fn idle_nudge_message(minutes_since: i64, files: &[String]) -> String {
    let file_context = if files.is_empty() {
        "(none yet)".to_string()
    } else {
        files.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
    };
    format!(
        "MEMORY REMINDER: {minutes_since} minutes since last muninn_save. \
Reflect: Did you or the user learn something, make a decision, \
confirm a direction, discover a gotcha, express a preference, \
fix a bug, or answer a question that reveals a business rule? \
If yes to ANY → muninn_save NOW. \
Files modified: {file_context}"
    )
}

/// Extract the user prompt from a Claude Code UserPromptSubmit hook payload.
/// Current Claude Code sends `prompt`; older/TS-era payloads sent `user_prompt`.
/// Accept both so the hook keeps working across versions.
pub fn parse_user_prompt(stdin_payload: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Payload {
        prompt: Option<String>,
        user_prompt: Option<String>,
    }
    serde_json::from_str::<Payload>(stdin_payload)
        .ok()
        .and_then(|p| p.prompt.or(p.user_prompt))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_generated_markers_detected() {
        let cases = [
            "<system-reminder>recalled memory blah</system-reminder> plus text",
            "context: <task-notification>done</task-notification>",
            "<command-name>/model</command-name><command-message>model</command-message>",
            "<local-command-stdout>Set model</local-command-stdout>",
            "The user selected the following lines from src/app.ts: 10-20",
            "notes [Pasted text #1 +422 lines]",
            "<ide_selection>fn main() {}</ide_selection>",
        ];
        for case in cases {
            assert!(is_machine_generated_prompt(case), "should detect: {case}");
        }
    }

    #[test]
    fn genuine_prompts_pass_machine_filter() {
        assert!(!is_machine_generated_prompt(
            "can you refactor the auth middleware to use the new token validator?"
        ));
        // Typed question with a short pasted error stays under the ratio bar.
        let mixed = "why does the build fail with this?\n\nERROR in ./src/index.ts\nModule not found: ./missing";
        assert!(!is_machine_generated_prompt(mixed));
    }

    #[test]
    fn bulk_log_paste_detected() {
        let mut paste = String::new();
        for i in 0..50 {
            paste.push_str(&format!(
                "2026-07-19T10:00:{i:02}Z ERROR service failed to connect attempt {i}\n"
            ));
        }
        assert!(is_machine_generated_prompt(&paste));
    }

    #[test]
    fn protocol_includes_project_id() {
        let out = build_memory_protocol("myproj", &[]);
        assert!(out.contains("\"myproj\""));
        assert!(out.contains("Muninn Memory Protocol"));
        assert!(out.contains("_none yet_"));
    }

    #[test]
    fn protocol_lists_files() {
        let files = vec!["src/a.ts".into(), "src/b.ts".into()];
        let out = build_memory_protocol("p", &files);
        assert!(out.contains("- `src/a.ts`"));
        assert!(out.contains("- `src/b.ts`"));
    }

    #[test]
    fn protocol_caps_at_10_files() {
        let files: Vec<String> = (0..20).map(|i| format!("f{i}.ts")).collect();
        let out = build_memory_protocol("p", &files);
        assert!(out.contains("- `f0.ts`"));
        assert!(out.contains("- `f9.ts`"));
        assert!(!out.contains("- `f10.ts`"));
    }

    #[test]
    fn read_ping_missing_returns_default() {
        // project we know has no ping file
        let data = read_ping("non-existent-project-12345");
        assert!(data.files_modified.is_empty());
        assert!(data.last_save.is_none());
    }

    #[test]
    fn ping_round_trip() {
        // Use a unique project name + default TMPDIR to avoid env var races
        // with setup::tests (which mutate HOME). The ping file path resolves
        // through TMPDIR which we leave at the process default.
        let pid = format!("round_trip_test_{}", std::process::id());
        let data = PingData {
            files_modified: vec!["src/a.ts".into()],
            last_ping: Some(1_000),
            last_save: Some(2_000),
            session_started_at: Some(500),
        };
        write_ping(&pid, &data).unwrap();
        let back = read_ping(&pid);
        assert_eq!(back.files_modified, vec!["src/a.ts"]);
        assert_eq!(back.last_save, Some(2_000));
        // cleanup
        let _ = std::fs::remove_file(ping_file_path(&pid));
    }

    #[test]
    fn trivial_prompt_detection() {
        assert!(is_trivial_prompt(""));
        assert!(is_trivial_prompt("yes"));
        assert!(is_trivial_prompt("YES"));
        assert!(is_trivial_prompt("/help"));
        assert!(is_trivial_prompt("dale"));
        assert!(!is_trivial_prompt("implement the new login flow"));
    }

    #[test]
    fn parse_user_prompt_shape() {
        // Current Claude Code payload
        let current = r#"{"prompt": "hello world", "session_id": "x"}"#;
        assert_eq!(parse_user_prompt(current), Some("hello world".into()));
        // Legacy field still accepted
        let legacy = r#"{"user_prompt": "legacy prompt", "session_id": "x"}"#;
        assert_eq!(parse_user_prompt(legacy), Some("legacy prompt".into()));
        // `prompt` wins when both are present
        let both = r#"{"prompt":"new","user_prompt":"old"}"#;
        assert_eq!(parse_user_prompt(both), Some("new".into()));
        assert_eq!(parse_user_prompt("{}"), None);
        assert_eq!(parse_user_prompt("not json"), None);
        assert_eq!(parse_user_prompt(r#"{"prompt":""}"#), None);
        assert_eq!(parse_user_prompt(r#"{"user_prompt":""}"#), None);
    }

    #[test]
    fn nudge_message_includes_minutes_and_files() {
        let msg = idle_nudge_message(17, &["a.ts".into(), "b.ts".into()]);
        assert!(msg.contains("17 minutes"));
        assert!(msg.contains("a.ts, b.ts"));
    }
}
