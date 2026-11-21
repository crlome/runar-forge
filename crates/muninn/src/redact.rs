//! Strip `<private>...</private>` markers from text before memory entries
//! are persisted. Inspired by claude-mem's README-documented redaction;
//! useful when a user pastes a secret or sensitive detail in chat and
//! doesn't want it copied into memory.
//!
//! This is **not a security boundary** — treat it as best-effort hygiene.
//! A motivated attacker with MCP access can still cause unredacted saves.
//! Docs should be explicit about this.

// Offsets come from searches; cuts go through `str::get`, never `&s[a..b]`.
#![deny(clippy::string_slice)]

/// Redact content between `<private>...</private>` tags. Tag names match
/// case-insensitively in ASCII, multiple blocks are supported, the first
/// closing tag ends a block, and an unclosed tag redacts through
/// end-of-input. Returns the stripped string and how many blocks were
/// redacted.
pub fn strip_private(input: &str) -> (String, usize) {
    strip_blocks(input, "[redacted]")
}

/// [`strip_private`] for text used as a search query. Each block becomes a
/// space, so the query never searches for the word "redacted".
pub fn strip_private_for_query(input: &str) -> String {
    strip_blocks(input, " ").0
}

/// Replace each private block with `marker`.
///
/// Tags are found in an ASCII-lowercased copy. ASCII folding never changes
/// a byte length and the tags are ASCII, so every offset found in the copy
/// is a char boundary of `input`. Full Unicode lowercasing does change
/// lengths (U+0130 grows from 2 bytes to 3, U+212A shrinks from 3 to 1),
/// and cutting `input` at offsets taken from such a copy kept private text,
/// dropped public text, or panicked mid-character. Cuts still go through
/// `str::get`: if one were ever invalid, the rest is redacted (fail closed)
/// rather than kept or panicked on.
fn strip_blocks(input: &str, marker: &str) -> (String, usize) {
    const OPEN: &str = "<private>";
    const CLOSE: &str = "</private>";

    let folded = input.to_ascii_lowercase();
    if !folded.contains(OPEN) {
        return (input.to_string(), 0);
    }

    let mut out = String::with_capacity(input.len());
    let mut count = 0usize;
    let mut cursor = 0usize;
    while let Some(rel_open) = folded.get(cursor..).and_then(|rest| rest.find(OPEN)) {
        let open_at = cursor + rel_open;
        let Some(before) = input.get(cursor..open_at) else {
            out.push_str(marker);
            return (out, count + 1);
        };
        out.push_str(before);
        out.push_str(marker);
        count += 1;
        let after_open = open_at + OPEN.len();
        match folded.get(after_open..).and_then(|rest| rest.find(CLOSE)) {
            Some(rel_close) => cursor = after_open + rel_close + CLOSE.len(),
            // Unterminated: the rest of the input is private.
            None => return (out, count),
        }
    }
    match input.get(cursor..) {
        Some(rest) => out.push_str(rest),
        None => {
            out.push_str(marker);
            count += 1;
        }
    }
    (out, count)
}

use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// One redaction outcome: which pattern kind fired and how many times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretHit {
    pub kind: &'static str,
    pub count: usize,
}

