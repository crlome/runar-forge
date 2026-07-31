//! TypeScript, TSX and JavaScript extraction rules.
//!
//! The three grammars agree on every node kind JavaScript has, so the bulk of
//! each query is shared. They cannot share it *wholesale*: `interface_declaration`,
//! `type_alias_declaration`, `enum_declaration`, `abstract_class_declaration`,
//! `extends_clause` and `public_field_definition` exist only in the TypeScript
//! grammars, and a query naming a node type its grammar does not know fails to
//! compile — which the walker turns into "this language extracts nothing",
//! silently. The shared text therefore lives in a macro and each family splices
//! its own tail onto it, so the JavaScript query never mentions a TypeScript
//! node.
//!
//! Relation direction: `@rel.subject` is the type that *declares* the relation
//! (the subclass / sub-interface) and `@rel.object` is the type being extended.

use super::LangSpec;
use crate::codegraph::EdgeKind;
use crate::huginn::graph::parsers::Lang;

/// Patterns valid against all three grammars.
///
/// A macro rather than a `const` because `concat!` splices literals, not
/// constants, and each family needs this text plus its own tail.
macro_rules! defs_shared {
    () => {
        r#"
(function_declaration
  name: (identifier) @def.function
  parameters: (formal_parameters) @def.params
  body: (statement_block) @def.body)

(generator_function_declaration
  name: (identifier) @def.function
  parameters: (formal_parameters) @def.params
  body: (statement_block) @def.body)

(method_definition
  name: [(property_identifier) (private_property_identifier)] @def.function
  parameters: (formal_parameters) @def.params
  body: (statement_block) @def.body)

(class_declaration
  name: (_) @def.class)

; `const f = () => {}` and `const f = function () {}` are how most modern code
; declares a function; without these two patterns the majority of a TS codebase
; has no definitions at all.
(lexical_declaration
  (variable_declarator
    name: (identifier) @def.function
    value: [
      (arrow_function
        parameters: (formal_parameters) @def.params
        body: (_) @def.body)
      (function_expression
        parameters: (formal_parameters) @def.params
        body: (_) @def.body)
    ]))

; `const f = x => x`: the unparenthesised single parameter is a different field
; (`parameter`, not `parameters`), so it needs its own pattern.
(lexical_declaration
  (variable_declarator
    name: (identifier) @def.function
    value: (arrow_function
      parameter: (identifier)
      body: (_) @def.body)))
"#
    };
}

const DEFS_TS: &str = concat!(
    defs_shared!(),
    r#"
(abstract_class_declaration
  name: (type_identifier) @def.class)

(interface_declaration
  name: (type_identifier) @def.interface)

(type_alias_declaration
  name: (type_identifier) @def.type)

(enum_declaration
  name: (identifier) @def.enum)

(public_field_definition
  name: [(property_identifier) (private_property_identifier)] @def.function
  value: [
    (arrow_function
      parameters: (formal_parameters) @def.params
      body: (_) @def.body)
    (function_expression
      parameters: (formal_parameters) @def.params
      body: (_) @def.body)
  ])
"#
);

const DEFS_JS: &str = concat!(
    defs_shared!(),
    r#"
; The JavaScript grammar spells the class-field name field `property`, not
; `name` as the TypeScript grammars do.
(field_definition
  property: [(property_identifier) (private_property_identifier)] @def.function
  value: [
    (arrow_function
      parameters: (formal_parameters) @def.params
      body: (_) @def.body)
    (function_expression
      parameters: (formal_parameters) @def.params
      body: (_) @def.body)
  ])
"#
);

const CALLS: &str = r#"
(call_expression
  function: (identifier) @call.name)

(call_expression
  function: (member_expression
    object: (_) @call.qualifier
    property: (property_identifier) @call.name))

(new_expression
  constructor: (identifier) @call.name)

(new_expression
  constructor: (member_expression
    object: (_) @call.qualifier
    property: (property_identifier) @call.name))
"#;

