//! Rendering query results for an agent to read.
//!
//! Column names are declared once in a header rather than repeated per row as
//! JSON keys would, and nothing is padded for alignment — both cost tokens on
//! every call and buy nothing a model needs. `format: "json"` is available for
//! anything consuming these programmatically.

use super::store::{Coverage, Neighbor, SymbolRow};

/// `file:line`, the form an editor and a human both accept.
fn locus(row: &SymbolRow) -> String {
    format!("{}:{}", row.file_path, row.start_line)
}

/// One line per definition: what it is, where it is, how connected, how complex.
pub fn symbols(rows: &[SymbolRow]) -> String {
    if rows.is_empty() {
        return "no matching symbols".into();
    }
    let mut out = format!(
        "symbols[{}] name | label | file:line | fan-in | complexity\n",
        rows.len()
    );
    for r in rows {
        out.push_str(&format!(
            "{} | {} | {} | {} | {}\n",
            r.qualified_name,
            r.label,
            locus(r),
            r.fan_in,
            r.metrics.complexity
        ));
    }
    out
}

/// One line per edge. `via` is how the call was matched, which is the reader's
/// cue for how much to trust it.
pub fn neighbors(heading: &str, rows: &[Neighbor]) -> String {
    if rows.is_empty() {
        return format!("{heading}[0]\n");
    }
    let mut out = format!("{heading}[{}] name | file:line | depth | via\n", rows.len());
    for n in rows {
        out.push_str(&format!(
            "{} | {} | {} | {}\n",
            n.symbol.qualified_name,
            locus(&n.symbol),
            n.depth,
            n.resolution.as_deref().unwrap_or(n.kind.as_str())
        ));
    }
    out
}

/// A single definition in full, with its immediate neighbourhood.
pub fn symbol_detail(row: &SymbolRow, callers: &[Neighbor], callees: &[Neighbor]) -> String {
    let m = &row.metrics;
    let mut out = format!(
        "{}\n{} at {}-{}{}\n{}\ncomplexity {} | cognitive {} | loop depth {} | params {} | fan-in {} | fan-out {}\n",
        row.qualified_name,
        row.label,
        locus(row),
        row.end_line,
        if row.exported { " (exported)" } else { "" },
        row.signature,
        m.complexity,
        m.cognitive,
        m.loop_depth,
        m.param_count,
        row.fan_in,
        row.fan_out,
    );
    out.push('\n');
    out.push_str(&neighbors("callers", callers));
    out.push_str(&neighbors("callees", callees));
    out
}

/// Coverage as a reader should see it: what was parsed, and what was not.
pub fn coverage(project: &str, cov: &Coverage) -> String {
    let parsed = cov.indexed + cov.partial;
    let pct = if cov.files_total > 0 {
        parsed * 100 / cov.files_total
    } else {
        0
    };
    let mut out = format!(
        "{project}: {} symbols, {} edges, {parsed}/{} files parsed ({pct}%)\n",
        cov.symbols, cov.edges, cov.files_total
    );
    if cov.partial > 0 {
        out.push_str(&format!(
            "{} file(s) parsed only partially — symbols there may be incomplete\n",
            cov.partial
        ));
    }
    if cov.errored > 0 {
        out.push_str(&format!("{} file(s) could not be read\n", cov.errored));
    }
    if !cov.skipped_by_ext.is_empty() {
        let listed: Vec<String> = cov
            .skipped_by_ext
            .iter()
            .map(|(ext, n)| format!("{ext} ({n})"))
            .collect();
        out.push_str(&format!(
            "not parseable by this build: {}\n",
            listed.join(", ")
        ));
    }
    out.push_str(&format!(
        "{} call site(s) matched no definition and are not in the graph\n",
        cov.unresolved_calls
    ));
    out
}

/// JSON for programmatic consumers.
pub fn symbols_json(rows: &[SymbolRow]) -> serde_json::Value {
    serde_json::Value::Array(rows.iter().map(symbol_json).collect())
}

