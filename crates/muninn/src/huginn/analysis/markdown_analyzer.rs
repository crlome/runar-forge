//! Markdown analyzer — produces a structural summary of `.md` / `.mdx`
//! files so the crawler can surface architecture docs, decision logs,
//! phase plans, and onboarding material as first-class memory entries
//! instead of a generic per-file blob.
//!
//! Design notes
//! - Deliberately text-based (no pulldown-cmark dep): heading hierarchy +
//!   first meaningful paragraph + bullet-list classification is enough
//!   for our needs.
//! - `classify_path` decides whether a given `.md` is architecture-worthy
//!   (README, CLAUDE, AGENTS, `/docs/*`, `/phases/*`). Non-architecture
//!   Markdown still gets analyzed but lands as a Context entry.

use super::DeepAnalysis;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownRole {
    /// `README.md`, `CLAUDE.md`, `AGENTS.md`, `/docs/**`, `/phases/**`
    Architecture,
    /// Any other `.md` — release notes, changelogs, notes, etc.
    Contextual,
}

#[derive(Debug, Clone)]
pub struct Heading {
    pub level: u8,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct MarkdownAnalysis {
    pub role: MarkdownRole,
    pub purpose: String,
    pub headings: Vec<Heading>,
    pub responsibilities: Vec<String>,
    pub decisions: Vec<String>,
    pub rules: Vec<String>,
}

pub fn classify_path(relative_path: &str) -> MarkdownRole {
    let p = relative_path.replace('\\', "/").to_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p).to_string();

    if matches!(
        name.as_str(),
        "readme.md" | "claude.md" | "agents.md" | "contributing.md"
    ) {
        return MarkdownRole::Architecture;
    }
    if p.contains("/docs/") || p.starts_with("docs/") {
        return MarkdownRole::Architecture;
    }
    if p.contains("/phases/") || p.starts_with("phases/") {
        return MarkdownRole::Architecture;
    }
    MarkdownRole::Contextual
}

/// Run the structural Markdown analysis on a file's text content.
pub fn analyze(content: &str, relative_path: &str) -> MarkdownAnalysis {
    let role = classify_path(relative_path);
    let headings = extract_headings(content);
    let purpose = extract_purpose(content, &headings, relative_path);
    let (responsibilities, decisions, rules) = classify_bullets(content);

    MarkdownAnalysis {
        role,
        purpose,
        headings,
        responsibilities,
        decisions,
        rules,
    }
}

/// Fold a `MarkdownAnalysis` into the generic `DeepAnalysis` shape so the
/// rest of the crawl pipeline (memory-entry generation, synthesizer, etc.)
/// can consume it uniformly. Leaves code-specific fields empty.
pub fn into_deep(md: MarkdownAnalysis) -> DeepAnalysis {
    DeepAnalysis {
        purpose: md.purpose,
        responsibilities: md.responsibilities,
        key_exports: md
            .headings
            .iter()
            .filter(|h| h.level <= 2)
            .map(|h| h.text.clone())
            .take(10)
            .collect(),
        key_imports: Vec::new(),
        patterns: Vec::new(),
        business_rules: md.rules,
        gotchas: Vec::new(),
        security_notes: Vec::new(),
        relationships: Vec::new(),
        tech_debt_markers: Vec::new(),
    }
    // Decision-like bullets fold into business_rules since DeepAnalysis has
    // no dedicated `decisions` bucket.
    .extend_with_decisions(md.decisions)
}

// Tiny extension trait so the builder above can stay expression-shaped.
trait WithDecisions {
    fn extend_with_decisions(self, decisions: Vec<String>) -> Self;
}

impl WithDecisions for DeepAnalysis {
    fn extend_with_decisions(mut self, decisions: Vec<String>) -> Self {
        for d in decisions {
            self.business_rules.push(format!("decision: {d}"));
        }
        self
    }
}

