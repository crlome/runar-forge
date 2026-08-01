//! Rust extraction rules for tree-sitter-rust 0.23.
//!
//! Node kinds and field names below are taken from that grammar's
//! `node-types.json`; a query naming a kind the grammar does not have fails to
//! compile, and a spec whose query failed to compile extracts nothing at all.

use super::LangSpec;
use crate::codegraph::EdgeKind;
use crate::huginn::graph::parsers::Lang;

pub static SPEC: &LangSpec = &LangSpec {
    lang: Lang::Rust,
    defs: DEFS,
    calls: CALLS,
    imports: IMPORTS,
    relations: RELATIONS,
    // `mod_item` is deliberately absent: it introduces a namespace, not a
    // receiver, and the walker turns every function with a container into a
    // Method.
    container_kinds: &["impl_item", "trait_item"],
    // `impl_item` has no `name`; its own identity is the implementing type, so
    // `type` is tried first and `trait_item` falls through to `name`.
    container_name_fields: &["type", "name"],
    // `&&` / `||` are absent because the grammar gives them no kind of their
    // own — they are `binary_expression`, which also covers every arithmetic
    // and comparison operator, so counting that kind would score `a + b`.
    branch_kinds: &[
        "if_expression",
        "match_arm",
        "while_expression",
        "for_expression",
        "loop_expression",
        "try_expression",
    ],
    loop_kinds: &["for_expression", "while_expression", "loop_expression"],
    relation_kind: EdgeKind::Implements,
    capitalized_names_are_exported: false,
};

/// `function_signature_item` covers a trait's method declarations, which are
/// the definitions an implementing call resolves against.
const DEFS: &str = r#"
(function_item
  name: (identifier) @def.function
  parameters: (parameters) @def.params
  body: (block) @def.body)

(function_signature_item
  name: (identifier) @def.function
  parameters: (parameters) @def.params)

(struct_item name: (type_identifier) @def.struct)
(union_item name: (type_identifier) @def.struct)
(enum_item name: (type_identifier) @def.enum)
(trait_item name: (type_identifier) @def.trait)
(type_item name: (type_identifier) @def.type)
(const_item name: (identifier) @def.const)
(static_item name: (identifier) @def.const)
(mod_item name: (identifier) @def.module)
"#;

/// Each callee shape gets its own pattern rather than one `(_)`, so that a call
/// site produces exactly one match and never a duplicate edge.
const CALLS: &str = r#"
(call_expression
  function: (identifier) @call.name)

(call_expression
  function: (scoped_identifier
    path: (_) @call.qualifier
    name: (identifier) @call.name))

(call_expression
  function: (field_expression
    value: (_) @call.qualifier
    field: (field_identifier) @call.name))

(call_expression
  function: (generic_function
    function: (identifier) @call.name))

(call_expression
  function: (generic_function
    function: (scoped_identifier
      path: (_) @call.qualifier
      name: (identifier) @call.name)))

(call_expression
  function: (generic_function
    function: (field_expression
      value: (_) @call.qualifier
      field: (field_identifier) @call.name)))

(macro_invocation
  macro: (identifier) @call.name)

(macro_invocation
  macro: (scoped_identifier
    path: (_) @call.qualifier
    name: (identifier) @call.name))
"#;

/// The whole use-path is the source. Truncating it to its first segment, as the
/// older import parser did, collapses every `crate::…` import onto one node and
/// leaves the resolver nothing to match a definition against.
///
/// The argument kinds are listed rather than matched with `(_)` because
/// `use_as_clause` and `scoped_use_list` have their own patterns below, and a
/// wildcard would match them a second time with the alias or brace list glued
/// into the path text.
const IMPORTS: &str = r#"
(use_declaration
  argument: [
    (identifier)
    (scoped_identifier)
    (use_wildcard)
    (crate)
    (self)
    (super)
    (metavariable)
  ] @import.source)

(use_declaration
  argument: (use_as_clause
    path: (_) @import.source
    alias: (identifier) @import.name))

(use_declaration
  argument: (scoped_use_list
    path: (_) @import.source
    list: (use_list (identifier) @import.name)))

(use_declaration
  argument: (scoped_use_list
    path: (_) @import.source
    list: (use_list (use_as_clause alias: (identifier) @import.name))))

(use_declaration
  argument: (scoped_use_list
    path: (_) @import.source
    list: (use_list (self))))
"#;

/// `trait` is an optional field, so an inherent `impl Foo` never matches.
const RELATIONS: &str = r#"
(impl_item
  trait: (_) @rel.subject
  type: (_) @rel.object)
"#;

#[cfg(test)]
mod tests {
    use crate::codegraph::extract::extract_file;
    use crate::codegraph::{EdgeKind, FileExtract, SymbolLabel};
    use crate::huginn::graph::parsers::Lang;

