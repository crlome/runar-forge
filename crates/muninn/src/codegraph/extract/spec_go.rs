//! Go extraction rules for tree-sitter-go 0.23.
//!
//! Node kinds and field names below are taken from that grammar's
//! `node-types.json`; a query naming a kind the grammar does not have fails to
//! compile, and a spec whose query failed to compile extracts nothing at all.

use super::LangSpec;
use crate::codegraph::EdgeKind;
use crate::huginn::graph::parsers::Lang;

pub static SPEC: &LangSpec = &LangSpec {
    lang: Lang::Go,
    defs: DEFS,
    calls: CALLS,
    imports: IMPORTS,
    relations: RELATIONS,
    // A `type_spec` encloses the method declarations of an interface, and its
    // own name is reached through the `name` field, which `container_of`
    // recognizes as self-reference and skips. A method on a *struct* is not
    // enclosed by anything — see `@def.receiver` in DEFS.
    container_kinds: &["type_spec"],
    container_name_fields: &["name"],
    // Go's switch is three distinct case kinds, and `select` a fourth. There is
    // no `&&`/`||` kind to count: the grammar gives both `binary_expression`,
    // which also covers arithmetic, so counting it would score `a + b`.
    branch_kinds: &[
        "if_statement",
        "expression_case",
        "default_case",
        "type_case",
        "communication_case",
        "for_statement",
    ],
    // `for` is Go's only loop keyword; `range` is a clause inside one.
    loop_kinds: &["for_statement"],
    // Go has no `implements`: interface satisfaction is structural and cannot
    // be read off a declaration. What RELATIONS captures is embedding, which
    // promotes the embedded type's methods onto the outer type — the one
    // inheritance-shaped relation the syntax actually states.
    relation_kind: EdgeKind::Inherits,
    capitalized_names_are_exported: true,
};

/// A method's receiver is a sibling field of the declaration rather than an
/// enclosing node, so each receiver shape is captured explicitly: plain,
/// pointer, and either of those on a generic type. Listing them beats capturing
/// the whole `parameter_list` and parsing `(s *Store[T])` back apart in Rust.
///
/// `method_elem` is an interface's method declarations — the definitions a call
/// through an interface resolves against, the same role `function_signature_item`
/// plays for a Rust trait.
///
/// `const_spec` and `var_spec` are anchored to `source_file` on purpose. Go
/// declares locals with the same nodes, and a package-level `var ErrNotFound`
/// is a symbol worth having while a `var i int` inside a loop is not.
const DEFS: &str = r#"
(function_declaration
  name: (identifier) @def.function
  parameters: (parameter_list) @def.params
  body: (block) @def.body)

(method_declaration
  receiver: (parameter_list
    (parameter_declaration type: (type_identifier) @def.receiver))
  name: (field_identifier) @def.function
  parameters: (parameter_list) @def.params
  body: (block) @def.body)

(method_declaration
  receiver: (parameter_list
    (parameter_declaration type: (pointer_type (type_identifier) @def.receiver)))
  name: (field_identifier) @def.function
  parameters: (parameter_list) @def.params
  body: (block) @def.body)

(method_declaration
  receiver: (parameter_list
    (parameter_declaration type: (generic_type type: (type_identifier) @def.receiver)))
  name: (field_identifier) @def.function
  parameters: (parameter_list) @def.params
  body: (block) @def.body)

(method_declaration
  receiver: (parameter_list
    (parameter_declaration
      type: (pointer_type (generic_type type: (type_identifier) @def.receiver))))
  name: (field_identifier) @def.function
  parameters: (parameter_list) @def.params
  body: (block) @def.body)

(method_elem
  name: (field_identifier) @def.function
  parameters: (parameter_list) @def.params)

(field_declaration name: (field_identifier) @def.field)

(type_spec
  name: (type_identifier) @def.struct
  type: (struct_type))

(type_spec
  name: (type_identifier) @def.interface
  type: (interface_type))

(source_file
  (const_declaration (const_spec name: (identifier) @def.const)))

(source_file
  (var_declaration (var_spec name: (identifier) @def.const)))
"#;

/// Each callee shape gets its own pattern rather than one `(_)`, so that a call
/// site produces exactly one match and never a duplicate edge.
///
/// **Known limitation, deliberate.** `X[Y](Z)` never appears here. The grammar
/// parses every one of them — a generic instantiation `Map[int](xs)` and an
/// ordinary call through a map of functions `handlers[key](w)` alike — as a
/// `type_conversion_expression`, because Go's syntax genuinely cannot tell
/// those from a conversion to the type `X[Y]` without type information. Reading
/// that node as a call would invent edges for real conversions. Consistent with
/// the rest of the resolver: an edge is evidence or it is absent, and the
/// unmatched call is counted rather than guessed.
const CALLS: &str = r#"
(call_expression
  function: (identifier) @call.name)

