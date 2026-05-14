use super::{TechDebtMarker, TechDebtType};

/// Extract TODO/FIXME/HACK/XXX/OPTIMIZE/BUG/NOTE markers from source text.
/// Looks for comments (`//`, `#`, `/*`, `*`) containing the marker.
pub fn extract(source: &str, file_path: &str) -> Vec<TechDebtMarker> {
    let mut out = Vec::new();
    for (idx, line) in source.lines().enumerate() {
        let comment_body = match extract_comment_body(line) {
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

fn extract_comment_body(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    for prefix in ["//", "#", "/*", "*"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let body = rest.trim_start_matches(['*', '/']).trim();
            if !body.is_empty() {
                return Some(body.to_string());
            }
        }
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
}
