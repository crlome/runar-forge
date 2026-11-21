//! Passive `## Key Learnings:` section parser. Extracts bulleted / numbered
//! items from a markdown block so one tool call can fan out into many memory
//! entries.
//!
//! The `muninn_capture_passive` MCP tool calls into this module with raw
//! text (typically the end-of-turn message an agent would otherwise throw
//! away). We accept several header variants (`##`/`###`, with or without a
//! colon) and stop at the next same-or-higher-level heading so adjacent
//! sections stay separate.

// Offsets come from searches; cuts go through `str::get`, never `&s[a..b]`.
#![deny(clippy::string_slice)]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLearning {
    pub title: String,
    pub content: String,
    /// 16-char hex of a normalized-content hash; callers can use this as a
    /// stable `topic_key` to make repeated saves idempotent.
    pub dedup_hash: String,
}

const SECTION_HEADERS: &[&str] = &[
    "## key learnings",
    "### key learnings",
    "## learnings",
    "### learnings",
    "## takeaways",
    "### takeaways",
];

/// Parse `text`, find the first matching section header, and return each
/// bullet / numbered item inside as its own `ExtractedLearning`. Returns an
/// empty vec when no matching section is present or no items are found.
pub fn parse_key_learnings(text: &str) -> Vec<ExtractedLearning> {
    let section = match locate_section(text) {
        Some(s) => s,
        None => return Vec::new(),
    };

    let mut items: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for raw in section.lines() {
        let line = raw.trim_end();
        if let Some(stripped) = strip_bullet(line) {
            // New item — flush the previous.
            if !current.is_empty() {
                items.push(current.join("\n").trim().to_string());
                current.clear();
            }
            current.push(stripped.to_string());
        } else if !current.is_empty() {
            // Continuation of the current item (indented line or prose).
            let cont = line.trim_start();
            if !cont.is_empty() {
                current.push(cont.to_string());
            }
        }
    }
    if !current.is_empty() {
        items.push(current.join("\n").trim().to_string());
    }

    items
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(|content| {
            let title = derive_title(&content);
            let dedup_hash = short_hash(&normalize(&content));
            ExtractedLearning {
                title,
                content,
                dedup_hash,
            }
        })
        .collect()
}

// ── Internals ────────────────────────────────────────────────────

fn locate_section(text: &str) -> Option<&str> {
    // The headers are ASCII, and ASCII folding changes no byte length, so an
    // offset found in `lower` is a char boundary of `text`. Full Unicode
    // lowercasing does change lengths (U+0130, U+212A, ...), which shifted
    // the section start: learnings were lost, or the cut panicked.
    let lower = text.to_ascii_lowercase();
    // The first header variant that occurs at a line start wins. Every
    // occurrence is tried, so an inline mention earlier in a sentence
    // ("see ## Learnings below") does not hide the real header.
    let header_at = SECTION_HEADERS.iter().find_map(|header| {
        lower
            .match_indices(header)
            .map(|(at, _)| at)
            .find(|&at| at == 0 || lower.as_bytes().get(at - 1) == Some(&b'\n'))
    })?;
    let start = match text.get(header_at..)?.find('\n') {
        Some(newline) => header_at + newline + 1,
        None => text.len(),
    };
    let tail = text.get(start..)?;
    // Stop at the next same-or-higher-level heading so adjacent sections
    // don't bleed into each other.
    let end = tail
        .match_indices('\n')
        .map(|(i, _)| i)
        .find(|&i| {
            tail.get(i + 1..).is_some_and(|after| {
                after.starts_with("# ") || after.starts_with("## ") || after.starts_with("### ")
            })
        })
        .unwrap_or(tail.len());
    tail.get(..end)
}

fn strip_bullet(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    // Bullet: - foo | * foo | + foo
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return Some(rest);
    }
    // Numbered: 1. foo | 12) foo
    let chars = trimmed.char_indices();
    let mut digits = 0;
    for (i, c) in chars {
        if c.is_ascii_digit() {
            digits = i + c.len_utf8();
            continue;
        }
        if digits == 0 {
            return None;
        }
        if c == '.' || c == ')' {
            let rest = trimmed
                .get(digits + c.len_utf8()..)
                .and_then(|after_marker| after_marker.strip_prefix(' '));
            if rest.is_some() {
                return rest;
            }
        }
        break;
    }
    None
}

