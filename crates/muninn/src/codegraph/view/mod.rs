//! The human-facing code view: payload, and the page that renders it.
//!
//! Every other surface on the graph — the CLI, the MCP tools, the hooks —
//! targets an agent and is token-lean for that reason. This one targets a
//! person, so it trades bytes for legibility.
//!
//! The page is one self-contained file with no external requests. The same
//! assets serve both delivery modes: `graph export` inlines the payload as
//! `window.__GRAPH__`, and the server leaves that `null` so the identical
//! JavaScript fetches `/api/graph` instead. Keeping the data source swappable
//! is what makes those one build rather than two.

pub mod serve;

use super::store::{CodeGraphStore, ProjectRow, ViewEdge, ViewNode};
use serde_json::{json, Value};

const HTML: &str = include_str!("index.html");
const CSS: &str = include_str!("app.css");
const JS: &str = include_str!("app.js");

/// Directories kept as their own block before the tail is folded together.
///
/// A real project can carry 135 directories; a treemap of 135 cells is a
/// mosaic, not a map. The tail becomes one honest "N more" block rather than
/// being dropped.
const MAX_MODULES: usize = 22;

/// How many rows each whole-graph ranking carries.
const RANK_LIMIT: usize = 14;

/// The module a file belongs to: its parent directory, or a marker for files
/// sitting at the project root.
fn module_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "(root)",
    }
}

/// Everything the view needs for one project.
pub fn payload(store: &CodeGraphStore, project: &str) -> super::store::Result<Value> {
    let nodes = store.all_nodes(project)?;
    let edges = store.all_edges(project)?;
    let coverage = store.coverage(project)?;
    let indexed_at = store.indexed_at(project)?.unwrap_or_default();
    let root = store
        .project_rows()?
        .into_iter()
        .find(|p| p.project == project)
        .map(|p| p.root)
        .unwrap_or_default();

    Ok(json!({
        "meta": {
            "project": project,
            "root": root,
            "indexedAt": indexed_at,
            "symbols": nodes.len(),
            "edges": edges.len(),
            "files": coverage.files_total,
            "unresolvedCalls": coverage.unresolved_calls,
        },
        "modules": modules_json(&nodes),
        "nodes": nodes.iter().map(node_json).collect::<Vec<_>>(),
        "edges": edges.iter().map(edge_json).collect::<Vec<_>>(),
        "ranks": ranks_json(&nodes),
    }))
}

/// Short keys throughout: this payload is one row per symbol, and on a
/// 12k-symbol project the difference between `qualifiedName` and `qn` is
/// hundreds of kilobytes on the wire.
fn node_json(n: &ViewNode) -> Value {
    json!({
        "id": n.id,
        "nm": n.name,
        "qn": n.qualified_name,
        "lb": n.label,
        "f": n.file_path,
        "ln": n.start_line,
        "sig": n.signature,
        "ex": n.exported,
        "cx": n.metrics.complexity,
        "cg": n.metrics.cognitive,
        "pc": n.metrics.param_count,
        // Call sites and distinct counterparts are both reported because they
        // answer different questions. They differ by up to 3.6x on real hubs,
        // and a view that showed only the first would rank a function called
        // ten times from one test above one called once from ten places.
        "si": n.call_sites_in,
        "ci": n.callers,
        "so": n.call_sites_out,
        "co": n.callees,
        "m": module_of(&n.file_path),
    })
}

fn edge_json(e: &ViewEdge) -> Value {
    json!({
        "s": e.source,
        "t": e.target,
        "ty": e.kind,
        // Stays null for kinds the resolver does not score, e.g. IMPLEMENTS.
        // A default number here would invent confidence the graph never had.
        "c": e.confidence,
        "r": e.resolution,
    })
}

