//! Python extraction rules for tree-sitter-python 0.23.
//!
//! Node kinds and field names below are taken from that grammar's
//! `node-types.json` and checked against a real parse; a query naming a kind
//! the grammar does not have fails to compile, and a spec whose query failed to
//! compile extracts nothing at all.

use super::LangSpec;
use crate::codegraph::EdgeKind;
use crate::huginn::graph::parsers::Lang;

pub static SPEC: &LangSpec = &LangSpec {
    lang: Lang::Python,
    defs: DEFS,
    calls: CALLS,
    imports: IMPORTS,
    relations: RELATIONS,
    // A method is written inside its class, so the ancestor walk finds the
    // owner with no help — including through `decorated_definition`, which
    // wraps the function but sits inside the class body all the same.
    container_kinds: &["class_definition"],
    container_name_fields: &["name"],
    // `elif` is an `if_statement` nested in the alternative, so it is counted
    // by the same kind. `boolean_operator` covers `and`/`or` and nothing else —
    // unlike the binary-operator node in the other grammars here, which also
    // covers arithmetic and so cannot be counted.
    branch_kinds: &[
        "if_statement",
        "elif_clause",
        "case_clause",
        "except_clause",
        "conditional_expression",
        "boolean_operator",
        "for_statement",
        "while_statement",
    ],
    loop_kinds: &["for_statement", "while_statement"],
    // `class Widget(Base)` states inheritance, not interface satisfaction.
    relation_kind: EdgeKind::Inherits,
    // Python states visibility by convention (a leading underscore) rather than
    // in the syntax, and a convention is not evidence.
    capitalized_names_are_exported: false,
};

/// A class attribute needs the annotated form. `count: int = 0` inside a class
/// body is a declaration; a bare `count = 0` is an assignment that happens to
/// run at class scope, and the two are the same node with and without a `type`
/// field. Requiring the annotation is what keeps arbitrary assignments out.
///
/// Module-level assignments are deliberately absent — Python has no constant,
/// and whether `NAME = value` deserves a symbol is an open question for every
/// language here, not one to answer implicitly for this one.
const DEFS: &str = r#"
(function_definition
  name: (identifier) @def.function
  parameters: (parameters) @def.params
  body: (block) @def.body)

(class_definition
  name: (identifier) @def.class
  body: (block) @def.body)

(class_definition
  body: (block
    (expression_statement
      (assignment
        left: (identifier) @def.field
        type: (type)))))
"#;

/// Each callee shape gets its own pattern rather than one `(_)`, so that a call
/// site produces exactly one match and never a duplicate edge. `attribute`
/// covers both `obj.method()` and `self.field.method()`; the qualifier text is
/// whatever the object subtree spells, which is what the resolver's receiver
/// handling expects.
const CALLS: &str = r#"
(call
  function: (identifier) @call.name)

(call
  function: (attribute
    object: (_) @call.qualifier
    attribute: (identifier) @call.name))
"#;

/// `from a.b import c` binds `c` with `a.b` as the source; `import a.b as ab`
/// binds `ab`. A dotted path is left as written and turned into a file by the
/// resolver, the same way a Rust `use` path is.
///
/// `from . import x` carries its dots in a `relative_import` node, so the
/// leading-dot spelling reaches the resolver intact and can mean "beside this
/// file" rather than "top of the project".
const IMPORTS: &str = r#"
(import_statement
  name: (dotted_name) @import.source)

(import_statement
  name: (aliased_import
    name: (dotted_name) @import.source
    alias: (identifier) @import.name))

(import_from_statement
  module_name: (_) @import.source
  name: (dotted_name (identifier) @import.name))

(import_from_statement
  module_name: (_) @import.source
  name: (aliased_import alias: (identifier) @import.name))
"#;

/// `class Widget(Base)`. A keyword argument such as `metaclass=Meta` is not a
/// base class, so only bare identifiers and dotted paths count.
const RELATIONS: &str = r#"
(class_definition
  name: (identifier) @rel.object
  superclasses: (argument_list (identifier) @rel.subject))

(class_definition
  name: (identifier) @rel.object
  superclasses: (argument_list (attribute attribute: (identifier) @rel.subject)))
"#;

#[cfg(test)]
mod tests {
    use crate::codegraph::extract::extract_file;
    use crate::codegraph::{EdgeKind, FileExtract, SymbolLabel};
    use crate::huginn::graph::parsers::Lang;

    const PATH: &str = "pkg/widget.py";

    const SRC: &str = r#"
from a.b import helper
from . import sibling
import os.path as osp

class Base:
    pass

class Widget(Base):
    count: int = 0
    untyped = 1

    def __init__(self, store):
        self.store = store

    @property
    def size(self) -> int:
        return self.count

    async def render(self, ctx):
        self.store.save(ctx)
        return helper(ctx)

def free_fn(n):
    if n > 0:
        for i in range(n):
            n -= 1
    return n
"#;

