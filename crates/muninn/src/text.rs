//! Small text helpers shared by the hook, CLI, and MCP paths.
//!
//! These exist because `&s[..n]` panics when `n` lands inside a multibyte
//! character. That is not a theoretical concern: a single non-ASCII entry
//! whose cut point fell mid-character permanently killed the PreToolUse
//! context hook for three projects, because the context packet is a fixed
//! recency window that kept re-including the same row on every fire.

/// Char-boundary-safe prefix. Never panics, whatever `s` contains.
pub fn char_prefix(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Char-boundary-safe prefix under a **byte** budget.
///
/// Use this when the budget is genuinely about bytes — an IO read cap, a
/// column width, a wire limit — rather than a display length. Truncating to a
/// char count instead would change how much is read for non-ASCII input.
///
/// `String::truncate(n)` and `&s[..n]` both panic when `n` lands inside a
/// multibyte character. This floors to the nearest boundary at or below the
/// budget, so it always returns at most `max_bytes` and never panics.
pub fn byte_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Preview helper: at most `max_chars` characters, with `...` appended only
/// when something was actually cut.
///
/// Prefer this over `if s.len() > max { &s[..max] }` — that pattern compares a
/// *byte* length against a *char* budget and panics on multibyte input.
pub fn truncate_ellipsis(s: &str, max_chars: usize) -> String {
    let prefix = char_prefix(s, max_chars);
    if prefix.len() == s.len() {
        s.to_string()
    } else {
        format!("{prefix}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_shorter_than_budget_is_unchanged() {
        assert_eq!(truncate_ellipsis("hello", 200), "hello");
        assert_eq!(char_prefix("hello", 200), "hello");
    }

    #[test]
    fn ascii_longer_than_budget_is_cut() {
        assert_eq!(truncate_ellipsis("abcdef", 3), "abc...");
    }

    #[test]
    fn cut_inside_a_multibyte_char_does_not_panic() {
        // Every one of these used to panic under `&s[..n]`.
        let cases = [
            "café serves crème brûlée",         // Latin-1 accents
            "配置ファイルを読み込む",           // CJK, 3 bytes/char
            "🐦‍⬛ raven emoji with ZWJ sequence", // 4-byte + joiner
            "Здравствуйте, мир",                // Cyrillic
        ];
        for s in cases {
            for n in 0..s.chars().count() + 2 {
                let out = truncate_ellipsis(s, n);
                assert!(
                    out.starts_with(char_prefix(s, n)),
                    "bad prefix for {s:?}@{n}"
                );
            }
        }
    }

    #[test]
    fn budget_counts_chars_not_bytes() {
        // 5 chars, 15 bytes — a byte-length guard would wrongly truncate this.
        let s = "配置ファイル";
        assert_eq!(truncate_ellipsis(s, 10), s);
        assert_eq!(truncate_ellipsis(s, 2), "配置...");
    }

    /// The shape that crashed `runar crawl`: a byte budget landing inside a
    /// multibyte character. `String::truncate(8192)` / `&s[..8192]` both panic
    /// there; this must not.
    #[test]
    fn byte_budget_inside_a_multibyte_char_floors_to_the_boundary() {
        // U+2500 (3 bytes) — the `─` from this repo's `// ── Section ──`
        // dividers, which is the real-world trigger.
        let s = "a".repeat(8191) + &"─".repeat(20);
        assert!(
            !s.is_char_boundary(8192),
            "fixture must straddle the budget"
        );

        let out = byte_prefix(&s, 8192);
        assert_eq!(out.len(), 8191, "floors DOWN to the boundary");
        assert!(out.len() <= 8192, "never exceeds the budget");
        assert!(s.starts_with(out));

        // Every offset across a multibyte run must be safe, not just this one.
        for n in 0..s.len() + 4 {
            let cut = byte_prefix(&s, n);
            assert!(cut.len() <= n.min(s.len()));
            assert!(s.starts_with(cut));
        }
    }

    #[test]
    fn byte_prefix_is_a_byte_budget_not_a_char_budget() {
        let cjk = "配置ファイル"; // 6 chars, 18 bytes
        assert_eq!(byte_prefix(cjk, 100), cjk, "under budget is untouched");
        assert_eq!(byte_prefix(cjk, 18), cjk, "exactly at budget is untouched");
        // 7 bytes = 2 whole chars (6 bytes) + 1 stray byte -> floors to 6.
        assert_eq!(byte_prefix(cjk, 7), "配置");
        assert_eq!(byte_prefix(cjk, 2), "", "no whole char fits");
        assert_eq!(byte_prefix("", 0), "");
    }

    #[test]
    fn zero_budget_yields_only_the_ellipsis() {
        assert_eq!(truncate_ellipsis("anything", 0), "...");
        assert_eq!(truncate_ellipsis("", 0), "");
    }
}