(call_expression
  function: (selector_expression
    operand: (_) @call.qualifier
    field: (field_identifier) @call.name))
"#;

/// The import path is the source. Quotes are stripped by the walker, which also
/// binds the trailing path segment when no alias is given — which is exactly
/// Go's rule: `import "net/http"` binds `http`.
const IMPORTS: &str = r#"
(import_spec
  path: (_) @import.source)

(import_spec
  name: (package_identifier) @import.name
  path: (_) @import.source)
"#;

/// An embedded field has a type and no name, which is what `!name` asserts;
/// without it every ordinary `count int` field would read as an embedding.
/// `type_elem` is the same idea inside an interface.
const RELATIONS: &str = r#"
(type_spec
  name: (type_identifier) @rel.object
  type: (struct_type
    (field_declaration_list
      (field_declaration !name type: (type_identifier) @rel.subject))))

(type_spec
  name: (type_identifier) @rel.object
  type: (struct_type
    (field_declaration_list
      (field_declaration !name
        type: (pointer_type (type_identifier) @rel.subject)))))

(type_spec
  name: (type_identifier) @rel.object
  type: (interface_type
    (type_elem (type_identifier) @rel.subject)))
"#;

#[cfg(test)]
mod tests {
    use crate::codegraph::extract::extract_file;
    use crate::codegraph::{EdgeKind, FileExtract, SymbolLabel};
    use crate::huginn::graph::parsers::Lang;

    const PATH: &str = "internal/server/server.go";

    const SRC: &str = r#"
package server

import (
	"net/http"
	stdlog "log"
)

const DefaultPort = 8080

var ErrClosed = errors.New("closed")

type Handler interface {
	Serve(w http.ResponseWriter)
}

type Base struct {
	id int
}

type Server struct {
	Base
	port  int
	inner *http.Client
}

func (s *Server) Handle(w http.ResponseWriter) {
	helper(s.port)
	stdlog.Print("x")
}

func (b Base) ID() int {
	return b.id
}

func helper(n int) int {
	if n > 0 {
		for i := 0; i < n; i++ {
			n--
		}
	}
	return n
}
"#;