    fn extracted() -> FileExtract {
        let out = extract_file(SRC, Lang::Python, PATH);
        assert!(!out.had_errors, "fixture must parse cleanly");
        out
    }

    #[test]
    fn a_module_level_function_has_no_container() {
        let out = extracted();
        let f = out
            .symbols
            .iter()
            .find(|s| s.name == "free_fn")
            .expect("free_fn");
        assert_eq!(f.label, SymbolLabel::Function);
        assert!(f.container.is_none());
    }

    #[test]
    fn a_method_takes_its_class_as_container() {
        let out = extracted();
        let m = out
            .symbols
            .iter()
            .find(|s| s.name == "render")
            .expect("render");
        assert_eq!(m.label, SymbolLabel::Method);
        assert_eq!(m.container.as_deref(), Some("Widget"));
    }

    /// The decorator wraps the function in another node, so the walk has one
    /// more level to climb before it reaches the class.
    #[test]
    fn a_decorated_method_still_finds_its_class() {
        let out = extracted();
        let m = out.symbols.iter().find(|s| s.name == "size").expect("size");
        assert_eq!(m.label, SymbolLabel::Method);
        assert_eq!(m.container.as_deref(), Some("Widget"));
    }

    #[test]
    fn classes_are_labelled_and_do_not_contain_themselves() {
        let out = extracted();
        let c = out
            .symbols
            .iter()
            .find(|s| s.name == "Widget")
            .expect("Widget");
        assert_eq!(c.label, SymbolLabel::Class);
        assert!(c.container.is_none(), "{:?}", c.container);
    }

    /// An annotated class attribute is a declaration; a bare assignment at
    /// class scope is a statement that happens to run there. The grammar gives
    /// both the same node, distinguished only by the `type` field.
    #[test]
    fn only_annotated_class_attributes_are_fields() {
        let out = extracted();
        let count = out
            .symbols
            .iter()
            .find(|s| s.name == "count")
            .expect("count");
        assert_eq!(count.label, SymbolLabel::Field);
        assert_eq!(count.container.as_deref(), Some("Widget"));
        assert!(
            !out.symbols.iter().any(|s| s.name == "untyped"),
            "an unannotated class assignment became a symbol"
        );
    }

    #[test]
    fn a_call_records_its_enclosing_definition() {
        let out = extracted();
        let c = out
            .calls
            .iter()
            .find(|c| c.callee == "helper")
            .expect("call to helper");
        assert_eq!(c.caller.as_deref(), Some("pkg/widget.py:Widget.render"));
        assert!(c.qualifier.is_none());
    }

    /// The receiver text is whatever the object subtree spells, so a call on a
    /// field arrives as `self.store` — which is the shape the resolver's
    /// field-type handling reads a declared type from.
    #[test]
    fn a_call_on_a_field_keeps_the_whole_receiver_path() {
        let out = extracted();
        let c = out
            .calls
            .iter()
            .find(|c| c.callee == "save")
            .expect("call to save");
        assert_eq!(c.qualifier.as_deref(), Some("self.store"));
    }

    #[test]
    fn a_call_site_is_recorded_once() {
        let out = extract_file("def f():\n    helper()\n", Lang::Python, PATH);
        assert_eq!(out.calls.iter().filter(|c| c.callee == "helper").count(), 1);
    }

    #[test]
    fn from_import_binds_the_name_with_the_module_as_source() {
        let out = extracted();
        let i = out
            .imports
            .iter()
            .find(|i| i.local_name == "helper")
            .expect("helper is bound");
        assert_eq!(i.source, "a.b");
    }

    #[test]
    fn a_relative_import_keeps_its_leading_dot() {
        let out = extracted();
        let i = out
            .imports
            .iter()
            .find(|i| i.local_name == "sibling")
            .expect("sibling is bound");
        assert!(
            i.source.starts_with('.'),
            "the dot is what makes it relative: {:?}",
            i.source
        );
    }

    #[test]
    fn an_aliased_import_binds_the_alias() {
        let out = extracted();
        let i = out
            .imports
            .iter()
            .find(|i| i.local_name == "osp")
            .expect("osp is bound");
        assert_eq!(i.source, "os.path");
    }

    #[test]
    fn a_base_class_yields_an_inherits_relation() {
        let out = extracted();
        assert!(
            out.relations.iter().any(|r| r.subject == "Base"
                && r.object == "Widget"
                && r.kind == EdgeKind::Inherits),
            "{:?}",
            out.relations
        );
    }

    /// `metaclass=Meta` is not a base class.
    #[test]
    fn a_keyword_argument_is_not_a_base_class() {
        let out = extract_file(
            "class Meta: pass\nclass Widget(metaclass=Meta):\n    pass\n",
            Lang::Python,
            PATH,
        );
        assert!(
            !out.relations.iter().any(|r| r.subject == "Meta"),
            "a metaclass was read as a base: {:?}",
            out.relations
        );
    }
}
