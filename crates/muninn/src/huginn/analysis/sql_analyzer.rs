//! SQL analyzer — extracts table definitions, alters, indexes, and
//! foreign-key links from `.sql` files. High-signal targets: migration
//! files, because they literally define the schema. Each recognized
//! `CREATE TABLE` is surfaced as its own memory entry by the generator.
//!
//! Deliberately regex-based rather than a full SQL parser: the goal is
//! to record structural landmarks for retrieval, not to execute SQL.

use std::sync::OnceLock;

use regex::Regex;

#[derive(Debug, Clone, Default)]
pub struct SqlColumn {
    pub name: String,
    pub data_type: String,
    pub constraints: String,
}

#[derive(Debug, Clone, Default)]
pub struct SqlIndex {
    pub name: String,
    pub table: String,
    pub columns: String,
    pub unique: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SqlForeignKey {
    pub from_table: String,
    pub from_columns: String,
    pub ref_table: String,
    pub ref_columns: String,
}

#[derive(Debug, Clone, Default)]
pub struct SqlTable {
    pub name: String,
    pub schema: Option<String>,
    pub columns: Vec<SqlColumn>,
    pub inline_foreign_keys: Vec<SqlForeignKey>,
}

#[derive(Debug, Clone)]
pub enum SqlAlter {
    AddColumn { table: String, column: SqlColumn },
    DropColumn { table: String, column: String },
    AddForeignKey(SqlForeignKey),
    Other { table: String, raw: String },
}

#[derive(Debug, Clone, Default)]
pub struct SqlAnalysis {
    pub tables: Vec<SqlTable>,
    pub alters: Vec<SqlAlter>,
    pub indexes: Vec<SqlIndex>,
    /// Numeric migration prefix when the filename starts with digits
    /// (e.g. `001_initial_schema.sql` → `Some("001")`).
    pub migration_sequence: Option<String>,
    pub migration_name: Option<String>,
}

pub fn analyze(content: &str, relative_path: &str) -> SqlAnalysis {
    let stripped = strip_comments(content);
    let tables = extract_tables(&stripped);
    let alters = extract_alters(&stripped);
    let indexes = extract_indexes(&stripped);
    let (migration_sequence, migration_name) = migration_metadata(relative_path);

    SqlAnalysis {
        tables,
        alters,
        indexes,
        migration_sequence,
        migration_name,
    }
}

/// Rough filename parser: captures leading digits and the descriptive
/// suffix so migrations can be ordered and labelled without fishing
/// through the raw path in every consumer.
fn migration_metadata(relative_path: &str) -> (Option<String>, Option<String>) {
    let name = relative_path
        .rsplit('/')
        .next()
        .unwrap_or(relative_path)
        .trim_end_matches(".sql");

    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return (None, Some(name.to_string()));
    }

    let rest = name
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start_matches(['_', '-']);

    (
        Some(digits),
        if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        },
    )
}

fn strip_comments(sql: &str) -> String {
    // Drop `--` line comments and `/* ... */` block comments.
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek() == Some(&'-') {
            // line comment
            for ch in chars.by_ref() {
                if ch == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut prev = '\0';
            for ch in chars.by_ref() {
                if prev == '*' && ch == '/' {
                    break;
                }
                prev = ch;
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn extract_tables(sql: &str) -> Vec<SqlTable> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?is)CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([A-Za-z_"][\w\."]*)\s*\(([^;]*?)\)\s*;"#,
        )
        .expect("create-table regex")
    });

    let mut out = Vec::new();
    for caps in re.captures_iter(sql) {
        let full_name = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let body = caps.get(2).map(|m| m.as_str()).unwrap_or_default();

        let (schema, name) = split_schema(full_name);
        let (columns, fks) = parse_table_body(body);

        out.push(SqlTable {
            name,
            schema,
            columns,
            inline_foreign_keys: fks
                .into_iter()
                .map(|fk| SqlForeignKey {
                    from_table: full_name.to_string(),
                    ..fk
                })
                .collect(),
        });
    }
    out
}

fn split_schema(full: &str) -> (Option<String>, String) {
    let full = full.trim_matches('"');
    if let Some((s, n)) = full.split_once('.') {
        (
            Some(s.trim_matches('"').to_string()),
            n.trim_matches('"').to_string(),
        )
    } else {
        (None, full.to_string())
    }
}

