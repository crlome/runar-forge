//! Diagnostic: why does a call site produce no edge?
//!
//! `cargo run --release -p runar-muninn --example unresolved -- <path>`
//!
//! Prints every unmatched call bucketed by the reason it could not be an edge,
//! so the recall gap can be argued about with counts rather than intuition.

use std::collections::HashMap;
use std::path::Path;

use runar_muninn::codegraph::extract::{extract_file, is_supported};
use runar_muninn::codegraph::resolve::{resolve, FileFacts};
use runar_muninn::huginn::graph::parsers::Lang;
use runar_muninn::huginn::scanner::scan_project;

fn main() {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".".to_string())
        .parse::<String>()
        .unwrap();
    let root = Path::new(&root);

    let scan = scan_project(root);
    let mut files = Vec::new();
    for f in &scan.files {
        let Some(lang) = Lang::from_extension(&f.extension) else {
            continue;
        };
        if !is_supported(lang) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&f.path) else {
            continue;
        };
        let extract = extract_file(&src, lang, &f.relative_path);
        files.push(FileFacts {
            path: f.relative_path.clone(),
            extract,
        });
    }

    // Every definition name in the project, so "does any definition carry this
    // name at all" can be answered — that is the line between a call the
    // resolver *could* have matched and one that points outside the project.
    let mut defined: HashMap<&str, usize> = HashMap::new();
    let mut containers: HashMap<&str, usize> = HashMap::new();
    for f in &files {
        for s in &f.extract.symbols {
            *defined.entry(s.name.as_str()).or_insert(0) += 1;
            if let Some(c) = &s.container {
                *containers.entry(c.as_str()).or_insert(0) += 1;
            }
        }
    }

    let out = resolve(&files, root);
    let resolved_sites: usize = out.edges.len();
    let unresolved_total: usize = out.unresolved.values().sum();

    // Re-walk the call sites and bucket the ones that produced no edge. An edge
    // is keyed by (source, target, line); a call site that produced one is not
    // in the gap, so match on source+line.
    let mut edged: HashMap<(&str, u32), ()> = HashMap::new();
    for e in &out.edges {
        edged.insert((e.source_qualified.as_str(), e.line), ());
    }

    let mut buckets: HashMap<&'static str, usize> = HashMap::new();
    let mut samples: HashMap<&'static str, Vec<String>> = HashMap::new();
    let mut by_callee: HashMap<String, usize> = HashMap::new();

    for f in &files {
        for c in &f.extract.calls {
            let Some(caller) = c.caller.as_deref() else {
                *buckets.entry("no enclosing definition").or_insert(0) += 1;
                continue;
            };
            if edged.contains_key(&(caller, c.line)) {
                continue;
            }
            let name_known = defined.contains_key(c.callee.as_str());
            let bucket = match (&c.qualifier, name_known) {
                (None, false) => "bare call, name not defined in project",
                (None, true) => "bare call, name IS defined in project",
                (Some(q), false) => {
                    if containers.contains_key(q.as_str()) {
                        "qualified by a known container, name not defined"
                    } else {
                        "qualified, name not defined in project"
                    }
                }
                (Some(_), true) => "qualified, name IS defined in project",
            };
            *buckets.entry(bucket).or_insert(0) += 1;
            *by_callee.entry(c.callee.clone()).or_insert(0) += 1;
            let s = samples.entry(bucket).or_default();
            if s.len() < 12 {
                s.push(match &c.qualifier {
                    Some(q) => format!("{q}.{}()  [{}:{}]", c.callee, f.path, c.line),
                    None => format!("{}()  [{}:{}]", c.callee, f.path, c.line),
                });
            }
        }
    }

    println!("files parsed      {}", files.len());
    println!("edges             {resolved_sites}");
    println!("unresolved (rpt)  {unresolved_total}");
    println!();

    let mut ordered: Vec<_> = buckets.iter().collect();
    ordered.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    let gap: usize = buckets.values().sum();
    println!("=== unmatched call sites: {gap} ===");
    for (bucket, n) in &ordered {
        let pct = 100.0 * **n as f64 / gap.max(1) as f64;
        println!("  {n:6}  {pct:5.1}%  {bucket}");
    }

    println!("\n=== the two buckets that are actual misses ===");
    for bucket in [
        "bare call, name IS defined in project",
        "qualified, name IS defined in project",
        "qualified by a known container, name not defined",
    ] {
        if let Some(s) = samples.get(bucket) {
            println!("\n-- {bucket} --");
            for line in s {
                println!("   {line}");
            }
        }
    }

    println!("\n=== most frequent unmatched callees ===");
    let mut top: Vec<_> = by_callee.into_iter().collect();
    top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (name, n) in top.into_iter().take(30) {
        let known = if defined.contains_key(name.as_str()) {
            "  <- defined in project"
        } else {
            ""
        };
        println!("  {n:5}  {name}{known}");
    }
}
