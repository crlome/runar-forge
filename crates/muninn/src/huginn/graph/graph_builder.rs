use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::Path;

use crate::huginn::scanner::FileEntry;

use super::import_parser::{parse_file, resolve_import};
use super::parsers::Lang;
use super::{DependencyGraph, FileNode};

pub struct GraphBuilder;

impl GraphBuilder {
    pub fn build(
        files: &[FileEntry],
        project_root: &Path,
        entry_points: &[String],
    ) -> DependencyGraph {
        let entry_set: HashSet<String> = entry_points.iter().cloned().collect();
        let mut graph = DependencyGraph::default();

        for f in files {
            let lang = match Lang::from_extension(&f.extension) {
                Some(l) => l,
                None => continue,
            };

            let content = fs::read_to_string(&f.path).unwrap_or_default();
            let parsed = parse_file(&content, lang, &f.relative_path);

            let mut raw_imports = Vec::new();
            let mut resolved = Vec::new();
            for imp in &parsed.imports {
                raw_imports.push(imp.source.clone());
                if imp.is_relative {
                    if let Some(r) = resolve_import(&imp.source, &f.path, project_root, lang) {
                        resolved.push(r);
                    }
                }
            }

            let node = FileNode {
                path: f.relative_path.clone(),
                absolute_path: f.path.clone(),
                extension: f.extension.clone(),
                imports: raw_imports,
                resolved_imports: resolved.clone(),
                export_count: parsed.exports.len(),
                line_count: f.line_count,
                last_modified: f.last_modified,
                is_entry_point: entry_set.contains(&f.relative_path),
                depth: -1,
            };

            graph.nodes.insert(f.relative_path.clone(), node);
            graph
                .edges
                .insert(f.relative_path.clone(), resolved.iter().cloned().collect());

            for r in &resolved {
                graph
                    .reverse_edges
                    .entry(r.clone())
                    .or_default()
                    .insert(f.relative_path.clone());
            }
        }

        calculate_depths(&mut graph, &entry_set);
        graph
    }

    /// Barrel files: small index files that mostly re-export
    pub fn detect_barrels(graph: &DependencyGraph) -> Vec<String> {
        let mut barrels = Vec::new();
        for (path, node) in &graph.nodes {
            let is_index = path.ends_with("index.ts")
                || path.ends_with("index.tsx")
                || path.ends_with("index.js")
                || path.ends_with("index.jsx");
            if !is_index {
                continue;
            }
            if node.line_count <= 30 && node.export_count > 0 && !node.resolved_imports.is_empty() {
                barrels.push(path.clone());
            }
        }
        barrels
    }
}

fn calculate_depths(graph: &mut DependencyGraph, entry_paths: &HashSet<String>) {
    let mut queue: VecDeque<(String, i32)> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();

    for ep in entry_paths {
        if let Some(n) = graph.nodes.get_mut(ep) {
            n.depth = 0;
            queue.push_back((ep.clone(), 0));
            visited.insert(ep.clone());
        }
    }

    while let Some((path, depth)) = queue.pop_front() {
        let edges = match graph.edges.get(&path) {
            Some(e) => e.clone(),
            None => continue,
        };
        for imp in edges {
            if !visited.insert(imp.clone()) {
                continue;
            }
            if let Some(n) = graph.nodes.get_mut(&imp) {
                n.depth = depth + 1;
                queue.push_back((imp, depth + 1));
            }
        }
    }

    // Unreachable files: max_depth + 1
    let max_depth = graph
        .nodes
        .values()
        .map(|n| n.depth)
        .filter(|d| *d >= 0)
        .max()
        .unwrap_or(0);
    for n in graph.nodes.values_mut() {
        if n.depth < 0 {
            n.depth = max_depth + 1;
        }
    }
}

/// Fan-in: how many files import this one.
pub fn fan_in(graph: &DependencyGraph, path: &str) -> usize {
    graph.reverse_edges.get(path).map(|s| s.len()).unwrap_or(0)
}

/// Fan-out: how many files this one imports.
pub fn fan_out(graph: &DependencyGraph, path: &str) -> usize {
    graph.edges.get(path).map(|s| s.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;

    fn mk_entry(root: &Path, rel: &str, content: &str) -> FileEntry {
        let path = root.join(rel);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&path, content).unwrap();
        FileEntry {
            path,
            relative_path: rel.to_string(),
            size: content.len() as u64,
            line_count: content.lines().count(),
            last_modified: Some(SystemTime::now()),
            extension: rel.rsplit('.').next().unwrap_or("").to_string(),
        }
    }

    #[test]
    fn builds_graph_with_depths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let entry = mk_entry(root, "main.ts", "import { a } from './a'\nimport './b'\n");
        let a = mk_entry(
            root,
            "a.ts",
            "import { c } from './c'\nexport const a = 1\n",
        );
        let b = mk_entry(root, "b.ts", "export const b = 1\n");
        let c = mk_entry(root, "c.ts", "export const c = 1\n");

        let files = vec![entry, a, b, c];
        let graph = GraphBuilder::build(&files, root, &["main.ts".into()]);

        assert_eq!(graph.nodes.get("main.ts").unwrap().depth, 0);
        assert_eq!(graph.nodes.get("a.ts").unwrap().depth, 1);
        assert_eq!(graph.nodes.get("b.ts").unwrap().depth, 1);
        assert_eq!(graph.nodes.get("c.ts").unwrap().depth, 2);

        assert_eq!(fan_out(&graph, "main.ts"), 2);
        assert_eq!(fan_in(&graph, "c.ts"), 1);
        assert_eq!(fan_in(&graph, "a.ts"), 1);
    }

    #[test]
    fn detects_barrels() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let index = mk_entry(
            root,
            "src/index.ts",
            "export { foo } from './foo'\nexport { bar } from './bar'\n",
        );
        let foo = mk_entry(root, "src/foo.ts", "export function foo() {}\n");
        let bar = mk_entry(root, "src/bar.ts", "export function bar() {}\n");

        let files = vec![index, foo, bar];
        let graph = GraphBuilder::build(&files, root, &[]);
        let barrels = GraphBuilder::detect_barrels(&graph);
        assert!(barrels.contains(&"src/index.ts".to_string()));
    }
}