fn parse_table_body(body: &str) -> (Vec<SqlColumn>, Vec<SqlForeignKey>) {
    // Split on commas that are NOT inside parentheses (avoid splitting
    // `DECIMAL(10, 2)` or `CHECK (x IN (...))`).
    let parts = split_top_level(body, ',');
    let mut columns = Vec::new();
    let mut fks = Vec::new();

    for raw in parts {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let upper = line.to_uppercase();

        // Inline foreign key
        if upper.starts_with("FOREIGN KEY")
            || upper.starts_with("CONSTRAINT ") && upper.contains("FOREIGN KEY")
        {
            if let Some(fk) = parse_inline_foreign_key(line) {
                fks.push(fk);
                continue;
            }
        }

        // Table-level PRIMARY KEY / UNIQUE / CHECK — skip for now
        if upper.starts_with("PRIMARY KEY")
            || upper.starts_with("UNIQUE ")
            || upper.starts_with("UNIQUE(")
            || upper.starts_with("CHECK ")
            || upper.starts_with("CHECK(")
        {
            continue;
        }

        // Otherwise treat as a column definition: NAME TYPE [CONSTRAINTS...]
        let mut tokens = line.splitn(3, char::is_whitespace);
        let name = tokens.next().unwrap_or("").trim_matches('"').to_string();
        let data_type = tokens.next().unwrap_or("").to_string();
        let constraints = tokens.next().unwrap_or("").trim().to_string();
        if !name.is_empty() && !data_type.is_empty() {
            columns.push(SqlColumn {
                name,
                data_type,
                constraints,
            });
        }
    }
    (columns, fks)
}

fn parse_inline_foreign_key(line: &str) -> Option<SqlForeignKey> {
    // Very rough: FOREIGN KEY (col_a, col_b) REFERENCES other(col_x, col_y)
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?is)FOREIGN\s+KEY\s*\(([^)]+)\)\s*REFERENCES\s+([A-Za-z_"][\w\."]*)\s*(?:\(([^)]+)\))?"#,
        )
        .expect("fk regex")
    });
    let caps = re.captures(line)?;
    Some(SqlForeignKey {
        from_table: String::new(),
        from_columns: caps.get(1)?.as_str().trim().to_string(),
        ref_table: caps.get(2)?.as_str().trim().to_string(),
        ref_columns: caps
            .get(3)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default(),
    })
}

fn split_top_level(s: &str, delim: char) -> Vec<String> {
    let mut depth = 0i32;
    let mut current = String::new();
    let mut out = Vec::new();
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = (depth - 1).max(0);
                current.push(c);
            }
            ch if ch == delim && depth == 0 => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn extract_alters(sql: &str) -> Vec<SqlAlter> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?is)ALTER\s+TABLE\s+([A-Za-z_"][\w\."]*)\s+([^;]+);"#).expect("alter regex")
    });

    let mut out = Vec::new();
    for caps in re.captures_iter(sql) {
        let table = caps
            .get(1)
            .map(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let body = caps.get(2).map(|m| m.as_str()).unwrap_or_default().trim();
        let upper = body.to_uppercase();

        if let Some(rest) = upper.strip_prefix("ADD COLUMN ") {
            let mut tokens = rest.splitn(3, char::is_whitespace);
            let name = tokens.next().unwrap_or("").to_string();
            let data_type = tokens.next().unwrap_or("").to_string();
            let constraints = tokens.next().unwrap_or("").trim().to_string();
            out.push(SqlAlter::AddColumn {
                table,
                column: SqlColumn {
                    name,
                    data_type,
                    constraints,
                },
            });
        } else if let Some(rest) = upper.strip_prefix("DROP COLUMN ") {
            let name = rest.split_whitespace().next().unwrap_or("").to_string();
            out.push(SqlAlter::DropColumn {
                table,
                column: name,
            });
        } else if upper.starts_with("ADD CONSTRAINT") && upper.contains("FOREIGN KEY") {
            if let Some(fk) = parse_inline_foreign_key(body) {
                let mut fk = fk;
                fk.from_table = table;
                out.push(SqlAlter::AddForeignKey(fk));
            }
        } else {
            out.push(SqlAlter::Other {
                table,
                raw: body.to_string(),
            });
        }
    }
    out
}