/// Directory blocks, largest first, with the tail folded into one.
fn modules_json(nodes: &[ViewNode]) -> Value {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for n in nodes {
        *counts.entry(module_of(&n.file_path)).or_insert(0) += 1;
    }
    let mut all: Vec<(&str, usize)> = counts.into_iter().collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

    let mut out: Vec<Value> = all
        .iter()
        .take(MAX_MODULES)
        .map(|(id, n)| json!({ "id": id, "label": id, "n": n }))
        .collect();

    if all.len() > MAX_MODULES {
        let rest: usize = all[MAX_MODULES..].iter().map(|(_, n)| n).sum();
        let folded = all.len() - MAX_MODULES;
        out.push(json!({
            "id": "\u{0}rest",
            "label": format!("{folded} more directories"),
            "n": rest,
        }));
    }
    Value::Array(out)
}

/// Rankings over the whole graph, not just what gets drawn — the view says so
/// where it shows them, and marks the rows it cannot select.
fn ranks_json(nodes: &[ViewNode]) -> Value {
    let top = |key: fn(&ViewNode) -> usize| -> Vec<Value> {
        let mut v: Vec<&ViewNode> = nodes.iter().collect();
        v.sort_by(|a, b| key(b).cmp(&key(a)).then(a.name.cmp(&b.name)));
        v.into_iter()
            .take(RANK_LIMIT)
            .map(|n| {
                json!({
                    "id": n.id, "nm": n.name, "qn": n.qualified_name,
                    "cx": n.metrics.complexity, "si": n.call_sites_in, "ci": n.callers,
                })
            })
            .collect()
    };
    json!({
        "complexity": top(|n| n.metrics.complexity as usize),
        "callSites": top(|n| n.call_sites_in),
    })
}

/// The project list the switcher offers.
pub fn projects_json(rows: &[ProjectRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|p| {
                json!({
                    "project": p.project,
                    "root": p.root,
                    "indexedAt": p.indexed_at,
                    "symbols": p.symbols,
                    "edges": p.edges,
                    "files": p.files,
                })
            })
            .collect(),
    )
}

