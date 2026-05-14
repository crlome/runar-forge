//! Cheap token-count estimator. ~4 characters per token is Anthropic's
//! published rule of thumb for English prose; accurate enough for
//! agent-side budgeting decisions without pulling in a real tokenizer.
//!
//! Exposed via `muninn_search` / `muninn_get` responses as
//! `tokensEstimate` so the caller can decide whether to `get` the full
//! entry or stay with the compact snippet.

/// Estimate the token count of `text`. Returns 0 for empty input,
/// otherwise `max(1, ceil(chars / 4))`.
pub fn estimate(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 {
        return 0;
    }
    // Round up so a 3-char string still reports ≥1 token.
    chars.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(estimate(""), 0);
    }

    #[test]
    fn short_strings_round_up() {
        assert_eq!(estimate("a"), 1);
        assert_eq!(estimate("ab"), 1);
        assert_eq!(estimate("abc"), 1);
        assert_eq!(estimate("abcd"), 1);
        assert_eq!(estimate("abcde"), 2);
    }

    #[test]
    fn long_string() {
        // 400 chars → 100 tokens.
        let s: String = "x".repeat(400);
        assert_eq!(estimate(&s), 100);
    }

    #[test]
    fn counts_unicode_codepoints_not_bytes() {
        // "héllo" — 5 codepoints, not 6 bytes.
        assert_eq!(estimate("héllo"), 2);
    }
}