/// Whole-match patterns: the entire match is a secret. PEM first so a key
/// block is consumed before smaller patterns can nibble at its body.
static WHOLE_MATCH: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "pem-key",
            Regex::new(
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
            )
            .unwrap(),
        ),
        (
            "github-token",
            Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36,255}\b").unwrap(),
        ),
        (
            "github-token",
            Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{22,255}\b").unwrap(),
        ),
        (
            "aws-key-id",
            Regex::new(r"\b(?:AKIA|ASIA|ABIA|ACCA)[A-Z0-9]{16}\b").unwrap(),
        ),
        (
            "atlassian-token",
            Regex::new(r"\bATATT[A-Za-z0-9_\-=]{20,}\b").unwrap(),
        ),
        (
            "slack-token",
            Regex::new(r"\bxox[abpos]-[A-Za-z0-9-]{10,}\b").unwrap(),
        ),
        (
            "jwt",
            Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
                .unwrap(),
        ),
        // Vendor-prefixed keys. Each of these is self-identifying, so it is
        // recognisable on its own and does not need to sit right of an
        // `API_KEY=` the way an unprefixed value does. For a memory system
        // living inside Claude Code, `sk-ant-` is the likeliest credential of
        // all and was previously caught only when assigned to a named key.
        (
            "anthropic-key",
            Regex::new(r"\bsk-ant-[A-Za-z0-9_-]{16,}").unwrap(),
        ),
        (
            "openai-key",
            Regex::new(r"\bsk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{16,}").unwrap(),
        ),
        // Legacy unprefixed OpenAI keys. The charset excludes `-`, so this
        // cannot swallow any of the `sk-<vendor>-` forms above.
        (
            "openai-key",
            Regex::new(r"\bsk-[A-Za-z0-9]{32,}\b").unwrap(),
        ),
        (
            "google-api-key",
            Regex::new(r"\bAIza[0-9A-Za-z_-]{35}").unwrap(),
        ),
        (
            "gitlab-token",
            Regex::new(r"\bglpat-[0-9A-Za-z_-]{20,}").unwrap(),
        ),
        (
            "sendgrid-key",
            Regex::new(r"\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}").unwrap(),
        ),
        ("npm-token", Regex::new(r"\bnpm_[A-Za-z0-9]{36}\b").unwrap()),
        (
            "slack-app-token",
            Regex::new(r"\bxapp-[0-9]-[A-Za-z0-9_-]{10,}").unwrap(),
        ),
        // The path segment is the credential: anyone holding the URL can post.
        (
            "slack-webhook",
            Regex::new(r"https://hooks\.slack\.com/services/[A-Za-z0-9_/-]{10,}").unwrap(),
        ),
        (
            "stripe-key",
            Regex::new(r"\b[rs]k_(?:live|test)_[A-Za-z0-9]{16,}\b").unwrap(),
        ),
        (
            "huggingface-token",
            Regex::new(r"\bhf_[A-Za-z0-9]{30,}\b").unwrap(),
        ),
    ]
});

/// `scheme://user:password@host` — only the password group is a secret.
static URL_CREDENTIALS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"://([^/\s:@]{1,64}):([^@/\s]{1,256})@").unwrap());

/// `Authorization: Bearer <token>` — keep the scheme word, redact the token.
static BEARER_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(bearer\s+)[A-Za-z0-9._\-+/=]{16,}\b").unwrap());

/// `SOME_PASSWORD = value` style assignments — keep the key, redact the value.
static KEYED_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([A-Z0-9_]*(?:password|passwd|secret|token|api[_-]?key|_pass|_key)["']?\s*[:=]\s*["']?)([^\s"']{8,})"#,
    )
    .unwrap()
});

/// Short git SHAs (7–12 hex) or a full 40-hex SHA. Deliberately NOT any
/// hex run: 16/32-hex strings are a common API-key shape and must stay
/// redactable.
static GIT_SHA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[0-9a-fA-F]{7,12}|[0-9a-fA-F]{40})$").unwrap());

/// Path-shaped values: multiple '/' separators AND only the quiet
/// lowercase charset paths use. A 2+-slash base64 blob (AWS secret keys
/// often contain slashes) has uppercase/'+'/'=' and fails this.
static PATHLIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9._~/-]+$").unwrap());
static UUID_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
        .unwrap()
});

