#[cfg(test)]
mod tests {
    use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

    #[test]
    fn ts_import_extraction() {
        let src = r#"
import { foo } from './foo'
import bar from 'bar'
const x = require('x')
"#;
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(src, None).unwrap();

        let query = Query::new(
            &lang,
            r#"(import_statement source: (string (string_fragment) @path))"#,
        )
        .unwrap();

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), src.as_bytes());
        let mut found: Vec<String> = Vec::new();
        while let Some(m) = matches.next() {
            for cap in m.captures {
                found.push(cap.node.utf8_text(src.as_bytes()).unwrap().to_string());
            }
        }
        assert_eq!(found, vec!["./foo", "bar"]);
    }

    #[test]
    fn ast_grep_pattern_match() {
        use ast_grep_core::AstGrep;
        use ast_grep_language::SupportLang;

        let src = "app.use(authMiddleware); app.use(logger)";
        let grep = AstGrep::new(src, SupportLang::TypeScript);
        let matches: Vec<_> = grep.root().find_all("app.use($MW)").collect();
        assert_eq!(matches.len(), 2);
    }
}