pub fn symbol_json(r: &SymbolRow) -> serde_json::Value {
    serde_json::json!({
        "name": r.name,
        "qualifiedName": r.qualified_name,
        "label": r.label,
        "file": r.file_path,
        "startLine": r.start_line,
        "endLine": r.end_line,
        "signature": r.signature,
        "exported": r.exported,
        "complexity": r.metrics.complexity,
        "cognitive": r.metrics.cognitive,
        "loopDepth": r.metrics.loop_depth,
        "paramCount": r.metrics.param_count,
        "fanIn": r.fan_in,
        "fanOut": r.fan_out,
    })
}

pub fn neighbors_json(rows: &[Neighbor]) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .map(|n| {
                serde_json::json!({
                    "symbol": symbol_json(&n.symbol),
                    "edgeType": n.kind,
                    "confidence": n.confidence,
                    "resolution": n.resolution,
                    "line": n.line,
                    "depth": n.depth,
                })
            })
            .collect(),
    )
}

pub fn coverage_json(cov: &Coverage) -> serde_json::Value {
    serde_json::json!({
        "filesTotal": cov.files_total,
        "indexed": cov.indexed,
        "partial": cov.partial,
        "skippedByLanguage": cov.skipped_lang,
        "errored": cov.errored,
        "symbols": cov.symbols,
        "edges": cov.edges,
        "unresolvedCalls": cov.unresolved_calls,
        "notParseableByExtension": cov.skipped_by_ext.iter()
            .map(|(e, n)| serde_json::json!({ "extension": e, "files": n }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegraph::SymbolMetrics;

    fn row(name: &str, label: &str, fan_in: usize) -> SymbolRow {
        SymbolRow {
            id: 1,
            name: name.into(),
            qualified_name: format!("src/a.rs:{name}"),
            label: label.into(),
            file_path: "src/a.rs".into(),
            start_line: 12,
            end_line: 30,
            signature: format!("fn {name}()"),
            exported: true,
            metrics: SymbolMetrics {
                complexity: 3,
                cognitive: 5,
                loop_depth: 1,
                param_count: 2,
            },
            fan_in,
            fan_out: 4,
        }
    }

    #[test]
    fn the_header_names_the_columns_once() {
        let out = symbols(&[row("alpha", "Function", 7), row("beta", "Method", 1)]);
        assert!(out.starts_with("symbols[2] name | label | file:line | fan-in | complexity\n"));
        // One line per row, and no repeated key names in the body.
        assert_eq!(out.lines().count(), 3);
        assert!(out.contains("src/a.rs:alpha | Function | src/a.rs:12 | 7 | 3"));
        assert!(!out.lines().skip(1).any(|l| l.contains("label")));
    }

    #[test]
    fn an_empty_result_says_so_rather_than_printing_a_bare_header() {
        assert_eq!(symbols(&[]), "no matching symbols");
    }

    #[test]
    fn coverage_names_what_it_could_not_parse() {
        let cov = Coverage {
            files_total: 10,
            indexed: 6,
            partial: 1,
            skipped_lang: 3,
            errored: 0,
            symbols: 40,
            edges: 25,
            unresolved_calls: 12,
            skipped_by_ext: vec![(".java".into(), 2), (".rb".into(), 1)],
        };
        let out = coverage("proj", &cov);
        assert!(out.contains("7/10 files parsed (70%)"), "got {out}");
        assert!(
            out.contains(".java (2)"),
            "the gap has to name itself: {out}"
        );
        assert!(out.contains("12 call site(s) matched no definition"));
        assert!(out.contains("parsed only partially"));
    }

    #[test]
    fn detail_carries_the_metrics_and_both_directions() {
        let callers = vec![Neighbor {
            symbol: row("caller", "Function", 0),
            kind: "CALLS".into(),
            confidence: Some(0.9),
            resolution: Some("same_module".into()),
            line: 4,
            depth: 1,
        }];
        let out = symbol_detail(&row("alpha", "Function", 1), &callers, &[]);
        assert!(out.contains("complexity 3 | cognitive 5"));
        assert!(out.contains("callers[1]"));
        assert!(out.contains("callees[0]"));
        assert!(
            out.contains("same_module"),
            "the tier is the trust cue: {out}"
        );
    }
}
