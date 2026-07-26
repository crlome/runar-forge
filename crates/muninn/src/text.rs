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

    #[test]
    fn zero_budget_yields_only_the_ellipsis() {
        assert_eq!(truncate_ellipsis("anything", 0), "...");
        assert_eq!(truncate_ellipsis("", 0), "");
    }
}
