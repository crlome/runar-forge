use std::path::Path;

use super::{TechDebtMarker, TechDebtType};

/// Extract TODO/FIXME/HACK/XXX/OPTIMIZE/BUG/NOTE markers from source text.
/// Looks for comments (`//`, `#`, `/*`, `*`) containing the marker, either at
/// the start of the line or trailing after code. In markdown `#` and `*` are
/// headings and bullets, so only `<!-- ... -->` counts there.
pub fn extract(source: &str, file_path: &str) -> Vec<TechDebtMarker> {
    let markdown = is_markdown(file_path);
    let mut out = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        let comment_body = match extract_comment_body(line, markdown) {
            Some(c) => c,
            None => continue,
        };
        let upper = comment_body.to_uppercase();
        let marker_type = if upper.starts_with("TODO") {
            TechDebtType::Todo
        } else if upper.starts_with("FIXME") {
            TechDebtType::Fixme
        } else if upper.starts_with("HACK") {
            TechDebtType::Hack
        } else if upper.starts_with("XXX") {
            TechDebtType::Xxx
        } else if upper.starts_with("OPTIMIZE") {
            TechDebtType::Optimize
        } else if upper.starts_with("BUG") {
            TechDebtType::Bug
        } else if upper.starts_with("NOTE") {
            TechDebtType::Note
        } else {
            continue;
        };

        let payload = strip_marker_prefix(&comment_body);
        if payload.is_empty() {
            continue;
        }
        out.push(TechDebtMarker {
            marker_type,
            line: idx + 1,
            comment: payload,
            file_path: file_path.to_string(),
        });
    }
    out
}

fn is_markdown(file_path: &str) -> bool {
    Path::new(file_path).extension().is_some_and(|ext| {
        matches!(
            ext.to_string_lossy().to_lowercase().as_str(),
            "md" | "mdx" | "markdown"
        )
    })
}

fn extract_comment_body(line: &str, markdown: bool) -> Option<String> {
    if markdown {
        return html_comment_body(line);
    }
    let trimmed = line.trim_start();
    for prefix in ["//", "#", "/*", "*"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let body = rest.trim_start_matches(['*', '/']).trim();
            if !body.is_empty() {
                return Some(body.to_string());
            }
        }
    }
    trailing_comment_body(line)
}

fn html_comment_body(line: &str) -> Option<String> {
    let after = &line[line.find("<!--")? + 4..];
    let body = match after.find("-->") {
        Some(end) => &after[..end],
        None => after,
    };
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    Some(body.to_string())
}

/// First `//` or `#` that is not inside a string literal, so that a URL or a
/// quoted `#` in code is not mistaken for a comment.
fn trailing_comment_body(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if q == b'"' && b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' || b == b'`' {
                    quote = Some(b);
                } else if b == b'#' || (b == b'/' && bytes.get(i + 1) == Some(&b'/')) {
                    // `i` indexes an ASCII byte, so slicing here is on a char boundary.
                    let body = line[i..].trim_start_matches(['/', '#', '*']).trim();
                    if body.is_empty() {
                        return None;
                    }
                    return Some(body.to_string());
                }
            }
        }
        i += 1;
    }
    None
}

fn strip_marker_prefix(body: &str) -> String {
    // strip leading marker word + optional `:` / `(name)`
    let mut chars = body.chars().peekable();
    let mut marker = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphabetic() {
            marker.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let rest: String = chars.collect();
    let rest = rest
        .trim_start()
        .trim_start_matches([':', '-', '('])
        .trim_start_matches(|c: char| c != ')' && c != ':' && !c.is_whitespace())
        .trim_start_matches([')', ':'])
        .trim()
        .to_string();
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_todo() {
        let src =
            "// TODO: rename this\nfn foo() {}\n// FIXME(alice): broken\n# HACK work around\n";
        let m = extract(src, "f.rs");
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].marker_type, TechDebtType::Todo);
        assert_eq!(m[0].line, 1);
        assert_eq!(m[0].comment, "rename this");
        assert_eq!(m[1].marker_type, TechDebtType::Fixme);
        assert_eq!(m[1].comment, "broken");
        assert_eq!(m[2].marker_type, TechDebtType::Hack);
    }

    #[test]
    fn ignores_non_markers() {
        let src = "// just a regular comment\nlet x = 1;\n";
        assert!(extract(src, "f").is_empty());
    }

    #[test]
    fn extracts_trailing_marker_after_code() {
        let src = "let x = 1; // TODO: fix the cast\n";
        let m = extract(src, "f.rs");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].marker_type, TechDebtType::Todo);
        assert_eq!(m[0].line, 1);
        assert_eq!(m[0].comment, "fix the cast");
    }

    #[test]
    fn extracts_trailing_hash_marker() {
        let src = "value = compute()  # FIXME: slow\n";
        let m = extract(src, "f.py");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].marker_type, TechDebtType::Fixme);
        assert_eq!(m[0].comment, "slow");
    }

    #[test]
    fn url_in_string_is_not_a_comment() {
        let src = "let url = \"http://example.com/TODO: nope\";\n";
        assert!(extract(src, "f.rs").is_empty());
    }

    #[test]
    fn hash_marker_in_string_is_ignored() {
        let src = "let s = \"# TODO: nope\";\nlet t = \"escaped \\\" # TODO: nope\";\n";
        assert!(extract(src, "f.rs").is_empty());
    }

    #[test]
    fn markdown_heading_is_not_a_marker() {
        let src = "# TODO list\n\nSome prose about TODO items.\n";
        assert!(extract(src, "docs/README.md").is_empty());
    }

    #[test]
    fn markdown_bullet_is_not_a_marker() {
        let src = "* NOTE the following steps\n* TODO: ship it\n";
        assert!(extract(src, "docs/guide.MD").is_empty());
    }

    #[test]
    fn markdown_html_comment_is_a_marker() {
        let src = "# Title\n\n<!-- TODO: write the setup section -->\n";
        let m = extract(src, "README.mdx");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].marker_type, TechDebtType::Todo);
        assert_eq!(m[0].line, 3);
        assert_eq!(m[0].comment, "write the setup section");
    }

    #[test]
    fn markdown_code_fence_line_is_not_a_marker() {
        let src = "```rust\nlet x = 1; // TODO: not a doc marker\n```\n";
        assert!(extract(src, "guide.markdown").is_empty());
    }
}
