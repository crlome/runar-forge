//! SQLite store for the code graph.
//!
//! Separate file from `memory.db` on purpose: this is derived data that can be
//! rebuilt from source at any time, so it carries no migrations — a schema
//! version mismatch drops everything and asks for a recrawl. It is also always
//! SQLite regardless of `RUNAR_STORAGE`, because a crawl is inherently local
//! and hooks must be able to read it without a network round trip.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::{name_tokens, EdgeKind, FileStatus, Resolution, SymbolMetrics};

/// Bumping this discards every project's graph. Do it whenever the extracted
/// shape changes, since a partially-old graph is worse than no graph.
const SCHEMA_VERSION: i64 = 1;

/// Cross-process contention is real here: a crawl writes while hooks and the
/// MCP server read the same file.
const BUSY_TIMEOUT_MS: u32 = 5_000;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Db(String),
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Db(m) => write!(f, "codegraph db: {m}"),
            Error::Io(m) => write!(f, "codegraph io: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}

/// A definition ready to be written, with its identity already computed.
#[derive(Debug, Clone)]
pub struct SymbolRecord {
    pub name: String,
    pub qualified_name: String,
    pub label: &'static str,
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
    pub exported: bool,
    pub metrics: SymbolMetrics,
}

/// Per-file bookkeeping written alongside that file's symbols. Grouped so the
/// three adjacent `&str` fields cannot be passed in the wrong order.
#[derive(Debug, Clone, Copy)]
pub struct FileRecord<'a> {
    pub path: &'a str,
    pub lang: Option<&'a str>,
    pub content_hash: &'a str,
    pub status: FileStatus,
    pub detail: Option<&'a str>,
}

/// A resolved relation between two definitions.
#[derive(Debug, Clone)]
pub struct EdgeRecord {
    pub source_qualified: String,
    pub target_qualified: String,
    pub kind: EdgeKind,
    pub resolution: Option<Resolution>,
    pub line: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    pub files_total: usize,
    pub indexed: usize,
    pub partial: usize,
    pub skipped_lang: usize,
    pub errored: usize,
    pub symbols: usize,
    pub edges: usize,
    pub unresolved_calls: usize,
    /// Extension → count, for the languages this build cannot parse.
    pub skipped_by_ext: Vec<(String, usize)>,
}