    fn extracted() -> FileExtract {
        let out = extract_file(SRC, Lang::Go, PATH);
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

    /// The receiver is a sibling of the declaration, not an ancestor, so this
    /// is the case the generic ancestor walk cannot reach on its own.
    #[test]
    fn pointer_receiver_becomes_the_container() {
        let out = extracted();
        let m = out
            .symbols
            .iter()
            .find(|s| s.name == "Handle")
            .expect("Handle");
        assert_eq!(m.label, SymbolLabel::Method);
        assert_eq!(m.container.as_deref(), Some("Server"));
    }

    #[test]
    fn value_receiver_becomes_the_container() {
        let out = extracted();
        let m = out.symbols.iter().find(|s| s.name == "ID").expect("ID");
        assert_eq!(m.label, SymbolLabel::Method);
        assert_eq!(m.container.as_deref(), Some("Base"));
    }

    #[test]
    fn generic_receivers_drop_their_type_arguments() {
        let out = extract_file(
            "package p\ntype Store[T any] struct{ v T }\nfunc (s *Store[T]) Get() T { return s.v }\n",
            Lang::Go,
            PATH,
        );
        let m = out.symbols.iter().find(|s| s.name == "Get").expect("Get");
        assert_eq!(
            m.container.as_deref(),
            Some("Store"),
            "the container must match the `Store` definition, not `Store[T]`"
        );
    }

    #[test]
    fn interface_methods_take_the_interface_as_container() {
        let out = extracted();
        let m = out
            .symbols
            .iter()
            .find(|s| s.name == "Serve")
            .expect("Serve");
        assert_eq!(m.label, SymbolLabel::Method);
        assert_eq!(m.container.as_deref(), Some("Handler"));
    }

    #[test]
    fn struct_and_interface_types_are_labelled_apart() {
        let out = extracted();
        assert!(out
            .symbols
            .iter()
            .any(|s| s.name == "Server" && s.label == SymbolLabel::Struct));
        assert!(out
            .symbols
            .iter()
            .any(|s| s.name == "Handler" && s.label == SymbolLabel::Interface));
    }

    #[test]
    fn exported_names_are_the_capitalised_ones() {
        let out = extracted();
        let handle = out.symbols.iter().find(|s| s.name == "Handle").unwrap();
        let helper = out.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert!(handle.exported);
        assert!(!helper.exported);
    }

    #[test]
    fn package_level_const_and_var_are_symbols() {
        let out = extracted();
        assert!(out
            .symbols
            .iter()
            .any(|s| s.name == "DefaultPort" && s.label == SymbolLabel::Const));
        assert!(out
            .symbols
            .iter()
            .any(|s| s.name == "ErrClosed" && s.label == SymbolLabel::Const));
    }

    /// Go writes locals with the same nodes as package-level declarations, and
    /// there are vastly more of them. Anchoring to `source_file` is what keeps
    /// the graph from filling with loop counters.
    #[test]
    fn locals_are_not_symbols() {
        let out = extract_file(
            "package p\nfunc f() {\n\tvar scratch int\n\tconst inner = 2\n\t_ = scratch + inner\n}\n",
            Lang::Go,
            PATH,
        );
        assert!(!out.symbols.iter().any(|s| s.name == "scratch"));
        assert!(!out.symbols.iter().any(|s| s.name == "inner"));
    }

    #[test]
    fn a_call_records_its_enclosing_definition() {
        let out = extracted();
        let c = out
            .calls
            .iter()
            .find(|c| c.callee == "helper")
            .expect("call to helper");
        assert_eq!(
            c.caller.as_deref(),
            Some("internal/server/server.go:Server.Handle")
        );
        assert!(c.qualifier.is_none());
    }

    #[test]
    fn selector_calls_keep_their_receiver_as_the_qualifier() {
        let out = extracted();
        let p = out
            .calls
            .iter()
            .find(|c| c.callee == "Print")
            .expect("stdlog.Print");
        assert_eq!(p.qualifier.as_deref(), Some("stdlog"));
    }

    /// Pins the limitation documented on CALLS so it is a known cost rather
    /// than a surprise. The grammar reads `X[Y](Z)` as a conversion to the type
    /// `X[Y]`, and it does so whether the source meant a generic call or a call
    /// through a map of functions — the two are indistinguishable without type
    /// information. Recording them as calls would invent edges for genuine
    /// conversions, so no edge is produced.
    ///
    /// If this ever starts failing, the grammar learned to tell them apart and
    /// the patterns should be added back.
    #[test]
    fn bracketed_call_syntax_is_not_read_as_a_call() {
        let out = extract_file(
            "package p\nfunc f() { Map[int](xs); handlers[key](w) }\n",
            Lang::Go,
            PATH,
        );
        assert!(
            !out.calls.iter().any(|c| c.callee == "Map"),
            "ambiguous conversion syntax was recorded as a call: {:?}",
            out.calls
        );
    }

    #[test]
    fn struct_fields_are_labelled_and_take_their_type_as_container() {
        let out = extracted();
        let port = out
            .symbols
            .iter()
            .find(|s| s.name == "port")
            .expect("field port");
        assert_eq!(port.label, SymbolLabel::Field);
        assert_eq!(port.container.as_deref(), Some("Server"));
    }

    /// An embedded field has no name, so it produces a relation and never a
    /// Field symbol — the two queries must not both claim it.
    #[test]
    fn an_embedded_field_is_not_a_field_symbol() {
        let out = extracted();
        assert!(
            !out.symbols
                .iter()
                .any(|s| s.name == "Base" && s.label == SymbolLabel::Field),
            "embedding was also recorded as a field"
        );
    }

    #[test]
    fn a_call_site_is_recorded_once() {
        let out = extract_file("package p\nfunc f() { helper() }\n", Lang::Go, PATH);
        assert_eq!(out.calls.iter().filter(|c| c.callee == "helper").count(), 1);
    }

    #[test]
    fn plain_import_binds_the_last_path_segment() {
        let out = extracted();
        let i = out
            .imports
            .iter()
            .find(|i| i.local_name == "http")
            .expect("net/http binds http");
        assert_eq!(i.source, "net/http");
    }

    #[test]
    fn aliased_import_binds_the_alias() {
        let out = extracted();
        let i = out
            .imports
            .iter()
            .find(|i| i.local_name == "stdlog")
            .expect("stdlog alias");
        assert_eq!(i.source, "log");
    }

    #[test]
    fn embedding_yields_an_inherits_relation() {
        let out = extracted();
        assert!(
            out.relations.iter().any(|r| r.subject == "Base"
                && r.object == "Server"
                && r.kind == EdgeKind::Inherits),
            "embedded Base should be inherited by Server: {:?}",
            out.relations
        );
    }

    /// The distinction is `!name`: an embedded field has a type and no name,
    /// while `port int` has both. Without it every field would read as a
    /// base type.
    #[test]
    fn named_fields_are_not_embeddings() {
        let out = extracted();
        assert!(
            !out.relations.iter().any(|r| r.subject == "int"),
            "a named field was mistaken for an embedding: {:?}",
            out.relations
        );
    }
}