/// What a call site can reference is the *local* binding, so an aliased
/// specifier contributes its alias and a plain one contributes its name; the
/// `!alias` guard keeps `import { a as b }` from also binding `a`.
const IMPORTS: &str = r#"
(import_statement
  (import_clause
    (named_imports
      (import_specifier
        name: (identifier) @import.name
        !alias)))
  source: (string (string_fragment) @import.source))

(import_statement
  (import_clause
    (named_imports
      (import_specifier
        alias: (identifier) @import.name)))
  source: (string (string_fragment) @import.source))

(import_statement
  (import_clause (identifier) @import.name)
  source: (string (string_fragment) @import.source))

(import_statement
  (import_clause (namespace_import (identifier) @import.name))
  source: (string (string_fragment) @import.source))

(variable_declarator
  name: (identifier) @import.name
  value: (call_expression
    function: (identifier) @_require
    arguments: (arguments (string (string_fragment) @import.source)))
  (#eq? @_require "require"))
"#;

const RELATIONS_TS: &str = r#"
(class_declaration
  name: (type_identifier) @rel.subject
  (class_heritage
    (extends_clause
      value: (_) @rel.object)))

(abstract_class_declaration
  name: (type_identifier) @rel.subject
  (class_heritage
    (extends_clause
      value: (_) @rel.object)))

(interface_declaration
  name: (type_identifier) @rel.subject
  (extends_type_clause
    type: (_) @rel.object))
"#;

/// In the JavaScript grammar `class_heritage` holds the superclass expression
/// directly; there is no `extends_clause` node to unwrap.
const RELATIONS_JS: &str = r#"
(class_declaration
  name: (identifier) @rel.subject
  (class_heritage (_) @rel.object))
"#;

/// Listed for all three families: a kind the grammar lacks simply never
/// matches, and these are compared as strings rather than compiled.
const CONTAINER_KINDS: &[&str] = &[
    "class_declaration",
    "abstract_class_declaration",
    "class",
    "interface_declaration",
    "enum_declaration",
];

const CONTAINER_NAME_FIELDS: &[&str] = &["name"];

/// `&&` and `||` are the token kinds themselves: these grammars have no
/// distinct logical-expression node, only `binary_expression`, which also
/// covers arithmetic and comparison and so would count every `+` as a branch.
const BRANCH_KINDS: &[&str] = &[
    "if_statement",
    "ternary_expression",
    "switch_case",
    "&&",
    "||",
];

const LOOP_KINDS: &[&str] = &[
    "for_statement",
    "for_in_statement",
    "while_statement",
    "do_statement",
];

const fn spec(lang: Lang, defs: &'static str, relations: &'static str) -> LangSpec {
    LangSpec {
        lang,
        defs,
        calls: CALLS,
        imports: IMPORTS,
        relations,
        container_kinds: CONTAINER_KINDS,
        container_name_fields: CONTAINER_NAME_FIELDS,
        branch_kinds: BRANCH_KINDS,
        loop_kinds: LOOP_KINDS,
        relation_kind: EdgeKind::Inherits,
    }
}

static TYPESCRIPT: LangSpec = spec(Lang::TypeScript, DEFS_TS, RELATIONS_TS);
static TSX: LangSpec = spec(Lang::Tsx, DEFS_TS, RELATIONS_TS);
static JAVASCRIPT: LangSpec = spec(Lang::JavaScript, DEFS_JS, RELATIONS_JS);

/// Anything not in the JS family falls back to the TypeScript spec, which the
/// caller uses as a placeholder until that language has rules of its own.
pub fn spec_for(lang: Lang) -> &'static LangSpec {
    match lang {
        Lang::Tsx => &TSX,
        Lang::JavaScript => &JAVASCRIPT,
        _ => &TYPESCRIPT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegraph::extract::extract_file;
    use crate::codegraph::{FileExtract, RawSymbol, SymbolLabel};

    const TS_SRC: &str = r#"
import { helper, other as alt } from './m';

function internal(): number {
  return 1;
}

export function build(a: number): string {
  return helper(a);
}

export const render = (n: number) => {
  return internal();
};

export class Widget extends Base {
  draw(): void {
    this.paint();
  }
}
"#;

    const JS_SRC: &str = r#"
import { helper } from './m';
const fs = require('node:fs');

export function build(a) {
  return helper(a);
}

export const render = (n) => n + 1;

export class Widget extends Base {
  draw() {
    this.paint();
  }
}
"#;

    fn sym<'a>(ex: &'a FileExtract, name: &str) -> &'a RawSymbol {
        ex.symbols
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no symbol {name} in {:?}", ex.symbols))
    }

    #[test]
    fn spec_reports_the_language_it_was_asked_for() {
        assert_eq!(spec_for(Lang::TypeScript).lang, Lang::TypeScript);
        assert_eq!(spec_for(Lang::Tsx).lang, Lang::Tsx);
        assert_eq!(spec_for(Lang::JavaScript).lang, Lang::JavaScript);
    }

    #[test]
    fn typescript_source_parses_without_errors() {
        let ex = extract_file(TS_SRC, Lang::TypeScript, "f.ts");
        assert!(!ex.had_errors, "TS_SRC should parse cleanly");
    }

    #[test]
    fn exported_function_is_found_and_marked_exported() {
        let ex = extract_file(TS_SRC, Lang::TypeScript, "f.ts");
        let build = sym(&ex, "build");
        assert_eq!(build.label, SymbolLabel::Function);
        assert_eq!(build.container, None);
        assert!(build.exported);
    }

    #[test]
    fn unexported_function_is_not_marked_exported() {
        let ex = extract_file(TS_SRC, Lang::TypeScript, "f.ts");
        assert!(!sym(&ex, "internal").exported);
    }

    #[test]
    fn class_method_is_a_method_of_its_class() {
        let ex = extract_file(TS_SRC, Lang::TypeScript, "f.ts");
        let draw = sym(&ex, "draw");
        assert_eq!(draw.label, SymbolLabel::Method);
        assert_eq!(draw.container.as_deref(), Some("Widget"));
        assert_eq!(sym(&ex, "Widget").label, SymbolLabel::Class);
    }

    #[test]
    fn arrow_function_assigned_to_a_const_is_a_function() {
        let ex = extract_file(TS_SRC, Lang::TypeScript, "f.ts");
        let render = sym(&ex, "render");
        assert_eq!(render.label, SymbolLabel::Function);
        assert_eq!(render.container, None);
        assert!(render.exported);
    }

    #[test]
    fn single_parameter_arrow_without_parentheses_is_still_a_function() {
        let ex = extract_file("const twice = x => x * 2;\n", Lang::TypeScript, "f.ts");
        assert_eq!(sym(&ex, "twice").label, SymbolLabel::Function);
    }

    #[test]
    fn typescript_only_declarations_are_extracted() {
        let src = "export interface Shape { area(): number }\n\
                   export type Id = string;\n\
                   export enum Color { Red }\n";
        let ex = extract_file(src, Lang::TypeScript, "f.ts");
        assert_eq!(sym(&ex, "Shape").label, SymbolLabel::Interface);
        assert_eq!(sym(&ex, "Id").label, SymbolLabel::Type);
        assert_eq!(sym(&ex, "Color").label, SymbolLabel::Enum);
    }

    #[test]
    fn a_call_records_its_enclosing_function() {
        let ex = extract_file(TS_SRC, Lang::TypeScript, "f.ts");
        let call = ex
            .calls
            .iter()
            .find(|c| c.callee == "helper")
            .expect("helper call");
        assert_eq!(call.caller.as_deref(), Some("f.ts:build"));
        assert_eq!(call.qualifier, None);
    }

    #[test]
    fn a_method_call_records_its_receiver_and_enclosing_method() {
        let ex = extract_file(TS_SRC, Lang::TypeScript, "f.ts");
        let call = ex
            .calls
            .iter()
            .find(|c| c.callee == "paint")
            .expect("paint call");
        assert_eq!(call.qualifier.as_deref(), Some("this"));
        assert_eq!(call.caller.as_deref(), Some("f.ts:Widget.draw"));
    }

    #[test]
    fn named_imports_bind_their_local_name() {
        let ex = extract_file(TS_SRC, Lang::TypeScript, "f.ts");
        let pairs: Vec<(&str, &str)> = ex
            .imports
            .iter()
            .map(|i| (i.local_name.as_str(), i.source.as_str()))
            .collect();
        assert!(pairs.contains(&("helper", "./m")), "{pairs:?}");
        // The alias is the binding a call site can reference; `other` is not.
        assert!(pairs.contains(&("alt", "./m")), "{pairs:?}");
        assert!(!pairs.iter().any(|(n, _)| *n == "other"), "{pairs:?}");
    }

    #[test]
    fn default_and_namespace_imports_bind_their_local_name() {
        let src = "import def from 'pkg';\nimport * as ns from 'node:fs';\n";
        let ex = extract_file(src, Lang::TypeScript, "f.ts");
        let pairs: Vec<(&str, &str)> = ex
            .imports
            .iter()
            .map(|i| (i.local_name.as_str(), i.source.as_str()))
            .collect();
        assert!(pairs.contains(&("def", "pkg")), "{pairs:?}");
        assert!(pairs.contains(&("ns", "node:fs")), "{pairs:?}");
    }

    #[test]
    fn extends_yields_an_inherits_relation_from_subclass_to_base() {
        let ex = extract_file(TS_SRC, Lang::TypeScript, "f.ts");
        assert_eq!(ex.relations.len(), 1, "{:?}", ex.relations);
        let rel = &ex.relations[0];
        assert_eq!(rel.subject, "Widget");
        assert_eq!(rel.object, "Base");
        assert_eq!(rel.kind, EdgeKind::Inherits);
    }

    #[test]
    fn interface_extends_yields_an_inherits_relation() {
        let src = "interface Sub extends Shape {}\n";
        let ex = extract_file(src, Lang::TypeScript, "f.ts");
        assert_eq!(ex.relations.len(), 1, "{:?}", ex.relations);
        assert_eq!(ex.relations[0].subject, "Sub");
        assert_eq!(ex.relations[0].object, "Shape");
    }

    #[test]
    fn tsx_extracts_through_jsx() {
        let src =
            "export const App = (props: Props) => {\n  return <div>{render(props)}</div>;\n};\n";
        let ex = extract_file(src, Lang::Tsx, "a.tsx");
        assert!(!ex.had_errors, "TSX source should parse cleanly");
        assert_eq!(sym(&ex, "App").label, SymbolLabel::Function);
        assert!(ex.calls.iter().any(|c| c.callee == "render"));
    }

    #[test]
    fn javascript_grammar_compiles_and_extracts() {
        let ex = extract_file(JS_SRC, Lang::JavaScript, "f.js");
        assert!(!ex.had_errors, "JS_SRC should parse cleanly");

        let build = sym(&ex, "build");
        assert_eq!(build.label, SymbolLabel::Function);
        assert!(build.exported);
        assert_eq!(sym(&ex, "render").label, SymbolLabel::Function);

        let draw = sym(&ex, "draw");
        assert_eq!(draw.label, SymbolLabel::Method);
        assert_eq!(draw.container.as_deref(), Some("Widget"));

        assert!(ex
            .calls
            .iter()
            .any(|c| c.callee == "helper" && c.caller.as_deref() == Some("f.js:build")));

        let pairs: Vec<(&str, &str)> = ex
            .imports
            .iter()
            .map(|i| (i.local_name.as_str(), i.source.as_str()))
            .collect();
        assert!(pairs.contains(&("helper", "./m")), "{pairs:?}");
        assert!(pairs.contains(&("fs", "node:fs")), "{pairs:?}");

        assert_eq!(ex.relations.len(), 1, "{:?}", ex.relations);
        assert_eq!(ex.relations[0].subject, "Widget");
        assert_eq!(ex.relations[0].object, "Base");
        assert_eq!(ex.relations[0].kind, EdgeKind::Inherits);
    }

    #[test]
    fn a_non_require_call_is_not_read_as_an_import() {
        let ex = extract_file("const conf = load('./m');\n", Lang::JavaScript, "f.js");
        assert!(ex.imports.is_empty(), "{:?}", ex.imports);
    }
}
