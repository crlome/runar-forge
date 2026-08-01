//! Dump a file's tree-sitter parse.
//!
//! `cargo run --release -p runar-muninn --example sexp -- <file>`
//!
//! Exists because `node-types.json` describes what a grammar *can* produce, not
//! what it produces for a given piece of source, and queries written from the
//! summary alone can be dead on arrival: Go's grammar parses `Map[int](xs)` as
//! a type conversion rather than a call, so patterns matching a call in that
//! position never fire. Check the real shape before writing a `LangSpec`.

use std::path::Path;

use runar_muninn::huginn::graph::parsers::{make_parser, Lang};

fn main() {
    let Some(arg) = std::env::args().nth(1) else {
        eprintln!("usage: sexp <file>");
        std::process::exit(2);
    };
    let path = Path::new(&arg);
    let Some(lang) = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(Lang::from_extension)
    else {
        eprintln!("no grammar for {arg}");
        std::process::exit(2);
    };
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{arg}: {e}");
            std::process::exit(2);
        }
    };
    let mut parser = make_parser(lang);
    match parser.parse(&source, None) {
        Some(tree) => println!("{}", tree.root_node().to_sexp()),
        None => {
            eprintln!("{arg}: parse failed");
            std::process::exit(1);
        }
    }
}