fn derive_title(content: &str) -> String {
    // First line up to ~100 chars, stripping markdown emphasis.
    let first = content.lines().next().unwrap_or("").trim();
    let cleaned = first
        .trim_start_matches(['*', '_', '`'])
        .trim_end_matches(['*', '_', '`']);
    if cleaned.chars().count() <= 100 {
        cleaned.to_string()
    } else {
        format!("{}…", crate::text::char_prefix(cleaned, 97))
    }
}

fn normalize(s: &str) -> String {
    // Lowercase + collapse whitespace so "Foo  bar" and "foo bar" dedupe.
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.to_lowercase().chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn short_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_section_returns_empty() {
        assert!(parse_key_learnings("just some prose with no header").is_empty());
    }

    #[test]
    fn bulleted_section() {
        let text = "\
preamble\n\
## Key Learnings:\n\
- First learning\n\
- Second learning with more detail\n\
- Third\n\
\n\
## Next Section\n\
- Not a learning\n";
        let out = parse_key_learnings(text);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].title, "First learning");
        assert_eq!(out[1].title, "Second learning with more detail");
        assert_eq!(out[2].title, "Third");
    }

    #[test]
    fn numbered_section() {
        let text = "\
## Learnings\n\
1. Alpha\n\
2. Beta\n\
3) Gamma\n";
        let out = parse_key_learnings(text);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].title, "Alpha");
        assert_eq!(out[2].title, "Gamma");
    }

    #[test]
    fn continuation_lines_fold_into_content() {
        let text = "\
## Key Learnings\n\
- Primary point\n\
  extra detail on the next line\n\
- Another one\n";
        let out = parse_key_learnings(text);
        assert_eq!(out.len(), 2);
        assert!(out[0].content.contains("extra detail"));
    }

    #[test]
    fn dedup_hash_is_stable_across_whitespace() {
        let a = parse_key_learnings("## Learnings\n- foo  bar\n");
        let b = parse_key_learnings("## Learnings\n- foo bar\n");
        assert_eq!(a[0].dedup_hash, b[0].dedup_hash);
    }

    #[test]
    fn case_insensitive_header() {
        let text = "## KEY LEARNINGS:\n- only one\n";
        let out = parse_key_learnings(text);
        assert_eq!(out.len(), 1);
    }

    fn titles(text: &str) -> Vec<String> {
        parse_key_learnings(text)
            .into_iter()
            .map(|l| l.title)
            .collect()
    }

    /// U+0130 lowercases one byte longer, so the section started one byte
    /// late and the first item lost its bullet.
    #[test]
    fn dotted_capital_i_before_the_header_keeps_the_first_learning() {
        assert_eq!(
            titles("\u{0130}zmir notes\n## Key Learnings\n- First\n- Second\n"),
            ["First", "Second"]
        );
    }

    /// Three U+0130 moved the cut inside the first CJK character: a panic
    /// that killed the MCP server.
    #[test]
    fn growing_chars_before_the_header_do_not_panic() {
        let text = format!("{}\n## Learnings\n- 配置 first\n", "\u{0130}".repeat(3));
        assert_eq!(titles(&text), ["配置 first"]);
    }

    /// Ten Kelvin signs (3 bytes each, 1 when lowercased) moved the cut 20
    /// bytes early, and the section came back empty.
    #[test]
    fn shrinking_chars_before_the_header_keep_the_section() {
        let text = format!("{}\n## Learnings\n- a\n", "\u{212A}".repeat(10));
        assert_eq!(titles(&text), ["a"]);
    }

    /// Only the first occurrence of each header was tried; an inline
    /// mention is not at a line start, so the real header was never found.
    #[test]
    fn inline_mention_before_the_real_header_is_skipped() {
        assert_eq!(
            titles("See ## Learnings below.\n## Learnings\n- real\n"),
            ["real"]
        );
    }

    #[test]
    fn learnings_survive_hostile_text_before_the_header() {
        for s in crate::test_support::hostile_corpus() {
            assert_eq!(
                titles(&format!("{s}\n## Key Learnings\n- first\n- second\n")),
                ["first", "second"],
                "after {:?}",
                crate::text::char_prefix(&s, 40)
            );
        }
    }

    #[test]
    fn title_truncates_long_lines() {
        let long = "a".repeat(200);
        let text = format!("## Learnings\n- {long}\n");
        let out = parse_key_learnings(&text);
        assert_eq!(out.len(), 1);
        assert!(out[0].title.len() <= 100);
        assert!(out[0].title.ends_with('…'));
    }
}