/// Values that look like references or placeholders rather than secrets.
///
/// A bare UUID stays allowlisted deliberately. UUIDs are overwhelmingly
/// identifiers — session ids, row ids, trace ids — and this predicate only runs
/// on values already sitting right of a secret-ish key name, which is exactly
/// where `session_token=<uuid>` appears and means "which session", not "the
/// bearer". Redacting them would strip a large amount of legitimately useful
/// context to catch a credential format nobody issues.
fn value_allowlisted(value: &str) -> bool {
    GIT_SHA.is_match(value)
        || UUID_VALUE.is_match(value)
        || value.starts_with('$') // env interpolation: $VAR / ${VAR}
        // A `(` means the value is code. Requiring a whole well-formed call
        // instead was tried and reverted: measured against a 17k-entry corpus
        // it redacted 16 code fragments — `z.string(),`, `Some(format!(`,
        // `` `${prefix}` ``, calls captured mid-argument — and zero secrets.
        // The hole it closed (`api_key=secret(x`) is not a real credential
        // shape: every format in WHOLE_MATCH is drawn from base64, hex or
        // url-safe base64, none of which contain a parenthesis.
        || value.contains('(')
        || (value.starts_with('<') && value.ends_with('>')) // <your-token>
        || value.starts_with("[REDACTED") // already redacted by an earlier pattern
        || (value.matches('/').count() >= 2 && PATHLIKE.is_match(value)) // keys/deploy/id
}

/// Shannon entropy in bits per character.
fn shannon_entropy(s: &str) -> f64 {
    let mut freq: BTreeMap<char, usize> = BTreeMap::new();
    let mut len = 0usize;
    for c in s.chars() {
        *freq.entry(c).or_insert(0) += 1;
        len += 1;
    }
    if len == 0 {
        return 0.0;
    }
    freq.values()
        .map(|&n| {
            let p = n as f64 / len as f64;
            -p * p.log2()
        })
        .sum()
}

/// Should a bare keyed-secret value be redacted? Short values are redacted
/// outright (the key name already marks them as secrets); long values must
/// also look random so prose or paths assigned to a "*_key" variable survive.
fn should_redact_value(value: &str) -> bool {
    if value_allowlisted(value) {
        return false;
    }
    if value.chars().count() < 20 {
        return true;
    }
    shannon_entropy(value) > 3.0
}

