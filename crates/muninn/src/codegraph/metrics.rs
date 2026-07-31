//! Per-symbol complexity signals.
//!
//! All three body-derived numbers come out of a single depth-first pass, so
//! measuring a function costs one traversal rather than one per metric.

use tree_sitter::Node;

use super::extract::LangSpec;
use super::SymbolMetrics;

/// Generated and vendored files can nest thousands of levels deep. Traversal
/// stops here and reports what it has, so no single pathological file can make
/// a whole-repository crawl look like a hang.
const NODE_BUDGET: usize = 50_000;

/// Measure one definition. `body` absent means "not measured": the body-derived
/// fields stay at zero so a declaration without a body is distinguishable from
/// a trivial one, which a cyclomatic floor of 1 would hide. `params` is
/// independent of that — a bodyless declaration still has a real parameter
/// list, and it is still counted.
pub fn measure(body: Option<Node<'_>>, params: Option<Node<'_>>, spec: &LangSpec) -> SymbolMetrics {
    let param_count = params.map_or(0, |p| {
        u32::try_from(p.named_child_count()).unwrap_or(u32::MAX)
    });

    let Some(body) = body else {
        return SymbolMetrics {
            param_count,
            ..SymbolMetrics::default()
        };
    };

    let (complexity, cognitive, loop_depth) = walk(body, spec, NODE_BUDGET);
    SymbolMetrics {
        complexity,
        cognitive,
        loop_depth,
        param_count,
    }
}