    const PATH: &str = "src/foo.rs";

    const SRC: &str = r#"
use crate::a::b::C;
use std::fmt::Display;

pub struct Foo {
    n: u32,
}

pub trait Greeter {
    fn greet(&self) -> String;
}

impl Foo {
    pub fn bump(&mut self) -> u32 {
        helper(self.n)
    }
}

impl Display for Foo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.n)
    }
}

fn helper(n: u32) -> u32 {
    n + 1
}
"#;

    fn extracted() -> FileExtract {
        let out = extract_file(SRC, Lang::Rust, PATH);
        assert!(!out.had_errors, "fixture must parse cleanly");
        out
    }

    #[test]
    fn free_function_is_a_function_with_no_container() {
        let out = extracted();
        let f = out
            .symbols
            .iter()
            .find(|s| s.name == "helper")
            .expect("helper");
        assert_eq!(f.label, SymbolLabel::Function);
        assert!(f.container.is_none());
    }

    #[test]
    fn method_in_an_impl_takes_the_implementing_type_as_container() {
        let out = extracted();
        let m = out.symbols.iter().find(|s| s.name == "bump").expect("bump");
        assert_eq!(m.label, SymbolLabel::Method);
        assert_eq!(m.container.as_deref(), Some("Foo"));
        assert!(m.exported);
    }

    #[test]
    fn trait_impl_method_takes_the_type_and_not_the_trait() {
        let out = extracted();
        let m = out.symbols.iter().find(|s| s.name == "fmt").expect("fmt");
        assert_eq!(m.container.as_deref(), Some("Foo"));
    }

    #[test]
    fn types_are_labelled_by_their_item_kind() {
        let out = extracted();
        assert!(out
            .symbols
            .iter()
            .any(|s| s.name == "Foo" && s.label == SymbolLabel::Struct));
        assert!(out
            .symbols
            .iter()
            .any(|s| s.name == "Greeter" && s.label == SymbolLabel::Trait));
    }

    #[test]
    fn a_call_records_its_enclosing_definition() {
        let out = extracted();
        let c = out
            .calls
            .iter()
            .find(|c| c.callee == "helper")
            .expect("call to helper");
        assert_eq!(c.caller.as_deref(), Some("src/foo.rs:Foo.bump"));
        assert!(c.qualifier.is_none());
    }

    #[test]
    fn method_call_keeps_its_receiver_as_the_qualifier() {
        let out = extract_file(
            "fn go(a: A) { a.run(); crate::util::spin(); }",
            Lang::Rust,
            PATH,
        );
        let run = out.calls.iter().find(|c| c.callee == "run").expect("run");
        assert_eq!(run.qualifier.as_deref(), Some("a"));
        let spin = out.calls.iter().find(|c| c.callee == "spin").expect("spin");
        assert_eq!(spin.qualifier.as_deref(), Some("crate::util"));
    }

    #[test]
    fn use_path_is_captured_whole() {
        let out = extracted();
        let i = out
            .imports
            .iter()
            .find(|i| i.local_name == "C")
            .expect("import of C");
        assert_eq!(i.source, "crate::a::b::C");
    }

    #[test]
    fn use_list_binds_each_name_to_the_shared_prefix() {
        let out = extract_file("use crate::a::{B, c as d};", Lang::Rust, PATH);
        let b = out
            .imports
            .iter()
            .find(|i| i.local_name == "B")
            .expect("B is bound");
        assert_eq!(b.source, "crate::a");
        let d = out
            .imports
            .iter()
            .find(|i| i.local_name == "d")
            .expect("alias d is bound");
        assert_eq!(d.source, "crate::a");
    }

    #[test]
    fn aliased_use_binds_the_alias() {
        let out = extract_file("use std::io::Result as IoResult;", Lang::Rust, PATH);
        let i = out
            .imports
            .iter()
            .find(|i| i.local_name == "IoResult")
            .expect("IoResult");
        assert_eq!(i.source, "std::io::Result");
    }

    #[test]
    fn trait_impl_yields_an_implements_relation() {
        let out = extracted();
        assert!(out.relations.iter().any(|r| r.subject == "Display"
            && r.object == "Foo"
            && r.kind == EdgeKind::Implements));
    }

    #[test]
    fn inherent_impl_yields_no_relation() {
        let out = extract_file("struct Foo; impl Foo { fn a(&self) {} }", Lang::Rust, PATH);
        assert!(out.relations.is_empty());
    }

    #[test]
    fn a_call_site_is_recorded_once() {
        let out = extract_file("fn go() { helper(); }", Lang::Rust, PATH);
        assert_eq!(out.calls.iter().filter(|c| c.callee == "helper").count(), 1);
    }
}