/// Pattern-based secret redaction. Each hit is replaced with
/// `[REDACTED:<kind>]`; surrounding text is preserved so entries stay useful.
/// Like `strip_private`, this is best-effort hygiene, not a security boundary.
pub fn redact_secrets(input: &str) -> (String, Vec<SecretHit>) {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut text = input.to_string();

    for (kind, re) in WHOLE_MATCH.iter() {
        if re.is_match(&text) {
            let mut n = 0usize;
            text = re
                .replace_all(&text, |_: &regex::Captures| {
                    n += 1;
                    format!("[REDACTED:{kind}]")
                })
                .into_owned();
            *counts.entry(kind).or_insert(0) += n;
        }
    }

    if URL_CREDENTIALS.is_match(&text) {
        let mut n = 0usize;
        text = URL_CREDENTIALS
            .replace_all(&text, |caps: &regex::Captures| {
                // The marker this rule writes is itself a legal password as far
                // as the pattern is concerned, so without this the rule matches
                // its own output forever: `runar scrub` re-reported and
                // rewrote the same rows on every run and the count could never
                // reach zero. The keyed-secret branch has always had the same
                // guard via `value_allowlisted`.
                if caps[2].starts_with("[REDACTED") {
                    return caps[0].to_string();
                }
                n += 1;
                format!("://{}:[REDACTED:url-credentials]@", &caps[1])
            })
            .into_owned();
        if n > 0 {
            *counts.entry("url-credentials").or_insert(0) += n;
        }
    }

    if BEARER_HEADER.is_match(&text) {
        let mut n = 0usize;
        text = BEARER_HEADER
            .replace_all(&text, |caps: &regex::Captures| {
                n += 1;
                format!("{}[REDACTED:bearer-header]", &caps[1])
            })
            .into_owned();
        *counts.entry("bearer-header").or_insert(0) += n;
    }

    if KEYED_SECRET.is_match(&text) {
        let mut n = 0usize;
        text = KEYED_SECRET
            .replace_all(&text, |caps: &regex::Captures| {
                if should_redact_value(&caps[2]) {
                    n += 1;
                    format!("{}[REDACTED:keyed-secret]", &caps[1])
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();
        if n > 0 {
            *counts.entry("keyed-secret").or_insert(0) += n;
        }
    }

    let hits = counts
        .into_iter()
        .map(|(kind, count)| SecretHit { kind, count })
        .collect();
    (text, hits)
}

/// Redact in place, returning the number of hits.
///
/// Exists so the write paths that cannot go through `propose` — session
/// creation, the session-summary entry, a caller-supplied topic key — scrub
/// with the same call rather than each re-deriving the pattern. Every one of
/// those was previously unscrubbed.
pub fn scrub(text: &mut String) -> usize {
    let (clean, hits) = redact_secrets(text);
    let total: usize = hits.iter().map(|h| h.count).sum();
    if total > 0 {
        *text = clean;
    }
    total
}

/// Elide the input from a panic message. On Rust 1.94 (not on 1.100),
/// std's message for a failed str slice quotes up to 256 bytes of the
/// string being cut, after ` of `` or ` when slicing ``; the message is cut
/// there. Every message is capped at 300 chars. No regex, because this runs
/// inside the panic hook.
pub fn elide_panic_input(msg: &str) -> String {
    for subject in [" of `", " when slicing `"] {
        if let Some(head) = msg
            .find(subject)
            .and_then(|at| msg.get(..at + subject.len()))
        {
            return crate::text::truncate_ellipsis(&format!("{head}<input elided>`"), 300);
        }
    }
    crate::text::truncate_ellipsis(msg, 300)
}

/// [`strip_private`] in place, then [`scrub`]: the order `propose` uses, so
/// a secret inside a private block goes with the block. For write paths that
/// bypass `propose` (session goals, summaries, discoveries). Returns private
/// blocks plus secret hits. A second pass changes nothing.
pub fn scrub_all(text: &mut String) -> usize {
    let (stripped, blocks) = strip_private(text);
    if blocks > 0 {
        *text = stripped;
    }
    blocks + scrub(text)
}

/// Recursively redact every string leaf of a JSON value (hook payloads carry
/// tool input/output as nested JSON). Returns the total number of hits.
pub fn redact_secrets_value(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => {
            let (redacted, hits) = redact_secrets(s);
            let total: usize = hits.iter().map(|h| h.count).sum();
            if total > 0 {
                *s = redacted;
            }
            total
        }
        serde_json::Value::Array(items) => items.iter_mut().map(redact_secrets_value).sum(),
        serde_json::Value::Object(map) => map.values_mut().map(redact_secrets_value).sum(),
        _ => 0,
    }
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

    const DOTTED_I: &str = "\u{0130}"; // 2 bytes; lowercases to 3
    const KELVIN: &str = "\u{212A}"; // 3 bytes; lowercases to 1

    /// Offsets taken from a Unicode-lowercased copy were 17 bytes late
    /// here: the cut kept `<private>pin 4821` and dropped ` tail`.
    #[test]
    fn strip_private_keeps_offsets_after_length_growing_chars() {
        let prefix = DOTTED_I.repeat(17);
        let (out, n) = strip_private(&format!("{prefix} <private>pin 4821</private> tail"));
        assert_eq!(out, format!("{prefix} [redacted] tail"));
        assert_eq!(n, 1);
        assert!(!out.contains("4821"));
    }

    /// The open offset (4) came from a copy in which the Kelvin sign was one
    /// byte, so the cut landed inside it and panicked.
    #[test]
    fn strip_private_does_not_panic_on_a_shrinking_char_before_the_tag() {
        let (out, n) = strip_private(&format!("300{KELVIN}<private>hunter2</private> ok"));
        assert_eq!(out, format!("300{KELVIN}[redacted] ok"));
        assert_eq!(n, 1);
    }

    /// Six Kelvin signs inside the block made the close offset 12 bytes
    /// short, leaving `21</private>` in the output.
    #[test]
    fn strip_private_close_cut_ignores_case_changes_inside_the_span() {
        let kelvins = KELVIN.repeat(6);
        let (out, n) = strip_private(&format!(
            "<private>temp 300{kelvins} pin=4821</private> done"
        ));
        assert_eq!(out, "[redacted] done");
        assert_eq!(n, 1);
    }

    /// Both private payloads were kept and the public text between and
    /// after them dropped.
    #[test]
    fn strip_private_handles_every_block_after_shifting_chars() {
        let prefix = DOTTED_I.repeat(12);
        let (out, n) = strip_private(&format!(
            "{prefix}<private>aaa</private> mid <private>bbb</private> end"
        ));
        assert_eq!(out, format!("{prefix}[redacted] mid [redacted] end"));
        assert_eq!(n, 2);
    }

    #[test]
    fn strip_private_holds_on_the_hostile_corpus() {
        for s in crate::test_support::hostile_corpus() {
            if s.contains('<') {
                continue; // tag fragments: wrapping a block around them changes the meaning
            }
            let label = crate::text::char_prefix(&s, 40);
            for (open, close) in [("<private>", "</private>"), ("<PRIVATE>", "</Private>")] {
                let (out, n) = strip_private(&format!("{s}{open}SENTINEL{s}{close}{s}"));
                assert_eq!(out, format!("{s}[redacted]{s}"), "around {label:?}");
                assert_eq!(n, 1, "around {label:?}");
            }
            let (out, n) = strip_private(&format!("{s}<private>SENTINEL{s}"));
            assert_eq!(
                out,
                format!("{s}[redacted]"),
                "unterminated after {label:?}"
            );
            assert_eq!(n, 1);
        }
    }

    #[test]
    fn strip_private_on_seeded_mixes_never_emits_a_tag() {
        let mixes = crate::test_support::hostile_mixes(2000, 0x5EED_1234);

        // The mixer must keep producing the shapes that broke the old code,
        // or a seed or mixer change would quietly remove this test's teeth.
        let atoms_before_tag = |s: &str, atom: char| {
            s.to_ascii_lowercase()
                .find("<private>")
                .and_then(|at| s.get(..at))
                .map_or(0, |before| before.chars().filter(|c| *c == atom).count())
        };
        assert!(mixes.iter().any(|s| atoms_before_tag(s, '\u{0130}') >= 10));
        assert!(mixes.iter().any(|s| atoms_before_tag(s, '\u{212A}') >= 1));

        for s in &mixes {
            let (out, n) = strip_private(s);
            assert!(
                !out.to_ascii_lowercase().contains("<private>"),
                "tag emitted for {s:?}"
            );
            if !s.to_ascii_lowercase().contains("<private>") {
                assert_eq!((out.as_str(), n), (s.as_str(), 0));
            }
        }
    }

    fn total_hits(hits: &[SecretHit]) -> usize {
        hits.iter().map(|h| h.count).sum()
    }

    #[test]
    fn redact_secrets_leaves_hostile_text_alone_and_still_catches_secrets() {
        // Assembled at runtime so no secret-shaped literal sits in the source.
        let fake = ["Zq8x", "7Lm2", "Pw9R", "t4Kd"].concat();
        for s in crate::test_support::hostile_corpus() {
            let label = crate::text::char_prefix(&s, 40);
            let (out, hits) = redact_secrets(&s);
            assert_eq!(out, s, "clean text changed: {label:?}");
            assert_eq!(total_hits(&hits), 0, "false hit on {label:?}");

            let (out, _) = redact_secrets(&format!("{s} DB_PASSWORD={fake} {s}"));
            assert!(!out.contains(&fake), "secret survived next to {label:?}");
        }
    }

    #[test]
    fn redacts_github_tokens() {
        let (out, hits) = redact_secrets("push with ghp_abcdefghijklmnopqrstuvwxyz0123456789 now");
        assert!(out.contains("[REDACTED:github-token]"), "{out}");
        assert!(!out.contains("ghp_abcdef"));
        assert_eq!(total_hits(&hits), 1);

        let (out, _) = redact_secrets("github_pat_11ABCDEFG0abcdefghijklmnop_qrstuv");
        assert!(out.contains("[REDACTED:github-token]"), "{out}");
    }

    #[test]
    fn redacts_aws_atlassian_slack() {
        let (out, _) = redact_secrets("key AKIAIOSFODNN7EXAMPLE used");
        assert!(out.contains("[REDACTED:aws-key-id]"), "{out}");

        let (out, _) = redact_secrets("ATATT3xFfGF0abcdefghijklmnopqrstuvwx-yz=");
        assert!(out.contains("[REDACTED:atlassian-token]"), "{out}");

        let (out, _) = redact_secrets("xoxb-123456789012-abcdefghij");
        assert!(out.contains("[REDACTED:slack-token]"), "{out}");
    }

    #[test]
    fn redacts_jwt_but_not_fragment() {
        let jwt =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9P";
        let (out, _) = redact_secrets(&format!("token: {jwt}"));
        assert!(out.contains("[REDACTED:jwt]"), "{out}");

        // A bare "eyJhb" fragment is not a JWT.
        let (out, hits) = redact_secrets("the prefix eyJhb marks base64 JSON");
        assert!(out.contains("eyJhb"));
        assert_eq!(total_hits(&hits), 0);
    }

    #[test]
    fn redacts_pem_block_including_unterminated() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow...\n-----END RSA PRIVATE KEY-----";
        let (out, _) = redact_secrets(pem);
        assert_eq!(out, "[REDACTED:pem-key]");

        let (out, _) = redact_secrets("-----BEGIN PRIVATE KEY-----\ntruncated paste");
        assert_eq!(out, "[REDACTED:pem-key]");
    }

    #[test]
    fn redacts_url_password_keeps_username() {
        let (out, _) = redact_secrets("postgres://muninn:hunter2secret@10.0.0.5:5432/db");
        assert!(
            out.contains("://muninn:[REDACTED:url-credentials]@10.0.0.5"),
            "{out}"
        );
    }

    #[test]
    fn redacts_bearer_header() {
        let (out, _) = redact_secrets("Authorization: Bearer abcdef0123456789ABCDEF");
        assert!(out.contains("Bearer [REDACTED:bearer-header]"), "{out}");
    }

    #[test]
    fn redacts_keyed_assignments() {
        let (out, _) = redact_secrets("TENANT_DB_PASS=Sup3rS3cret99 host=10.0.0.5");
        assert!(
            out.contains("TENANT_DB_PASS=[REDACTED:keyed-secret]"),
            "{out}"
        );
        assert!(out.contains("host=10.0.0.5"));

        let (out, _) = redact_secrets(r#"api_key: "sk-live-abcdef123456""#);
        assert!(out.contains("[REDACTED:keyed-secret]"), "{out}");

        let (out, _) = redact_secrets("DB_PASSWORD = hunter2secret99");
        assert!(out.contains("[REDACTED:keyed-secret]"), "{out}");
    }

    #[test]
    fn keyed_allowlist_negatives_survive() {
        // Git SHAs, UUIDs, env refs, function calls, placeholders are not secrets.
        let cases = [
            "commit_token=3f5a2b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a",
            "token = a1b2c3d",
            "session_token=550e8400-e29b-41d4-a716-446655440000",
            "password=$DB_PASSWORD",
            "token = get_token()",
            "api_key=<your-api-key>",
        ];
        for case in cases {
            let (out, hits) = redact_secrets(case);
            assert_eq!(out, case, "should not redact: {case}");
            assert_eq!(total_hits(&hits), 0, "no hits expected for: {case}");
        }
    }

    #[test]
    fn aws_secret_with_slashes_and_hex_api_keys_redacted() {
        // AWS secret keys are base64 with frequent slashes — the path
        // allowlist must not save them (uppercase fails PATHLIKE).
        let (out, _) =
            redact_secrets("AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCyEXAMPLEKEY");
        assert!(out.contains("[REDACTED:keyed-secret]"), "{out}");

        // 32-hex is an API-key shape, not a git SHA.
        let (out, _) = redact_secrets("api_key=9f86d081884c7d659a2feaa0c55ad015");
        assert!(out.contains("[REDACTED:keyed-secret]"), "{out}");

        // Short and full git SHAs still pass.
        for sha in [
            "token=a1b2c3d",
            "commit_token=3f5a2b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a",
        ] {
            let (out, hits) = redact_secrets(sha);
            assert_eq!(out, sha);
            assert_eq!(total_hits(&hits), 0);
        }
    }

    #[test]
    fn long_low_entropy_value_survives_high_entropy_redacted() {
        // Long but wordy value assigned to a *_key variable: keep.
        let (out, hits) = redact_secrets("ssh_key=path/to/keys/deploy/production/id");
        assert_eq!(total_hits(&hits), 0, "{out}");

        // Long random value: redact.
        let (out, _) = redact_secrets("secret=q9X2vLp8Rj4tYw7ZnB3mK6dF1sG5hA0c");
        assert!(out.contains("[REDACTED:keyed-secret]"), "{out}");
    }

    #[test]
    fn jwt_assigned_to_key_not_double_redacted() {
        let jwt =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9P";
        let (out, hits) = redact_secrets(&format!("JWT_ACCESS_SECRET={jwt}"));
        assert!(out.contains("JWT_ACCESS_SECRET=[REDACTED:jwt]"), "{out}");
        assert_eq!(total_hits(&hits), 1, "{hits:?}");
    }

    /// Fixtures are assembled at runtime rather than written as literals.
    /// A well-formed key in the source is a well-formed key as far as any
    /// secret scanner is concerned — GitHub push protection rejects the push —
    /// and splitting the prefix from the body defeats that without weakening
    /// what the regex actually sees, which is the joined string.
    fn fake(prefix: &str, body: &str) -> String {
        format!("{prefix}{body}")
    }

    /// The motivating case for the vendor-prefix rules: a bare key in prose,
    /// with no `API_KEY=` to its left. Before these patterns every one of
    /// these survived verbatim.
    #[test]
    fn vendor_prefixed_keys_are_caught_bare_in_prose() {
        const BODY: &str = "x7Kq2mNp8vRt4wLs9dFg1hJk3nBc5yZa0eQu6iOp";
        let cases = [
            ("anthropic-key", fake("sk-ant-", &format!("api03-{BODY}"))),
            ("openai-key", fake("sk-proj-", BODY)),
            (
                "openai-key",
                fake("sk-", "Ab3dEf6gHi9jKl2mNo5pQr8sTu1vWx4yZa7bCd0eFg3hIj6k"),
            ),
            (
                "google-api-key",
                fake("AIza", "SyD3xK9mNp2vRt5wLs8dFg1hJk4nBc7yZaQ"),
            ),
            ("gitlab-token", fake("glpat-", "x7Kq2mNp8vRt4wLs9dFg")),
            (
                "sendgrid-key",
                fake(
                    "SG.",
                    "x7Kq2mNp8vRt4wLs9dFg1h.Jk3nBc5yZa0eQu6iOp2rSt4uVw7xYz1a",
                ),
            ),
            (
                "npm-token",
                fake("npm_", "x7Kq2mNp8vRt4wLs9dFg1hJk3nBc5yZa0eQu"),
            ),
            (
                "slack-app-token",
                fake("xapp-", "1-A012BCDEF-1234567890-abcdef0123456789"),
            ),
            (
                "slack-webhook",
                fake(
                    "https://hooks.slack.com/",
                    "services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX",
                ),
            ),
            ("stripe-key", fake("sk_", "live_x7Kq2mNp8vRt4wLs9dFg1hJk")),
            (
                "huggingface-token",
                fake("hf_", "x7Kq2mNp8vRt4wLs9dFg1hJk3nBc5yZa0e"),
            ),
        ];
        for (kind, secret) in cases {
            let input = format!("deploy uses {secret} for auth");
            let (out, hits) = redact_secrets(&input);
            assert!(
                out.contains(&format!("[REDACTED:{kind}]")),
                "{kind} not redacted in {input:?} -> {out:?}"
            );
            assert!(!out.contains(&secret), "{kind} survived verbatim: {out:?}");
            assert!(total_hits(&hits) >= 1, "no hit recorded for {kind}");
        }
    }

    /// Prose that merely starts the same way must survive, or every mention of
    /// these vendors becomes unreadable.
    #[test]
    fn vendor_prefixes_do_not_eat_ordinary_prose() {
        let cases = [
            "the sk-ant- prefix marks an Anthropic key",
            "AIza is the Google prefix",
            "set npm_config_registry in .npmrc",
            "read the docs at https://hooks.slack.com/services",
            "sk-proj- keys replaced the legacy ones",
        ];
        for case in cases {
            let (out, hits) = redact_secrets(case);
            assert_eq!(out, case, "prose was redacted: {case}");
            assert_eq!(total_hits(&hits), 0, "hits for prose: {case}");
        }
    }

    /// Code fragments assigned to secret-ish names, all taken from a real
    /// corpus where a stricter whole-call rule redacted every one of them and
    /// no actual secret. A parenthesis means the value is code: no credential
    /// format in `WHOLE_MATCH` — base64, hex, url-safe base64 — contains one.
    #[test]
    fn code_fragments_assigned_to_secret_names_survive() {
        let cases = [
            "OPENAI_API_KEY: z.string(),",
            "token = get_token()",
            "api_key=cfg.read()",
            "refresh_token = getServerSession(req)",
            "topic_key: Some(format!(",
            "password: decrypt(raw)",
        ];
        for case in cases {
            let (out, hits) = redact_secrets(case);
            assert_eq!(out, case, "code was redacted: {case}");
            assert_eq!(total_hits(&hits), 0, "hits for: {case}");
        }
    }

    /// The rule used to match the marker it had just written, so every `runar
    /// scrub` re-reported and rewrote the same rows and the count could never
    /// reach zero.
    #[test]
    fn url_credential_redaction_is_idempotent() {
        let (once, hits) = redact_secrets("postgres://muninn:hunter2secret@10.0.0.5/db");
        assert_eq!(total_hits(&hits), 1);
        let (twice, hits2) = redact_secrets(&once);
        assert_eq!(twice, once, "second pass changed already-redacted text");
        assert_eq!(total_hits(&hits2), 0, "second pass reported a fresh hit");
    }

    #[test]
    fn scrub_reports_and_rewrites_in_place() {
        let secret = fake("sk-ant-", "api03-x7Kq2mNp8vRt4wLs9dFg1hJk3nBc5yZa");
        let mut s = format!("ship with {secret}");
        assert_eq!(scrub(&mut s), 1);
        assert!(!s.contains(&secret), "{s}");

        let mut clean = "nothing to see".to_string();
        assert_eq!(scrub(&mut clean), 0);
        assert_eq!(clean, "nothing to see");
    }

    #[test]
    fn redact_secrets_value_walks_nested_json() {
        let mut v = serde_json::json!({
            "cmd": "export TENANT_DB_PASS=Sup3rS3cret99",
            "nested": { "list": ["ghp_abcdefghijklmnopqrstuvwxyz0123456789", "plain"] },
            "num": 42
        });
        let hits = redact_secrets_value(&mut v);
        assert_eq!(hits, 2);
        let s = v.to_string();
        assert!(!s.contains("Sup3rS3cret99"));
        assert!(!s.contains("ghp_abcdef"));
        assert!(s.contains("plain"));
    }
}