/// Drop the contentless-FTS rows for a project, optionally narrowed to one
/// file. Must run before the matching `code_nodes` rows are deleted.
///
/// The table is declared `contentless_delete=1`, which is what makes a plain
/// `DELETE` work. A `content=''` table without it needs the original column
/// values and silently keeps the old tokens when it does not get them, leaving
/// `code_nodes.id` rowids — which SQLite reuses — pointing at a dead symbol's
/// terms.
fn delete_fts_rows(
    tx: &rusqlite::Transaction<'_>,
    project: &str,
    path: Option<&str>,
) -> Result<()> {
    let ids: Vec<i64> = match path {
        Some(p) => {
            let mut stmt =
                tx.prepare("SELECT id FROM code_nodes WHERE project = ?1 AND file_path = ?2")?;
            let rows = stmt.query_map(params![project, p], |r| r.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        }
        None => {
            let mut stmt = tx.prepare("SELECT id FROM code_nodes WHERE project = ?1")?;
            let rows = stmt.query_map(params![project], |r| r.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        }
    };
    for id in ids {
        tx.execute("DELETE FROM code_symbols_fts WHERE rowid = ?1", params![id])?;
    }
    Ok(())
}

pub struct CodeGraphStore {
    conn: Mutex<Connection>,
}

impl CodeGraphStore {
    /// `RUNAR_CODEGRAPH_PATH`, else `<runar dir>/codegraph.db`.
    pub fn default_path() -> PathBuf {
        match std::env::var("RUNAR_CODEGRAPH_PATH") {
            Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => crate::setup::runar_dir().join("codegraph.db"),
        }
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&Self::default_path())
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.configure()?;
        store.initialize()?;
        Ok(store)
    }

    /// Read-only handle for the hook and query paths, which must never create
    /// or migrate the file.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let store = Self {
            conn: Mutex::new(Connection::open_in_memory()?),
        };
        store.initialize()?;
        Ok(store)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|e| Error::Db(e.to_string()))
    }

    fn configure(&self) -> Result<()> {
        let db = self.lock()?;
        db.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
        // journal_mode returns a result row, so use query_row instead of execute_batch
        let _: String = db.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        db.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(())
    }

    fn initialize(&self) -> Result<()> {
        let stored: Option<i64> = {
            let db = self.lock()?;
            db.execute(
                "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                [],
            )?;
            db.query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?
        };

        match stored {
            Some(v) if v == SCHEMA_VERSION => return Ok(()),
            Some(_) => self.drop_all()?,
            None => {}
        }

        let db = self.lock()?;
        db.execute_batch(SCHEMA)?;
        db.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    fn drop_all(&self) -> Result<()> {
        let db = self.lock()?;
        db.execute_batch(
            "DROP TABLE IF EXISTS code_symbols_fts;
             DROP TABLE IF EXISTS code_edges;
             DROP TABLE IF EXISTS code_calls;
             DROP TABLE IF EXISTS code_nodes;
             DROP TABLE IF EXISTS code_files;
             DROP TABLE IF EXISTS code_projects;",
        )?;
        Ok(())
    }

    /// Register a project and clear the previous run's coverage rows. Symbols
    /// are left alone so an incremental pass can replace them file by file.
    pub fn begin_project(&self, project: &str, root: &Path, full: bool) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO code_projects (project, root, indexed_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(project) DO UPDATE SET root = ?2, indexed_at = datetime('now')",
            params![project, root.to_string_lossy()],
        )?;
        if full {
            // A full crawl re-states the whole inventory; stale rows for files
            // that vanished would otherwise linger in the coverage counts.
            // The FTS index is contentless and SQLite reuses rowids, so its
            // rows have to go before the nodes do or a later symbol inherits
            // a dead entry's tokens.
            delete_fts_rows(&tx, project, None)?;
            tx.execute(
                "DELETE FROM code_files WHERE project = ?1",
                params![project],
            )?;
            tx.execute(
                "DELETE FROM code_nodes WHERE project = ?1",
                params![project],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Replace one file's contribution: its coverage row and all of its
    /// definitions. Edges referencing the old rows cascade away and are rebuilt
    /// wholesale afterwards.
    pub fn replace_file(
        &self,
        project: &str,
        file: FileRecord<'_>,
        symbols: &[SymbolRecord],
    ) -> Result<()> {
        let FileRecord {
            path,
            lang,
            content_hash,
            status,
            detail,
        } = file;
        let mut db = self.lock()?;
        let tx = db.transaction()?;

        // FTS is contentless, so its rows have to go before the ids do.
        delete_fts_rows(&tx, project, Some(path))?;
        tx.execute(
            "DELETE FROM code_nodes WHERE project = ?1 AND file_path = ?2",
            params![project, path],
        )?;

        tx.execute(
            "INSERT INTO code_files (project, path, lang, content_hash, status, detail, symbol_count, unresolved_calls)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
             ON CONFLICT(project, path) DO UPDATE SET
               lang = ?3, content_hash = ?4, status = ?5, detail = ?6,
               symbol_count = ?7, unresolved_calls = 0",
            params![
                project,
                path,
                lang,
                content_hash,
                status.as_str(),
                detail,
                symbols.len() as i64
            ],
        )?;

        for s in symbols {
            tx.execute(
                "INSERT INTO code_nodes
                   (project, label, name, qualified_name, file_path, start_line, end_line,
                    signature, exported, complexity, cognitive, loop_depth, param_count)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
                 ON CONFLICT(project, qualified_name) DO NOTHING",
                params![
                    project,
                    s.label,
                    s.name,
                    s.qualified_name,
                    s.file_path,
                    s.start_line,
                    s.end_line,
                    s.signature,
                    s.exported as i32,
                    s.metrics.complexity,
                    s.metrics.cognitive,
                    s.metrics.loop_depth,
                    s.metrics.param_count,
                ],
            )?;
            if tx.changes() == 0 {
                continue;
            }
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO code_symbols_fts (rowid, name_tokens, qualified_name, file_path)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, name_tokens(&s.name), s.qualified_name, s.file_path],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Rebuild every derived edge for a project in one transaction. Edges are
    /// derived from raw call sites rather than patched in place, so a stale
    /// edge cannot survive a re-resolve.
    pub fn rebuild_edges(&self, project: &str, edges: &[EdgeRecord]) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        tx.execute(
            "DELETE FROM code_edges WHERE project = ?1",
            params![project],
        )?;

        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO code_edges
               (project, source_id, target_id, type, confidence, resolution, line)
             SELECT ?1, src.id, tgt.id, ?4, ?5, ?6, ?7
             FROM code_nodes src, code_nodes tgt
             WHERE src.project = ?1 AND src.qualified_name = ?2
               AND tgt.project = ?1 AND tgt.qualified_name = ?3",
        )?;
        for e in edges {
            insert.execute(params![
                project,
                e.source_qualified,
                e.target_qualified,
                e.kind.as_str(),
                e.resolution.map(|r| r.confidence()),
                e.resolution.map(|r| r.as_str()),
                e.line,
            ])?;
        }
        drop(insert);
        tx.commit()?;
        Ok(())
    }

    pub fn set_unresolved(&self, project: &str, path: &str, count: usize) -> Result<()> {
        let db = self.lock()?;
        db.execute(
            "UPDATE code_files SET unresolved_calls = ?3 WHERE project = ?1 AND path = ?2",
            params![project, path, count as i64],
        )?;
        Ok(())
    }

    /// Store the precomputed hook summary so the injection path is a single
    /// indexed row read rather than an aggregation.
    pub fn set_summary(&self, project: &str, summary: &str) -> Result<()> {
        let db = self.lock()?;
        db.execute(
            "UPDATE code_projects SET summary = ?2 WHERE project = ?1",
            params![project, summary],
        )?;
        Ok(())
    }

    pub fn summary(&self, project: &str) -> Result<Option<String>> {
        let db = self.lock()?;
        Ok(db
            .query_row(
                "SELECT summary FROM code_projects WHERE project = ?1",
                params![project],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn coverage(&self, project: &str) -> Result<Coverage> {
        let db = self.lock()?;
        let mut cov = Coverage::default();

        let mut stmt = db.prepare(
            "SELECT status, COUNT(*) FROM code_files WHERE project = ?1 GROUP BY status",
        )?;
        let rows = stmt.query_map(params![project], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })?;
        for row in rows {
            let (status, n) = row?;
            cov.files_total += n;
            match status.as_str() {
                "indexed" => cov.indexed = n,
                "partial" => cov.partial = n,
                "skipped_lang" => cov.skipped_lang = n,
                _ => cov.errored = n,
            }
        }
        drop(stmt);

        cov.symbols = db.query_row(
            "SELECT COUNT(*) FROM code_nodes WHERE project = ?1",
            params![project],
            |r| r.get::<_, i64>(0),
        )? as usize;
        cov.edges = db.query_row(
            "SELECT COUNT(*) FROM code_edges WHERE project = ?1",
            params![project],
            |r| r.get::<_, i64>(0),
        )? as usize;
        cov.unresolved_calls = db.query_row(
            "SELECT COALESCE(SUM(unresolved_calls), 0) FROM code_files WHERE project = ?1",
            params![project],
            |r| r.get::<_, i64>(0),
        )? as usize;

        let mut stmt = db.prepare(
            "SELECT path FROM code_files WHERE project = ?1 AND status = 'skipped_lang'",
        )?;
        let mut by_ext: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for path in stmt.query_map(params![project], |r| r.get::<_, String>(0))? {
            let path = path?;
            let ext = Path::new(&path)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_else(|| "(no extension)".to_string());
            *by_ext.entry(ext).or_default() += 1;
        }
        let mut by_ext: Vec<(String, usize)> = by_ext.into_iter().collect();
        by_ext.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        by_ext.truncate(8);
        cov.skipped_by_ext = by_ext;

        Ok(cov)
    }

    /// Content hashes from the previous run, for deciding what to re-parse.
    pub fn file_hashes(&self, project: &str) -> Result<std::collections::HashMap<String, String>> {
        let db = self.lock()?;
        let mut stmt =
            db.prepare("SELECT path, content_hash FROM code_files WHERE project = ?1")?;
        let rows = stmt.query_map(params![project], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (p, h) = row?;
            out.insert(p, h);
        }
        Ok(out)
    }

    pub fn forget_file(&self, project: &str, path: &str) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction()?;
        delete_fts_rows(&tx, project, Some(path))?;
        tx.execute(
            "DELETE FROM code_nodes WHERE project = ?1 AND file_path = ?2",
            params![project, path],
        )?;
        tx.execute(
            "DELETE FROM code_files WHERE project = ?1 AND path = ?2",
            params![project, path],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Definitions with the most incoming CALLS edges — the hubs worth naming
    /// in a session summary.
    pub fn top_hubs(&self, project: &str, limit: usize) -> Result<Vec<(String, String, usize)>> {
        let db = self.lock()?;
        let mut stmt = db.prepare(
            "SELECT n.qualified_name, n.label, COUNT(e.id) AS fan_in
             FROM code_nodes n
             JOIN code_edges e ON e.target_id = n.id AND e.type = 'CALLS'
             WHERE n.project = ?1
             GROUP BY n.id
             ORDER BY fan_in DESC, n.qualified_name
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![project, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as usize,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS code_projects (
  project     TEXT PRIMARY KEY,
  root        TEXT NOT NULL,
  indexed_at  TEXT NOT NULL,
  summary     TEXT
);

CREATE TABLE IF NOT EXISTS code_files (
  project          TEXT NOT NULL,
  path             TEXT NOT NULL,
  lang             TEXT,
  content_hash     TEXT NOT NULL DEFAULT '',
  status           TEXT NOT NULL CHECK (status IN ('indexed','partial','skipped_lang','error')),
  detail           TEXT,
  symbol_count     INTEGER NOT NULL DEFAULT 0,
  unresolved_calls INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (project, path)
);

CREATE TABLE IF NOT EXISTS code_nodes (
  id             INTEGER PRIMARY KEY,
  project        TEXT NOT NULL,
  label          TEXT NOT NULL,
  name           TEXT NOT NULL,
  qualified_name TEXT NOT NULL,
  file_path      TEXT NOT NULL,
  start_line     INTEGER NOT NULL DEFAULT 0,
  end_line       INTEGER NOT NULL DEFAULT 0,
  signature      TEXT NOT NULL DEFAULT '',
  exported       INTEGER NOT NULL DEFAULT 0,
  complexity     INTEGER NOT NULL DEFAULT 0,
  cognitive      INTEGER NOT NULL DEFAULT 0,
  loop_depth     INTEGER NOT NULL DEFAULT 0,
  param_count    INTEGER NOT NULL DEFAULT 0,
  UNIQUE (project, qualified_name)
);

CREATE INDEX IF NOT EXISTS idx_code_nodes_name  ON code_nodes(project, name);
CREATE INDEX IF NOT EXISTS idx_code_nodes_file  ON code_nodes(project, file_path);
CREATE INDEX IF NOT EXISTS idx_code_nodes_label ON code_nodes(project, label);

CREATE TABLE IF NOT EXISTS code_edges (
  id         INTEGER PRIMARY KEY,
  project    TEXT NOT NULL,
  source_id  INTEGER NOT NULL REFERENCES code_nodes(id) ON DELETE CASCADE,
  target_id  INTEGER NOT NULL REFERENCES code_nodes(id) ON DELETE CASCADE,
  type       TEXT NOT NULL,
  confidence REAL,
  resolution TEXT,
  line       INTEGER NOT NULL DEFAULT 0,
  UNIQUE (source_id, target_id, type, line)
);

CREATE INDEX IF NOT EXISTS idx_code_edges_source ON code_edges(source_id, type);
CREATE INDEX IF NOT EXISTS idx_code_edges_target ON code_edges(target_id, type);

CREATE VIRTUAL TABLE IF NOT EXISTS code_symbols_fts USING fts5(
  name_tokens,
  qualified_name,
  file_path,
  content='',
  contentless_delete=1,
  tokenize='unicode61 remove_diacritics 2'
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegraph::{qualified_name, SymbolLabel};

    fn sym(file: &str, container: Option<&str>, name: &str, label: SymbolLabel) -> SymbolRecord {
        SymbolRecord {
            name: name.to_string(),
            qualified_name: qualified_name(file, container, name),
            label: label.as_str(),
            file_path: file.to_string(),
            start_line: 1,
            end_line: 5,
            signature: format!("fn {name}()"),
            exported: true,
            metrics: SymbolMetrics::default(),
        }
    }

    fn store() -> CodeGraphStore {
        CodeGraphStore::in_memory().unwrap()
    }

    #[test]
    fn replace_file_is_idempotent_and_swaps_symbols() {
        let s = store();
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();

        let a = vec![sym("src/a.rs", None, "alpha", SymbolLabel::Function)];
        s.replace_file(
            "p",
            FileRecord {
                path: "src/a.rs",
                lang: Some("rust"),
                content_hash: "h1",
                status: FileStatus::Indexed,
                detail: None,
            },
            &a,
        )
        .unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "src/a.rs",
                lang: Some("rust"),
                content_hash: "h1",
                status: FileStatus::Indexed,
                detail: None,
            },
            &a,
        )
        .unwrap();
        assert_eq!(
            s.coverage("p").unwrap().symbols,
            1,
            "re-indexing duplicated rows"
        );

        let b = vec![sym("src/a.rs", None, "beta", SymbolLabel::Function)];
        s.replace_file(
            "p",
            FileRecord {
                path: "src/a.rs",
                lang: Some("rust"),
                content_hash: "h2",
                status: FileStatus::Indexed,
                detail: None,
            },
            &b,
        )
        .unwrap();
        let cov = s.coverage("p").unwrap();
        assert_eq!(cov.symbols, 1, "the old definition should be gone");
        assert_eq!(cov.files_total, 1);
    }

    #[test]
    fn edges_resolve_by_qualified_name_and_rebuild_cleanly() {
        let s = store();
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "src/a.rs",
                lang: Some("rust"),
                content_hash: "h",
                status: FileStatus::Indexed,
                detail: None,
            },
            &[
                sym("src/a.rs", None, "caller", SymbolLabel::Function),
                sym("src/a.rs", None, "callee", SymbolLabel::Function),
            ],
        )
        .unwrap();

        let edge = EdgeRecord {
            source_qualified: "src/a.rs:caller".into(),
            target_qualified: "src/a.rs:callee".into(),
            kind: EdgeKind::Calls,
            resolution: Some(Resolution::SameModule),
            line: 3,
        };
        s.rebuild_edges("p", std::slice::from_ref(&edge)).unwrap();
        assert_eq!(s.coverage("p").unwrap().edges, 1);

        // Rebuilding replaces rather than accumulates.
        s.rebuild_edges("p", std::slice::from_ref(&edge)).unwrap();
        assert_eq!(s.coverage("p").unwrap().edges, 1);

        // An edge naming a definition that does not exist is dropped, not an error.
        s.rebuild_edges(
            "p",
            &[EdgeRecord {
                source_qualified: "src/a.rs:caller".into(),
                target_qualified: "src/nope.rs:ghost".into(),
                kind: EdgeKind::Calls,
                resolution: Some(Resolution::UniqueName),
                line: 9,
            }],
        )
        .unwrap();
        assert_eq!(s.coverage("p").unwrap().edges, 0);
    }

    #[test]
    fn deleting_a_file_cascades_to_edges() {
        let s = store();
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "src/a.rs",
                lang: Some("rust"),
                content_hash: "h",
                status: FileStatus::Indexed,
                detail: None,
            },
            &[
                sym("src/a.rs", None, "caller", SymbolLabel::Function),
                sym("src/a.rs", None, "callee", SymbolLabel::Function),
            ],
        )
        .unwrap();
        s.rebuild_edges(
            "p",
            &[EdgeRecord {
                source_qualified: "src/a.rs:caller".into(),
                target_qualified: "src/a.rs:callee".into(),
                kind: EdgeKind::Calls,
                resolution: Some(Resolution::SameModule),
                line: 3,
            }],
        )
        .unwrap();

        s.forget_file("p", "src/a.rs").unwrap();
        let cov = s.coverage("p").unwrap();
        assert_eq!(cov.symbols, 0);
        assert_eq!(cov.edges, 0, "edges must cascade with their nodes");
        assert_eq!(cov.files_total, 0);
    }

    fn fts_hits(store: &CodeGraphStore, term: &str) -> i64 {
        let db = store.lock().unwrap();
        db.query_row(
            "SELECT COUNT(*) FROM code_symbols_fts WHERE code_symbols_fts MATCH ?1",
            params![term],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn removing_a_symbol_removes_its_search_tokens() {
        // FTS5 rowids are code_nodes ids and SQLite reuses ids, so a delete that
        // does not really delete leaves the next symbol wearing a dead one's
        // terms — and it fails silently.
        let s = store();
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
        fn rec(hash: &str) -> FileRecord<'_> {
            FileRecord {
                path: "src/a.rs",
                lang: Some("rust"),
                content_hash: hash,
                status: FileStatus::Indexed,
                detail: None,
            }
        }
        s.replace_file(
            "p",
            rec("h1"),
            &[sym("src/a.rs", None, "zarquon", SymbolLabel::Function)],
        )
        .unwrap();
        assert_eq!(fts_hits(&s, "zarquon"), 1);

        s.replace_file(
            "p",
            rec("h2"),
            &[sym("src/a.rs", None, "blarg", SymbolLabel::Function)],
        )
        .unwrap();
        assert_eq!(
            fts_hits(&s, "zarquon"),
            0,
            "tokens for a replaced symbol survived"
        );
        assert_eq!(fts_hits(&s, "blarg"), 1);

        s.forget_file("p", "src/a.rs").unwrap();
        assert_eq!(fts_hits(&s, "blarg"), 0, "forget_file left tokens behind");

        // A full re-index must not accumulate a second copy either.
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
        s.replace_file(
            "p",
            rec("h3"),
            &[sym("src/a.rs", None, "zarquon", SymbolLabel::Function)],
        )
        .unwrap();
        assert_eq!(fts_hits(&s, "zarquon"), 1);
    }

    #[test]
    fn coverage_counts_every_status() {
        let s = store();
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "a.rs",
                lang: Some("rust"),
                content_hash: "h",
                status: FileStatus::Indexed,
                detail: None,
            },
            &[],
        )
        .unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "b.rs",
                lang: Some("rust"),
                content_hash: "h",
                status: FileStatus::Partial,
                detail: Some("2 errors"),
            },
            &[],
        )
        .unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "c.java",
                lang: None,
                content_hash: "h",
                status: FileStatus::SkippedLang,
                detail: None,
            },
            &[],
        )
        .unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "d.rs",
                lang: Some("rust"),
                content_hash: "h",
                status: FileStatus::Error,
                detail: Some("unreadable"),
            },
            &[],
        )
        .unwrap();

        let cov = s.coverage("p").unwrap();
        assert_eq!(cov.files_total, 4);
        assert_eq!(
            (cov.indexed, cov.partial, cov.skipped_lang, cov.errored),
            (1, 1, 1, 1)
        );
    }

    #[test]
    fn a_schema_version_bump_discards_the_old_graph() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cg.db");
        {
            let s = CodeGraphStore::open(&path).unwrap();
            s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
            s.replace_file(
                "p",
                FileRecord {
                    path: "a.rs",
                    lang: Some("rust"),
                    content_hash: "h",
                    status: FileStatus::Indexed,
                    detail: None,
                },
                &[sym("a.rs", None, "alpha", SymbolLabel::Function)],
            )
            .unwrap();
            assert_eq!(s.coverage("p").unwrap().symbols, 1);
            let db = s.lock().unwrap();
            db.execute(
                "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        }
        let s = CodeGraphStore::open(&path).unwrap();
        assert_eq!(
            s.coverage("p").unwrap().symbols,
            0,
            "a version mismatch must rebuild from empty"
        );
    }

    #[test]
    fn file_hashes_round_trip_for_incremental_decisions() {
        let s = store();
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
        s.replace_file(
            "p",
            FileRecord {
                path: "a.rs",
                lang: Some("rust"),
                content_hash: "hash-a",
                status: FileStatus::Indexed,
                detail: None,
            },
            &[],
        )
        .unwrap();
        let h = s.file_hashes("p").unwrap();
        assert_eq!(h.get("a.rs").map(String::as_str), Some("hash-a"));
    }
}
