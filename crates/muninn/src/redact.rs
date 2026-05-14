//! Strip `<private>...</private>` markers from text before memory entries
//! are persisted. Inspired by claude-mem's README-documented redaction;
//! useful when a user pastes a secret or sensitive detail in chat and
//! doesn't want it copied into memory.
//!
//! This is **not a security boundary** — treat it as best-effort hygiene.
//! A motivated attacker with MCP access can still cause unredacted saves.
//! Docs should be explicit about this.

/// Redact content between `<private>...</private>` tags. Case-insensitive on
/// the tag names, supports multiple blocks, and tolerates unclosed tags by
/// redacting through end-of-input. Returns the stripped string and how many
/// blocks were redacted.
pub fn strip_private(input: &str) -> (String, usize) {
    const OPEN: &str = "<private>";
    const CLOSE: &str = "</private>";
    const MARKER: &str = "[redacted]";

    // Short-circuit when no opening marker exists (case-insensitive scan).
    if !has_ci(input, OPEN) {
        return (input.to_string(), 0);
    }

    let lower = input.to_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0usize;
    let mut count = 0usize;

    while cursor < input.len() {
        match lower[cursor..].find(OPEN) {
            Some(rel_open) => {
                let open_at = cursor + rel_open;
                // Emit everything before the opening tag.
                out.push_str(&input[cursor..open_at]);
                // Find a matching closer; if none, redact through EOF.
                let after_open = open_at + OPEN.len();
                match lower[after_open..].find(CLOSE) {
                    Some(rel_close) => {
                        out.push_str(MARKER);
                        count += 1;
                        cursor = after_open + rel_close + CLOSE.len();
                    }
                    None => {
                        // Unterminated — treat the rest of the input as private.
                        out.push_str(MARKER);
                        count += 1;
                        cursor = input.len();
                    }
                }
            }
            None => {
                out.push_str(&input[cursor..]);
                break;
            }
        }
    }

    (out, count)
}

fn has_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tags_returns_input_unchanged() {
        let (out, n) = strip_private("plain content");
        assert_eq!(out, "plain content");
        assert_eq!(n, 0);
    }

    #[test]
    fn single_block_redacted() {
        let (out, n) = strip_private("before <private>secret</private> after");
        assert_eq!(out, "before [redacted] after");
        assert_eq!(n, 1);
    }

    #[test]
    fn multiple_blocks_redacted() {
        let (out, n) = strip_private("<private>a</private> middle <private>b</private>");
        assert_eq!(out, "[redacted] middle [redacted]");
        assert_eq!(n, 2);
    }

    #[test]
    fn case_insensitive_tags() {
        let (out, n) = strip_private("x <Private>shh</PRIVATE> y");
        assert_eq!(out, "x [redacted] y");
        assert_eq!(n, 1);
    }

    #[test]
    fn unterminated_tag_redacts_through_eof() {
        let (out, n) = strip_private("leak <private>still open forever");
        assert_eq!(out, "leak [redacted]");
        assert_eq!(n, 1);
    }

    #[test]
    fn nested_tags_outer_only() {
        // We don't attempt to handle nesting — first </private> wins.
        // Inner opener becomes literal content of the outer block.
        let (out, n) = strip_private("<private>outer <private>inner</private> tail</private>");
        assert_eq!(out, "[redacted] tail</private>");
        assert_eq!(n, 1);
    }

    #[test]
    fn preserves_non_private_markup() {
        let (out, n) = strip_private("use <code>foo</code> carefully");
        assert_eq!(out, "use <code>foo</code> carefully");
        assert_eq!(n, 0);
    }
}
