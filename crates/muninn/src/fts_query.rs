//! Turn a natural-language question into an FTS5 query expression.
//!
//! FTS5 gives a bare string implicit **AND** semantics and has no stopword
//! list, so "how does authentication work here" only matches documents that
//! contain *every* one of those words — including "how", "does" and "here".
//! Punctuation is worse than useless: `(`, `)`, `:`, `"`, `*` and `-` are
//! query syntax, so a question mark or a parenthesised aside can turn a
//! search into a hard `fts5: syntax error`.
//!
//! The 2026-07-26 audit measured the result: 15 of 16 realistic developer
//! questions returned zero rows, while the same questions rewritten as
//! keyword/OR queries returned hits for all 16. This module is that rewrite.

/// Function words and interrogatives that carry no retrieval signal. Kept
/// deliberately tight — anything that could plausibly be a domain term
/// ("state", "type", "index", "key") stays in.
const STOPWORDS: &[&str] = &[
    "a", "about", "after", "all", "also", "am", "an", "and", "any", "are", "as", "at", "back",
    "be", "because", "been", "before", "being", "but", "by", "can", "could", "did", "do", "does",
    "doing", "done", "for", "from", "had", "has", "have", "having", "he", "her", "here", "him",
    "his", "how", "i", "if", "in", "into", "is", "it", "its", "just", "let", "like", "may", "me",
    "might", "much", "must", "my", "need", "no", "not", "now", "of", "off", "on", "once", "one",
    "only", "or", "our", "out", "over", "please", "put", "said", "same", "see", "shall", "she",
    "should", "so", "some", "still", "such", "sure", "tell", "than", "that", "the", "their",
    "them", "then", "there", "these", "they", "thing", "things", "this", "those", "through", "to",
    "too", "under", "up", "us", "very", "want", "was", "way", "we", "were", "what", "when",
    "where", "which", "while", "who", "whom", "why", "will", "with", "would", "yes", "you", "your",
];

/// Upper bound on OR terms. Long pastes otherwise produce enormous queries
/// that match nearly everything and rank on noise.
const MAX_TERMS: usize = 12;

/// Shortest token worth searching for. One-character tokens are almost always
/// punctuation residue or list markers.
const MIN_TERM_CHARS: usize = 2;

/// Build an FTS5 expression from free text.
///
/// Explicit `"quoted phrases"` are preserved as phrase terms — that is the
/// escape hatch when someone really does want exact matching. Everything else
/// is tokenized, stripped of stopwords, and OR-joined so partial matches
/// surface and bm25 `rank` orders them.
///
/// Returns `None` when nothing searchable survives, which the caller should
/// treat as "no FTS results" rather than passing the raw string through.
pub fn build(input: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    // Pull out "quoted phrases" first so their inner spaces survive.
    let mut rest = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            rest.push(c);
            continue;
        }
        let phrase: String = chars.by_ref().take_while(|c| *c != '"').collect();
        let cleaned = sanitize_phrase(&phrase);
        if cleaned.split_whitespace().count() > 1 {
            terms.push(format!("\"{cleaned}\""));
        } else if !cleaned.is_empty() {
            rest.push(' ');
            rest.push_str(&cleaned);
        }
    }

    for raw in rest.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if terms.len() >= MAX_TERMS {
            break;
        }
        let token = raw.trim().to_lowercase();
        if token.chars().count() < MIN_TERM_CHARS
            || STOPWORDS.contains(&token.as_str())
            || seen.contains(&token)
        {
            continue;
        }
        seen.push(token.clone());
        terms.push(token);
    }

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

/// Strip everything FTS5 would read as syntax from inside a phrase.
fn sanitize_phrase(phrase: &str) -> String {
    phrase
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_stopwords_and_or_joins_the_rest() {
        // The exact shape that returned 0 rows for 15 of 16 audit questions.
        assert_eq!(
            build("how does authentication work here"),
            Some("authentication OR work".into())
        );
    }

    #[test]
    fn punctuation_never_reaches_fts5_as_syntax() {
        // Each of these is a syntax error or a silent mis-parse when passed raw.
        for q in [
            "what about the schema?",
            "auth (jwt) flow",
            "user-facing errors",
            "column: created_at",
            "search * everything",
            "why did it fail!!",
        ] {
            let out = build(q).expect("something searchable survives");
            assert!(
                !out.contains(['(', ')', ':', '*', '?', '!', '-']),
                "syntax leaked from {q:?}: {out}"
            );
        }
    }

    #[test]
    fn hyphenated_words_become_separate_terms() {
        assert_eq!(
            build("user-facing errors"),
            Some("user OR facing OR errors".into())
        );
    }

    #[test]
    fn explicit_quoted_phrases_are_preserved() {
        let out = build("where is \"connection pool\" configured").unwrap();
        assert!(out.contains("\"connection pool\""), "{out}");
        assert!(out.contains("configured"), "{out}");
    }

    #[test]
    fn single_word_quotes_degrade_to_a_plain_term() {
        assert_eq!(build("\"pgvector\""), Some("pgvector".into()));
    }

    #[test]
    fn duplicate_terms_appear_once() {
        assert_eq!(build("cache the cache CACHE"), Some("cache".into()));
    }

    #[test]
    fn long_input_is_capped() {
        let words: Vec<String> = (0..40).map(|i| format!("term{i}")).collect();
        let out = build(&words.join(" ")).unwrap();
        assert_eq!(out.split(" OR ").count(), MAX_TERMS);
    }

    #[test]
    fn nothing_searchable_yields_none() {
        assert_eq!(build(""), None);
        assert_eq!(build("   ?? !! ---   "), None);
        assert_eq!(build("how is it that we should do this"), None);
    }

    #[test]
    fn domain_terms_that_look_like_stopwords_survive() {
        // These are real column/concept names in this codebase.
        for term in ["state", "type", "index", "key", "value", "count"] {
            assert_eq!(build(term), Some(term.to_string()));
        }
    }

    #[test]
    fn non_ascii_input_is_tokenized_not_dropped() {
        assert_eq!(
            build("¿dónde está la configuración?"),
            Some("dónde OR está OR la OR configuración".into())
        );
    }
}