fn extract_headings(content: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut in_code_fence = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if hashes == 0 || hashes > 6 {
            continue;
        }
        // `# Heading` requires a space after the hashes.
        let rest = &trimmed[hashes..];
        if !rest.starts_with(' ') {
            continue;
        }
        let text = rest.trim().trim_end_matches('#').trim().to_string();
        if text.is_empty() {
            continue;
        }
        out.push(Heading {
            level: hashes as u8,
            text,
        });
    }
    out
}

fn extract_purpose(content: &str, headings: &[Heading], relative_path: &str) -> String {
    // Prefer: first non-empty, non-heading, non-blockquote, non-code paragraph
    // that lives before any H2+ boundary. Fall back to the top H1 heading,
    // then the filename.
    let mut paragraph: Vec<String> = Vec::new();
    let mut in_code_fence = false;
    for raw in content.lines() {
        let line = raw.trim_start();
        if line.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }
        if line.is_empty() {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        if line.starts_with('#') || line.starts_with('>') || line.starts_with("<!--") {
            continue;
        }
        // Skip badge / link-only lines (start with `[![`).
        if line.starts_with("[![") {
            continue;
        }
        paragraph.push(strip_md_inline(line).to_string());
        if paragraph.join(" ").len() > 280 {
            break;
        }
    }

    if !paragraph.is_empty() {
        let joined = paragraph.join(" ");
        return truncate_to(&joined, 280);
    }

    if let Some(h1) = headings.iter().find(|h| h.level == 1) {
        return h1.text.clone();
    }
    // Filename as the last resort, without the extension.
    relative_path
        .rsplit('/')
        .next()
        .unwrap_or(relative_path)
        .trim_end_matches(".md")
        .trim_end_matches(".mdx")
        .replace(['-', '_'], " ")
}

/// Classify top-level bullet lines into responsibilities, decisions, rules.
/// Heuristic, not a parser: catches common wording without false positives.
fn classify_bullets(content: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut responsibilities: Vec<String> = Vec::new();
    let mut decisions: Vec<String> = Vec::new();
    let mut rules: Vec<String> = Vec::new();
    let mut in_code_fence = false;

    for raw in content.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }
        let is_bullet =
            trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ");
        if !is_bullet {
            continue;
        }
        let body = trimmed[2..].trim();
        if body.is_empty() {
            continue;
        }
        let stripped = strip_md_inline(body).to_string();
        let lower = stripped.to_lowercase();

        // Decision signals (capture strong verbs / rationale patterns).
        if lower.starts_with("decided ")
            || lower.starts_with("chose ")
            || lower.contains(" we chose ")
            || lower.contains(" chose to ")
            || lower.contains("rationale:")
            || lower.contains("decision:")
        {
            decisions.push(stripped);
            continue;
        }

        // Rule signals (modal verbs, MUST/NEVER/ALWAYS, "required").
        if lower.contains("must ")
            || lower.contains("never ")
            || lower.contains("always ")
            || lower.contains("required")
            || lower.contains("do not ")
            || lower.contains("don't ")
        {
            rules.push(stripped);
            continue;
        }

        // Everything else becomes a responsibility (capped).
        if responsibilities.len() < 20 {
            responsibilities.push(stripped);
        }
    }
    (responsibilities, decisions, rules)
}

/// Strip common inline markup for cleaner memory-entry content. Removes
/// bold/italic markers, inline-code backticks, and link text brackets —
/// purely cosmetic so downstream search doesn't hit stray underscores.
fn strip_md_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '_' => {
                // collapse repeated markers
                while let Some(&next) = chars.peek() {
                    if next == c {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            '`' => { /* drop */ }
            '[' => {
                // `[text](url)` → `text`
                let mut link_text = String::new();
                for ch in chars.by_ref() {
                    if ch == ']' {
                        break;
                    }
                    link_text.push(ch);
                }
                // consume `(url)` if present
                if chars.peek() == Some(&'(') {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ch == ')' {
                            break;
                        }
                    }
                }
                out.push_str(&link_text);
            }
            other => out.push(other),
        }
    }
    out
}