/// One pass over the body subtree, yielding `(complexity, cognitive, loop_depth)`.
///
/// `budget` is a parameter rather than a constant so the truncation path is
/// testable without constructing a pathological tree.
fn walk(body: Node<'_>, spec: &LangSpec, budget: usize) -> (u32, u32, u32) {
    let mut complexity: u32 = 1;
    let mut cognitive: u32 = 0;
    let mut loop_depth: u32 = 0;

    let mut visited: usize = 0;
    let mut cursor = body.walk();
    // (node, enclosing branch/loop nesting, enclosing loop nesting)
    let mut stack = vec![(body, 0u32, 0u32)];

    while let Some((node, nesting, loops)) = stack.pop() {
        if visited >= budget {
            break;
        }
        visited += 1;

        let kind = node.kind();
        let is_branch = spec.branch_kinds.contains(&kind);
        let is_loop = spec.loop_kinds.contains(&kind);

        if is_branch {
            complexity = complexity.saturating_add(1);
        }

        // An approximation of cognitive complexity, not the published
        // specification: the only property relied on downstream is that nesting
        // costs more than sequence, so a construct at depth d is charged 1 + d.
        let (child_nesting, child_loops) = if is_branch || is_loop {
            cognitive = cognitive.saturating_add(1).saturating_add(nesting);
            (
                nesting.saturating_add(1),
                if is_loop {
                    loops.saturating_add(1)
                } else {
                    loops
                },
            )
        } else {
            (nesting, loops)
        };

        if is_loop {
            loop_depth = loop_depth.max(child_loops);
        }

        // Anonymous children are walked too: a spec may count an operator token
        // such as `&&` as a branch.
        for child in node.children(&mut cursor) {
            stack.push((child, child_nesting, child_loops));
        }
    }

    (complexity, cognitive, loop_depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegraph::extract::spec_for;
    use crate::huginn::graph::parsers::{make_parser, Lang};
    use tree_sitter::Tree;

    fn parse(src: &str) -> Tree {
        make_parser(Lang::Rust).parse(src, None).expect("parse")
    }

    /// Pre-order search, so a source with several functions yields the first.
    fn find_kind<'a>(root: Node<'a>, kind: &str) -> Option<Node<'a>> {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == kind {
                return Some(node);
            }
            for i in (0..node.child_count()).rev() {
                if let Some(child) = node.child(i) {
                    stack.push(child);
                }
            }
        }
        None
    }

    fn metrics_of(src: &str) -> SymbolMetrics {
        let tree = parse(src);
        let func = find_kind(tree.root_node(), "function_item").expect("function_item");
        measure(
            func.child_by_field_name("body"),
            func.child_by_field_name("parameters"),
            spec_for(Lang::Rust),
        )
    }

    /// The numbers asserted below are only meaningful if the Rust spec names
    /// these kinds; failing here points at the spec rather than at the walk.
    #[test]
    fn rust_spec_names_the_kinds_these_tests_rely_on() {
        let spec = spec_for(Lang::Rust);
        assert!(
            spec.branch_kinds.contains(&"if_expression"),
            "branch_kinds: {:?}",
            spec.branch_kinds
        );
        assert!(
            spec.loop_kinds.contains(&"for_expression"),
            "loop_kinds: {:?}",
            spec.loop_kinds
        );
    }

    #[test]
    fn straight_line_body_is_complexity_one() {
        let m = metrics_of("fn straight(a: u32) -> u32 { let b = a + 1; let c = b * 2; c }");
        assert_eq!(m.complexity, 1);
        assert_eq!(m.cognitive, 0);
        assert_eq!(m.loop_depth, 0);
    }

    #[test]
    fn each_branch_adds_one_to_complexity() {
        let m = metrics_of(
            r#"
fn two(a: bool, b: bool) {
    if a { work(); }
    if b { work(); }
}
"#,
        );
        assert_eq!(m.complexity, 3);
    }

    #[test]
    fn loop_depth_counts_only_loop_nesting() {
        let flat = metrics_of("fn flat() { for i in 0..3 { work(i); } }");
        assert_eq!(flat.loop_depth, 1);

        let nested = metrics_of(
            r#"
fn nested() {
    for i in 0..3 {
        for j in 0..3 {
            work(i, j);
        }
    }
}
"#,
        );
        assert_eq!(nested.loop_depth, 2);
    }

    #[test]
    fn nesting_costs_more_than_sequence() {
        let flat = metrics_of(
            r#"
fn flat(a: bool, b: bool) {
    if a { work(); }
    if b { work(); }
}
"#,
        );
        let nested = metrics_of(
            r#"
fn nested(a: bool, b: bool) {
    if a {
        if b { work(); }
    }
}
"#,
        );
        assert_eq!(flat.complexity, nested.complexity);
        assert!(
            nested.cognitive > flat.cognitive,
            "flat {} vs nested {}",
            flat.cognitive,
            nested.cognitive
        );
    }

    #[test]
    fn param_count_ignores_punctuation() {
        let m = metrics_of("fn three(a: u32, b: u32, c: u32) { let _ = (a, b, c); }");
        assert_eq!(m.param_count, 3);
    }

    #[test]
    fn param_count_survives_a_bodyless_declaration() {
        let tree = parse("trait T { fn f(&self, a: u32, b: u32); }");
        let func = find_kind(tree.root_node(), "function_signature_item").expect("signature");
        let m = measure(
            func.child_by_field_name("body"),
            func.child_by_field_name("parameters"),
            spec_for(Lang::Rust),
        );
        assert_eq!(m.complexity, 0);
        assert_eq!(m.param_count, 3);
    }

    #[test]
    fn absent_body_and_params_measure_nothing() {
        assert_eq!(
            measure(None, None, spec_for(Lang::Rust)),
            SymbolMetrics::default()
        );
    }

    #[test]
    fn budget_exhaustion_returns_partial_counts() {
        let tree = parse("fn nested() { for i in 0..3 { for j in 0..3 { work(i, j); } } }");
        let func = find_kind(tree.root_node(), "function_item").expect("function_item");
        let body = func.child_by_field_name("body").expect("body");

        let (_, _, full_depth) = walk(body, spec_for(Lang::Rust), NODE_BUDGET);
        assert_eq!(full_depth, 2);

        let (complexity, cognitive, loop_depth) = walk(body, spec_for(Lang::Rust), 1);
        assert_eq!((complexity, cognitive, loop_depth), (1, 0, 0));
    }
}