fn extract_indexes(sql: &str) -> Vec<SqlIndex> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?is)CREATE\s+(UNIQUE\s+)?INDEX\s+(?:IF\s+NOT\s+EXISTS\s+)?([A-Za-z_"][\w\."]*)\s+ON\s+([A-Za-z_"][\w\."]*)\s*\(([^)]+)\)"#,
        )
        .expect("index regex")
    });
    let mut out = Vec::new();
    for caps in re.captures_iter(sql) {
        let unique = caps.get(1).is_some();
        let name = caps
            .get(2)
            .map(|m| m.as_str().trim_matches('"').to_string())
            .unwrap_or_default();
        let table = caps
            .get(3)
            .map(|m| m.as_str().trim_matches('"').to_string())
            .unwrap_or_default();
        let columns = caps
            .get(4)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        out.push(SqlIndex {
            name,
            table,
            columns,
            unique,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_filename_parsed() {
        let (seq, name) = migration_metadata("migrations/001_initial_schema.sql");
        assert_eq!(seq.as_deref(), Some("001"));
        assert_eq!(name.as_deref(), Some("initial_schema"));

        let (seq2, name2) = migration_metadata("schema.sql");
        assert_eq!(seq2, None);
        assert_eq!(name2.as_deref(), Some("schema"));
    }

    #[test]
    fn strips_line_and_block_comments() {
        let src = "-- top\nCREATE TABLE a (id INT); /* drop\n */ SELECT 1;";
        let out = strip_comments(src);
        assert!(!out.contains("top"));
        assert!(!out.contains("drop"));
        assert!(out.contains("CREATE TABLE"));
    }

    #[test]
    fn extracts_table_with_columns_and_fk() {
        let src = r#"
            CREATE TABLE "muninn"."memory_entries" (
                id UUID PRIMARY KEY,
                namespace TEXT NOT NULL,
                created_at TIMESTAMPTZ DEFAULT NOW(),
                project_id TEXT,
                FOREIGN KEY (project_id) REFERENCES muninn.projects (id)
            );
        "#;
        let analysis = analyze(src, "migrations/001_x.sql");
        assert_eq!(analysis.tables.len(), 1);
        let t = &analysis.tables[0];
        assert_eq!(t.schema.as_deref(), Some("muninn"));
        assert_eq!(t.name, "memory_entries");
        assert!(t.columns.iter().any(|c| c.name == "id"));
        assert!(t.columns.iter().any(|c| c.name == "namespace"));
        assert_eq!(t.inline_foreign_keys.len(), 1);
        assert_eq!(t.inline_foreign_keys[0].ref_table, "muninn.projects");
    }

    #[test]
    fn extracts_alter_add_column_and_drop() {
        let src = r#"
            ALTER TABLE users ADD COLUMN email TEXT NOT NULL;
            ALTER TABLE users DROP COLUMN legacy_flag;
        "#;
        let a = analyze(src, "migrations/002_users.sql");
        assert_eq!(a.alters.len(), 2);
        match &a.alters[0] {
            SqlAlter::AddColumn { table, column } => {
                assert_eq!(table, "users");
                assert_eq!(column.name.to_uppercase(), "EMAIL");
            }
            other => panic!("expected AddColumn, got {:?}", other),
        }
        match &a.alters[1] {
            SqlAlter::DropColumn { table, column } => {
                assert_eq!(table, "users");
                assert_eq!(column.to_uppercase(), "LEGACY_FLAG");
            }
            other => panic!("expected DropColumn, got {:?}", other),
        }
    }

    #[test]
    fn extracts_indexes_unique_and_plain() {
        let src = r#"
            CREATE UNIQUE INDEX idx_users_email ON users (email);
            CREATE INDEX idx_memory_ns ON muninn.memory_entries (namespace, type);
        "#;
        let a = analyze(src, "schema.sql");
        assert_eq!(a.indexes.len(), 2);
        assert!(a.indexes[0].unique);
        assert_eq!(a.indexes[1].table, "muninn.memory_entries");
        assert_eq!(a.indexes[1].columns, "namespace, type");
    }

    #[test]
    fn ignores_types_with_parens_when_splitting() {
        let src = "CREATE TABLE t (id INT, amount DECIMAL(10, 2) NOT NULL);";
        let a = analyze(src, "x.sql");
        let t = &a.tables[0];
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.columns[1].data_type, "DECIMAL(10,");
    }
}