fn truncate_to(s: &str, max: usize) -> String {
    crate::text::truncate_ellipsis(s, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_path_architecture_roles() {
        assert_eq!(classify_path("README.md"), MarkdownRole::Architecture);
        assert_eq!(classify_path("CLAUDE.md"), MarkdownRole::Architecture);
        assert_eq!(classify_path("AGENTS.md"), MarkdownRole::Architecture);
        assert_eq!(
            classify_path("docs/getting-started.md"),
            MarkdownRole::Architecture
        );
        assert_eq!(
            classify_path("phases/PHASE-4.8.md"),
            MarkdownRole::Architecture
        );
    }

    #[test]
    fn classify_path_contextual_default() {
        assert_eq!(classify_path("RELEASE-NOTES.md"), MarkdownRole::Contextual);
    }

    #[test]
    fn extracts_headings_skipping_code_fences() {
        let src = "# Title\n\n```\n## not a heading\n```\n\n## Real H2\n### Real H3\n";
        let hs = extract_headings(src);
        assert_eq!(hs.len(), 3);
        assert_eq!(hs[0].level, 1);
        assert_eq!(hs[0].text, "Title");
        assert_eq!(hs[1].text, "Real H2");
        assert_eq!(hs[2].text, "Real H3");
    }

    #[test]
    fn purpose_pulls_first_paragraph() {
        let src = "# Title\n\nThis project does _structured_ things using `pgvector`.\nSecond line.\n\n## H2\n\nOther.";
        let hs = extract_headings(src);
        let purpose = extract_purpose(src, &hs, "README.md");
        assert!(purpose.contains("structured"));
        assert!(purpose.contains("pgvector"));
        assert!(!purpose.contains("`"));
    }

    #[test]
    fn purpose_falls_back_to_h1_then_filename() {
        let only_h1 = "# Just A Title\n";
        let hs = extract_headings(only_h1);
        assert_eq!(extract_purpose(only_h1, &hs, "x.md"), "Just A Title");

        let empty = "";
        let hs2 = extract_headings(empty);
        assert_eq!(extract_purpose(empty, &hs2, "docs/my-thing.md"), "my thing");
    }

    #[test]
    fn classifies_decision_and_rule_bullets() {
        let src = "- Decided to use pgvector for semantic search\n\
                   - Chose Rust over Go because of clippy gating\n\
                   - MUST never commit secrets\n\
                   - Required: `pnpm audit` passes in CI\n\
                   - Token cache lives in memory\n";
        let (resp, dec, rules) = classify_bullets(src);
        assert!(dec.iter().any(|d| d.contains("pgvector")));
        assert!(dec.iter().any(|d| d.contains("Rust")));
        assert!(rules.iter().any(|r| r.contains("secrets")));
        assert!(rules.iter().any(|r| r.contains("pnpm audit")));
        assert!(resp.iter().any(|r| r.contains("Token cache")));
    }

    #[test]
    fn analyze_end_to_end_for_readme() {
        let src = "# RunarForge\n\nPersistent memory for AI coding tools.\n\n## Decisions\n\n- Decided to ship as a single Rust binary\n- MUST validate MCP tool inputs\n";
        let m = analyze(src, "README.md");
        assert_eq!(m.role, MarkdownRole::Architecture);
        assert!(m.purpose.contains("Persistent memory"));
        assert!(m.headings.iter().any(|h| h.text == "Decisions"));
        assert!(!m.decisions.is_empty());
        assert!(!m.rules.is_empty());
    }

    #[test]
    fn into_deep_carries_decisions_into_business_rules() {
        let md = MarkdownAnalysis {
            role: MarkdownRole::Architecture,
            purpose: "p".into(),
            headings: vec![Heading {
                level: 1,
                text: "Top".into(),
            }],
            responsibilities: vec!["r".into()],
            decisions: vec!["d".into()],
            rules: vec!["must x".into()],
        };
        let deep = into_deep(md);
        assert_eq!(deep.purpose, "p");
        assert_eq!(deep.responsibilities, vec!["r".to_string()]);
        assert!(deep.business_rules.iter().any(|r| r.contains("must x")));
        assert!(deep
            .business_rules
            .iter()
            .any(|r| r.starts_with("decision: ")));
    }
}
