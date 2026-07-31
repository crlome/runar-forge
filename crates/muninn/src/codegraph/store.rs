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

    /// When this project's graph was last built. Nothing indexes incrementally
    /// yet, so anything read out of it is a snapshot of that moment.
    pub fn indexed_at(&self, project: &str) -> Result<Option<String>> {
        let db = self.lock()?;
        Ok(db
            .query_row(
                "SELECT indexed_at FROM code_projects WHERE project = ?1",
                params![project],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
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

    // ── Queries ────────────────────────────────────────────────────

    /// Read-only handle if this project has a graph, else `None`.
    ///
    /// For the hook paths, which must never create the file, never migrate it,
    /// and never fail a hook: every problem is simply "no graph".
    pub fn open_if_indexed(project: &str) -> Option<Self> {
        let path = Self::default_path();
        if !path.exists() {
            return None;
        }
        let store = Self::open_readonly(&path).ok()?;
        let known = store.projects().ok()?;
        known.iter().any(|p| p == project).then_some(store)
    }

    /// Every project with a graph, oldest index first.
    pub fn projects(&self) -> Result<Vec<String>> {
        let db = self.lock()?;
        let mut stmt = db.prepare("SELECT project FROM code_projects ORDER BY project")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The schema version the file on disk carries, if it has one.
    pub fn stored_schema_version(&self) -> Result<Option<i64>> {
        let db = self.lock()?;
        Ok(db
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The version this build writes; a file carrying anything else is rebuilt.
    pub fn expected_schema_version() -> i64 {
        SCHEMA_VERSION
    }

    /// Full-text search over symbol names, ranked by BM25 and nudged toward the
    /// kinds a question is usually about: someone searching a name wants the
    /// function that bears it before the module that contains it.
    pub fn search(
        &self,
        project: &str,
        query: &str,
        label: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SymbolRow>> {
        let Some(match_expr) = crate::fts_query::build(query) else {
            return Ok(Vec::new());
        };
        let db = self.lock()?;
        let sql = format!(
            "SELECT {COLS},
                    bm25(code_symbols_fts) - CASE n.label
                      WHEN 'Function' THEN 10.0 WHEN 'Method' THEN 10.0
                      WHEN 'Class' THEN 5.0 WHEN 'Trait' THEN 5.0
                      WHEN 'Struct' THEN 5.0 WHEN 'Interface' THEN 5.0
                      WHEN 'Enum' THEN 5.0 ELSE 0.0 END AS score
             FROM code_symbols_fts f
             JOIN code_nodes n ON n.id = f.rowid
             WHERE f.code_symbols_fts MATCH ?1 AND n.project = ?2
               AND (?3 IS NULL OR n.label = ?3)
             ORDER BY score, n.qualified_name
             LIMIT ?4"
        );
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map(
            params![match_expr, project, label, limit as i64],
            row_to_symbol,
        )?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Locate a definition by qualified name, then by bare name, then by
    /// qualified-name suffix — so both `Store.open` and `open` find something.
    ///
    /// The suffix tier is anchored on a separator and escapes LIKE wildcards:
    /// unanchored, `open` would match `reopen` and a needle containing `%`
    /// would match everything.
    pub fn symbol(&self, project: &str, needle: &str, limit: usize) -> Result<Matches> {
        let db = self.lock()?;
        let escaped = escape_like(needle);
        for (clause, bind) in [
            ("n.qualified_name = ?2", needle.to_string()),
            ("n.name = ?2", needle.to_string()),
            (
                "(n.qualified_name LIKE '%:' || ?2 ESCAPE '\\' \
                  OR n.qualified_name LIKE '%.' || ?2 ESCAPE '\\')",
                escaped.clone(),
            ),
        ] {
            let total: i64 = db.query_row(
                &format!("SELECT COUNT(*) FROM code_nodes n WHERE n.project = ?1 AND {clause}"),
                params![project, bind],
                |r| r.get(0),
            )?;
            if total == 0 {
                continue;
            }
            let sql = format!(
                "SELECT {COLS} FROM code_nodes n
                 WHERE n.project = ?1 AND {clause}
                 ORDER BY n.qualified_name LIMIT ?3"
            );
            let mut stmt = db.prepare(&sql)?;
            let rows: Vec<SymbolRow> = stmt
                .query_map(params![project, bind, limit as i64], row_to_symbol)?
                .collect::<std::result::Result<_, _>>()?;
            return Ok(Matches {
                total: total as usize,
                rows,
            });
        }
        Ok(Matches::default())
    }

    /// One hop out from a definition along the call graph.
    pub fn neighbors(&self, node_id: i64, dir: Direction) -> Result<Vec<Neighbor>> {
        let db = self.lock()?;
        self.neighbors_locked(&db, node_id, dir)
    }

    /// Restricted to CALLS. Inheritance and implementation edges are not call
    /// hops, and `fan_in`/`fan_out` count only calls — a traversal that mixed
    /// them would contradict the numbers printed beside it.
    fn neighbors_locked(
        &self,
        db: &Connection,
        node_id: i64,
        dir: Direction,
    ) -> Result<Vec<Neighbor>> {
        let (own, other) = match dir {
            Direction::Callees => ("source_id", "target_id"),
            Direction::Callers => ("target_id", "source_id"),
        };
        let sql = format!(
            "SELECT {COLS}, e.type, e.confidence, e.resolution, e.line
             FROM code_edges e JOIN code_nodes n ON n.id = e.{other}
             WHERE e.{own} = ?1 AND e.type = 'CALLS'
             ORDER BY e.line, n.qualified_name"
        );
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map(params![node_id], |r| {
            Ok(Neighbor {
                symbol: row_to_symbol(r)?,
                kind: r.get::<_, String>(COL_COUNT)?,
                confidence: r.get::<_, Option<f64>>(COL_COUNT + 1)?,
                resolution: r.get::<_, Option<String>>(COL_COUNT + 2)?,
                line: r.get::<_, i64>(COL_COUNT + 3)? as u32,
                depth: 1,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Breadth-first fan-out, nearest first. Bounded on both depth and total
    /// nodes so a hub cannot turn one question into the whole graph.
    pub fn trace(
        &self,
        node_id: i64,
        dir: Direction,
        max_depth: usize,
        limit: usize,
    ) -> Result<Reached> {
        let db = self.lock()?;
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        seen.insert(node_id);
        let mut frontier = vec![node_id];
        let mut out: Vec<Neighbor> = Vec::new();

        for depth in 1..=max_depth.max(1) {
            let mut next = Vec::new();
            for id in frontier.drain(..) {
                for mut n in self.neighbors_locked(&db, id, dir)? {
                    if !seen.insert(n.symbol.id) {
                        continue;
                    }
                    n.depth = depth;
                    next.push(n.symbol.id);
                    out.push(n);
                    // Stopping at the cap and exhausting the graph produce the
                    // same list, so the difference has to be reported.
                    if out.len() >= limit {
                        return Ok(Reached {
                            nodes: out,
                            truncated: true,
                        });
                    }
                }
            }
            if next.is_empty() {
                return Ok(Reached {
                    nodes: out,
                    truncated: false,
                });
            }
            frontier = next;
        }
        // The depth bound stopped the walk while a frontier remained.
        Ok(Reached {
            nodes: out,
            truncated: true,
        })
    }
}

/// Definitions matching a lookup, plus how many there were before the limit.
#[derive(Debug, Clone, Default)]
pub struct Matches {
    pub rows: Vec<SymbolRow>,
    pub total: usize,
}

impl Matches {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// More definitions matched than were returned.
    pub fn truncated(&self) -> bool {
        self.total > self.rows.len()
    }

    pub fn first(&self) -> Option<&SymbolRow> {
        self.rows.first()
    }
}

/// The result of a bounded walk, and whether a bound cut it short.
#[derive(Debug, Clone, Default)]
pub struct Reached {
    pub nodes: Vec<Neighbor>,
    pub truncated: bool,
}

/// Escape the wildcards SQLite's LIKE recognises, so a needle containing `%`
/// or `_` matches those characters instead of acting as a pattern.
fn escape_like(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len());
    for ch in needle.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Callers,
    Callees,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "callers" => Some(Direction::Callers),
            "callees" => Some(Direction::Callees),
            _ => None,
        }
    }
}

/// A definition as a query answers it, with the fan counts a caller wants.
#[derive(Debug, Clone)]
pub struct SymbolRow {
    pub id: i64,
    pub name: String,
    pub qualified_name: String,
    pub label: String,
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
    pub exported: bool,
    pub metrics: SymbolMetrics,
    pub fan_in: usize,
    pub fan_out: usize,
}

/// A definition reached across one edge.
#[derive(Debug, Clone)]
pub struct Neighbor {
    pub symbol: SymbolRow,
    pub kind: String,
    pub confidence: Option<f64>,
    pub resolution: Option<String>,
    pub line: u32,
    pub depth: usize,
}

/// The column list every symbol query selects, so `row_to_symbol` can read
/// them positionally and callers can append their own columns after.
const COLS: &str = "n.id, n.name, n.qualified_name, n.label, n.file_path, n.start_line, \
                    n.end_line, n.signature, n.exported, n.complexity, n.cognitive, \
                    n.loop_depth, n.param_count, \
                    (SELECT COUNT(*) FROM code_edges ce WHERE ce.target_id = n.id AND ce.type = 'CALLS'), \
                    (SELECT COUNT(*) FROM code_edges ce WHERE ce.source_id = n.id AND ce.type = 'CALLS')";

/// How many columns `COLS` selects; extra columns start here.
const COL_COUNT: usize = 15;

fn row_to_symbol(r: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolRow> {
    Ok(SymbolRow {
        id: r.get(0)?,
        name: r.get(1)?,
        qualified_name: r.get(2)?,
        label: r.get(3)?,
        file_path: r.get(4)?,
        start_line: r.get(5)?,
        end_line: r.get(6)?,
        signature: r.get(7)?,
        exported: r.get::<_, i64>(8)? != 0,
        metrics: SymbolMetrics {
            complexity: r.get(9)?,
            cognitive: r.get(10)?,
            loop_depth: r.get(11)?,
            param_count: r.get(12)?,
        },
        fan_in: r.get::<_, i64>(13)? as usize,
        fan_out: r.get::<_, i64>(14)? as usize,
    })
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

    /// A small graph: `caller` calls `helper`, plus an unrelated `Widget.open`.
    fn seeded() -> CodeGraphStore {
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
                sym("src/a.rs", None, "parse_config", SymbolLabel::Function),
                sym("src/a.rs", None, "helper", SymbolLabel::Function),
                sym("src/a.rs", Some("Widget"), "open", SymbolLabel::Method),
                sym("src/a.rs", None, "Widget", SymbolLabel::Struct),
            ],
        )
        .unwrap();
        s.rebuild_edges(
            "p",
            &[EdgeRecord {
                source_qualified: "src/a.rs:parse_config".into(),
                target_qualified: "src/a.rs:helper".into(),
                kind: EdgeKind::Calls,
                resolution: Some(Resolution::SameModule),
                line: 7,
            }],
        )
        .unwrap();
        s
    }

    #[test]
    fn search_finds_a_symbol_by_any_word_of_its_name() {
        let s = seeded();
        for term in ["parse_config", "parse", "config"] {
            let hits = s.search("p", term, None, 10).unwrap();
            assert!(
                hits.iter().any(|r| r.name == "parse_config"),
                "{term} found nothing"
            );
        }
        assert!(s
            .search("p", "nonexistent_zzz", None, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn search_can_be_narrowed_to_a_label_and_reports_fan_counts() {
        let s = seeded();
        let only_structs = s.search("p", "widget", Some("Struct"), 10).unwrap();
        assert!(only_structs.iter().all(|r| r.label == "Struct"));

        let helper = s
            .search("p", "helper", None, 10)
            .unwrap()
            .into_iter()
            .find(|r| r.name == "helper")
            .expect("helper is indexed");
        assert_eq!(helper.fan_in, 1, "helper is called once");
        assert_eq!(helper.fan_out, 0);
    }

    #[test]
    fn symbol_lookup_widens_from_exact_to_suffix() {
        let s = seeded();
        assert_eq!(s.symbol("p", "src/a.rs:helper", 5).unwrap().rows.len(), 1);
        assert_eq!(s.symbol("p", "helper", 5).unwrap().rows.len(), 1);
        // A container-qualified tail resolves through the suffix pass.
        let by_suffix = s.symbol("p", "Widget.open", 5).unwrap();
        assert_eq!(by_suffix.rows.len(), 1);
        assert_eq!(by_suffix.rows[0].qualified_name, "src/a.rs:Widget.open");
        assert!(s.symbol("p", "no_such_symbol", 5).unwrap().is_empty());
    }

    #[test]
    fn neighbors_read_both_directions() {
        let s = seeded();
        let caller = s.symbol("p", "parse_config", 1).unwrap().rows.remove(0);
        let callee = s.symbol("p", "helper", 1).unwrap().rows.remove(0);

        let out = s.neighbors(caller.id, Direction::Callees).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol.name, "helper");
        assert_eq!(out[0].resolution.as_deref(), Some("same_module"));

        let inbound = s.neighbors(callee.id, Direction::Callers).unwrap();
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].symbol.name, "parse_config");
        assert!(s
            .neighbors(callee.id, Direction::Callees)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn trace_is_bounded_by_depth_and_never_revisits() {
        let s = store();
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
        // A cycle: a -> b -> c -> a. Without a visited set this never ends.
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
                sym("src/a.rs", None, "a", SymbolLabel::Function),
                sym("src/a.rs", None, "b", SymbolLabel::Function),
                sym("src/a.rs", None, "c", SymbolLabel::Function),
            ],
        )
        .unwrap();
        let edge = |from: &str, to: &str| EdgeRecord {
            source_qualified: format!("src/a.rs:{from}"),
            target_qualified: format!("src/a.rs:{to}"),
            kind: EdgeKind::Calls,
            resolution: Some(Resolution::SameModule),
            line: 1,
        };
        s.rebuild_edges("p", &[edge("a", "b"), edge("b", "c"), edge("c", "a")])
            .unwrap();

        let a = s.symbol("p", "src/a.rs:a", 1).unwrap().rows.remove(0);
        let one = s.trace(a.id, Direction::Callees, 1, 50).unwrap().nodes;
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].symbol.name, "b");
        assert_eq!(one[0].depth, 1);

        let deep = s.trace(a.id, Direction::Callees, 5, 50).unwrap().nodes;
        assert_eq!(deep.len(), 2, "the cycle must not re-yield the origin");
        assert_eq!(deep.iter().map(|n| n.depth).max(), Some(2));

        let capped = s.trace(a.id, Direction::Callees, 5, 1).unwrap().nodes;
        assert_eq!(capped.len(), 1, "the total cap has to hold");
    }

    #[test]
    fn a_capped_lookup_still_reports_how_many_matched() {
        // A count that is really the limit reads as the whole truth, and a
        // model acts on it: it picks from the five it can see and never learns
        // the other twenty-five exist.
        let s = store();
        s.begin_project("p", Path::new("/tmp/p"), true).unwrap();
        let syms: Vec<SymbolRecord> = (0..30)
            .map(|i| {
                sym(
                    "src/a.rs",
                    Some(&format!("W{i:02}")),
                    "new",
                    SymbolLabel::Method,
                )
            })
            .collect();
        s.replace_file(
            "p",
            FileRecord {
                path: "src/a.rs",
                lang: Some("rust"),
                content_hash: "h",
                status: FileStatus::Indexed,
                detail: None,
            },
            &syms,
        )
        .unwrap();

        let found = s.symbol("p", "new", 5).unwrap();
        assert_eq!(found.rows.len(), 5);
        assert_eq!(found.total, 30, "the true count has to survive the limit");
        assert!(found.truncated());
    }

    #[test]
    fn the_suffix_tier_is_anchored_and_escapes_wildcards() {
        let s = seeded();
        // `open` must not match by sitting inside another identifier.
        s.replace_file(
            "p",
            FileRecord {
                path: "src/b.rs",
                lang: Some("rust"),
                content_hash: "h",
                status: FileStatus::Indexed,
                detail: None,
            },
            &[sym("src/b.rs", None, "reopen", SymbolLabel::Function)],
        )
        .unwrap();

        let found = s.symbol("p", "Widget.open", 5).unwrap();
        assert_eq!(found.total, 1);
        assert_eq!(found.rows[0].qualified_name, "src/a.rs:Widget.open");

        // A needle of pure wildcards must match nothing, not everything.
        assert_eq!(s.symbol("p", "%", 5).unwrap().total, 0);
        assert_eq!(s.symbol("p", "_pen", 5).unwrap().total, 0);
    }

    #[test]
    fn traversal_follows_calls_only_and_reports_being_cut_short() {
        let s = seeded();
        // An IMPLEMENTS edge is not a call hop, and fan-in counts only calls.
        s.rebuild_edges(
            "p",
            &[
                EdgeRecord {
                    source_qualified: "src/a.rs:parse_config".into(),
                    target_qualified: "src/a.rs:helper".into(),
                    kind: EdgeKind::Calls,
                    resolution: Some(Resolution::SameModule),
                    line: 7,
                },
                EdgeRecord {
                    source_qualified: "src/a.rs:parse_config".into(),
                    target_qualified: "src/a.rs:Widget".into(),
                    kind: EdgeKind::Implements,
                    resolution: None,
                    line: 0,
                },
            ],
        )
        .unwrap();

        let caller = s.symbol("p", "parse_config", 1).unwrap().rows.remove(0);
        let out = s.neighbors(caller.id, Direction::Callees).unwrap();
        assert_eq!(out.len(), 1, "only the CALLS edge is a callee");
        assert_eq!(out[0].symbol.name, "helper");

        let full = s.trace(caller.id, Direction::Callees, 3, 50).unwrap();
        assert!(!full.truncated, "an exhausted walk is not truncated");
        let capped = s.trace(caller.id, Direction::Callees, 3, 1).unwrap();
        assert!(capped.truncated, "hitting the cap has to be visible");
    }

    #[test]
    fn projects_and_schema_version_are_readable() {
        let s = seeded();
        assert_eq!(s.projects().unwrap(), vec!["p".to_string()]);
        assert_eq!(
            s.stored_schema_version().unwrap(),
            Some(CodeGraphStore::expected_schema_version())
        );
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