/// The page. `data` is the inlined payload for a static export, or `None` for
/// the server, whose copy fetches instead.
pub fn page(title: &str, data: Option<&Value>) -> String {
    let blob = match data {
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    // Order matters: substitute the payload last so a symbol name containing
    // one of these markers cannot be mistaken for one.
    HTML.replace("__TITLE__", &escape_html(title))
        .replace("__CSS__", CSS)
        .replace("__JS__", JS)
        .replace("__DATA__", &escape_script(&blob))
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `</script>` inside a JSON string literal ends the block early, and a symbol
/// name or signature is source text we do not control. Breaking the sequence
/// keeps the JSON valid while making it inert to the HTML parser.
fn escape_script(json: &str) -> String {
    json.replace("</", "<\\/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegraph::store::{EdgeRecord, FileRecord, RawFacts, SymbolRecord};
    use crate::codegraph::{EdgeKind, FileStatus, Resolution, SymbolLabel, SymbolMetrics};
    use std::path::Path;

    fn sym(file: &str, name: &str, cx: u32) -> SymbolRecord {
        SymbolRecord {
            name: name.into(),
            qualified_name: format!("{file}:{name}"),
            label: SymbolLabel::Function.as_str(),
            file_path: file.into(),
            start_line: 1,
            end_line: 2,
            signature: format!("fn {name}()"),
            exported: true,
            metrics: SymbolMetrics {
                complexity: cx,
                ..Default::default()
            },
        }
    }

    fn seeded() -> CodeGraphStore {
        let s = CodeGraphStore::in_memory().unwrap();
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "src/deep/a.rs",
                lang: Some("rust"),
                content_hash: "h",
                status: FileStatus::Indexed,
                detail: None,
            },
            &[
                sym("src/deep/a.rs", "alpha", 9),
                sym("src/deep/a.rs", "beta", 0),
            ],
            RawFacts::default(),
        )
        .unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "top.rs",
                lang: Some("rust"),
                content_hash: "h",
                status: FileStatus::Indexed,
                detail: None,
            },
            &[sym("top.rs", "gamma", 3)],
            RawFacts::default(),
        )
        .unwrap();
        s.rebuild_edges(
            "p",
            &[EdgeRecord {
                source_qualified: "top.rs:gamma".into(),
                target_qualified: "src/deep/a.rs:alpha".into(),
                kind: EdgeKind::Calls,
                resolution: Some(Resolution::ImportMap),
                line: 4,
            }],
        )
        .unwrap();
        s
    }

    #[test]
    fn payload_carries_ids_that_edges_resolve_against() {
        let s = seeded();
        let p = payload(&s, "p").unwrap();
        let ids: Vec<i64> = p["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_i64().unwrap())
            .collect();
        let e = &p["edges"].as_array().unwrap()[0];
        assert!(ids.contains(&e["s"].as_i64().unwrap()));
        assert!(ids.contains(&e["t"].as_i64().unwrap()));
        assert_eq!(e["r"], "import_map");
        assert_eq!(e["c"], 0.95);
    }

    #[test]
    fn a_file_at_the_project_root_still_gets_a_module() {
        assert_eq!(module_of("src/deep/a.rs"), "src/deep");
        assert_eq!(module_of("top.rs"), "(root)");
        let s = seeded();
        let p = payload(&s, "p").unwrap();
        let labels: Vec<&str> = p["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["label"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"src/deep"));
        assert!(
            labels.contains(&"(root)"),
            "a root-level file was dropped from the map"
        );
    }

    #[test]
    fn both_fan_counts_reach_the_payload() {
        let s = seeded();
        let p = payload(&s, "p").unwrap();
        let alpha = p["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["nm"] == "alpha")
            .unwrap()
            .clone();
        assert_eq!(alpha["si"], 1);
        assert_eq!(alpha["ci"], 1);
        assert!(alpha.get("so").is_some() && alpha.get("co").is_some());
    }

    /// A signature is source text. If it contained `</script>` the browser
    /// would end the block early and the page would render as garbage.
    #[test]
    fn a_payload_cannot_close_the_script_block() {
        let hostile = json!({ "nodes": [{ "sig": "fn x() { \"</script><b>\" }" }] });
        let html = page("t", Some(&hostile));
        assert!(
            !html.contains("</script><b>"),
            "the payload escaped its block"
        );
        assert!(html.contains("<\\/script>"));
    }

    #[test]
    fn the_server_page_inlines_no_payload() {
        let html = page("t", None);
        assert!(html.contains("window.__GRAPH__ = null;"));
        assert!(
            html.contains("\"api/graph\""),
            "the fetch path must survive, and stay relative so it resolves under the token prefix"
        );
    }

    /// Nothing may be fetched at open time — the export has to work offline
    /// and from a file:// URL. Note this checks request-triggering syntax, not
    /// the literal `http`: the SVG namespace URI is a constant identifier that
    /// is never dereferenced, and banning the substring would only teach the
    /// next person to weaken the test.
    #[test]
    fn the_page_makes_no_external_requests() {
        let s = seeded();
        let html = page("t", Some(&payload(&s, "p").unwrap()));
        for needle in [
            "src=\"http",
            "src='http",
            "href=\"http",
            "href='http",
            "url(http",
            "url(\"http",
            "url('http",
            "@import",
            "fetch(\"http",
            "fetch('http",
            "//cdn",
            "integrity=",
        ] {
            assert!(
                !html.contains(needle),
                "page reaches the network via {needle}"
            );
        }
        // and the assets really are inlined rather than linked
        assert!(html.contains("<style>") && html.contains("runar graph"));
        assert!(
            !html.contains("<link"),
            "a stylesheet is linked instead of inlined"
        );
    }

    #[test]
    fn the_title_cannot_inject_markup() {
        let html = page("</title><script>bad()</script>", None);
        assert!(!html.contains("<script>bad()"));
    }
}
