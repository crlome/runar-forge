use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use uuid::Uuid;

use super::{MemoryStorage, StorageError, StorageResult};
use crate::types::*;

pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "001_initial_schema",
        include_str!("sql/001_initial_schema.sql"),
    ),
    ("002_add_sessions", include_str!("sql/002_add_sessions.sql")),
    (
        "003_add_embeddings",
        include_str!("sql/003_add_embeddings.sql"),
    ),
    (
        "004_add_memory_edges",
        include_str!("sql/004_add_memory_edges.sql"),
    ),
    (
        "005_add_debug_log",
        include_str!("sql/005_add_debug_log.sql"),
    ),
    (
        "006_add_confidence",
        include_str!("sql/006_add_confidence.sql"),
    ),
    (
        "007_add_topic_key",
        include_str!("sql/007_add_topic_key.sql"),
    ),
    (
        "008_add_pending_observations",
        include_str!("sql/008_add_pending_observations.sql"),
    ),
    ("009_add_verified", include_str!("sql/009_add_verified.sql")),
    (
        "010_add_sync_outbox",
        include_str!("sql/010_add_sync_outbox.sql"),
    ),
    ("011_add_author", include_str!("sql/011_add_author.sql")),
    (
        "012_scope_project_namespaces",
        include_str!("sql/012_scope_project_namespaces.sql"),
    ),
    (
        "013_content_hash_fts_fix",
        include_str!("sql/013_content_hash_fts_fix.sql"),
    ),
];

pub struct SqliteAdapter {
    db: Mutex<Connection>,
    default_namespace: String,
}

impl SqliteAdapter {
    pub fn new(path: &str, namespace: &str) -> StorageResult<Self> {
        let path = PathBuf::from(shellexpand(path));

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StorageError::Init(e.to_string()))?;
        }

        let conn = Connection::open(&path).map_err(|e| StorageError::Init(e.to_string()))?;

        // journal_mode returns a result row, so use query_row instead of execute_batch
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(|e| StorageError::Init(e.to_string()))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| StorageError::Init(e.to_string()))?;

        Ok(Self {
            db: Mutex::new(conn),
            default_namespace: namespace.to_string(),
        })
    }

    pub fn in_memory(namespace: &str) -> StorageResult<Self> {
        let conn = Connection::open_in_memory().map_err(|e| StorageError::Init(e.to_string()))?;

        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| StorageError::Init(e.to_string()))?;

        Ok(Self {
            db: Mutex::new(conn),
            default_namespace: namespace.to_string(),
        })
    }

    fn run_migrations(conn: &Connection) -> StorageResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version    TEXT PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .map_err(|e| StorageError::Init(e.to_string()))?;

        let applied: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT version FROM schema_migrations ORDER BY version")
                .map_err(|e| StorageError::Init(e.to_string()))?;
            let rows: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .map_err(|e| StorageError::Init(e.to_string()))?
                .filter_map(|r| r.ok())
                .collect();
            rows
        };

        for (version, sql) in MIGRATIONS {
            if applied.contains(&version.to_string()) {
                continue;
            }
            tracing::info!(version, "applying migration");
            // One transaction per migration: the statements and the
            // schema_migrations record commit together, so a crash mid-batch
            // can never leave a half-applied migration that bricks
            // initialize() on the next start (e.g. re-running an
            // ALTER TABLE ADD COLUMN that already committed).
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| StorageError::Init(e.to_string()))?;
            tx.execute_batch(sql)
                .map_err(|e| StorageError::Init(format!("migration {version}: {e}")))?;
            tx.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                params![version],
            )
            .map_err(|e| StorageError::Init(e.to_string()))?;
            tx.commit().map_err(|e| StorageError::Init(e.to_string()))?;
        }

        Ok(())
    }

    /// Phase 5.6.2 — full-row replacement used by `apply_remote_entry`
    /// when the resolver picks Update. Distinct from `update()`
    /// (partial JSON patch) and `import_entry()` (insert-or-skip).
    async fn replace_remote_entry(&self, entry: &MemoryEntry) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let source_str = serde_json::to_value(entry.source)
            .unwrap()
            .as_str()
            .unwrap_or("human")
            .to_string();
        let type_str = entry.entry_type.as_str();
        let tags_json = serde_json::to_string(&entry.tags).unwrap_or_else(|_| "[]".into());
        let layer_val = entry.layer.value() as i32;
        let confidence = entry.confidence.clamp(0.0, 1.0) as f64;
        let verified = if entry.verified { 1 } else { 0 };

        db.execute(
            "INSERT INTO memory_entries
                (id, namespace, title, content, type, tags, project_id,
                 source, layer, confidence, topic_key, access_count,
                 verified, verified_at, author, verified_by,
                 created_at, updated_at, last_accessed_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
             ON CONFLICT(id) DO UPDATE SET
                namespace = excluded.namespace,
                title = excluded.title,
                content = excluded.content,
                type = excluded.type,
                tags = excluded.tags,
                project_id = excluded.project_id,
                source = excluded.source,
                layer = excluded.layer,
                confidence = excluded.confidence,
                topic_key = excluded.topic_key,
                access_count = excluded.access_count,
                verified = excluded.verified,
                verified_at = excluded.verified_at,
                author = excluded.author,
                verified_by = excluded.verified_by,
                updated_at = excluded.updated_at,
                last_accessed_at = excluded.last_accessed_at,
                deleted_at = excluded.deleted_at",
            params![
                entry.id.to_string(),
                entry.namespace,
                entry.title,
                entry.content,
                type_str,
                tags_json,
                entry.project_id,
                source_str,
                layer_val,
                confidence,
                entry.topic_key,
                entry.access_count,
                verified,
                entry.verified_at.map(|t| t.to_rfc3339()),
                entry.author,
                entry.verified_by,
                entry.created_at.to_rfc3339(),
                entry.updated_at.to_rfc3339(),
                entry.last_accessed_at.map(|t| t.to_rfc3339()),
                entry.deleted_at.map(|t| t.to_rfc3339()),
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }
}

fn shellexpand(path: &str) -> String {
    if path.starts_with('~') {
        if let Some(home) = dirs_home() {
            return path.replacen('~', &home, 1);
        }
    }
    path.to_string()
}

fn dirs_home() -> Option<String> {
    // `dirs::home_dir` resolves `USERPROFILE` on Windows and `HOME` on
    // Unix, so `~` expansion in `RUNAR_SQLITE_PATH` works everywhere.
    dirs::home_dir().map(|p| p.to_string_lossy().into_owned())
}

fn db_err(e: rusqlite::Error) -> StorageError {
    StorageError::Database(e.to_string())
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<MemoryEntry> {
    let tags_json: String = row.get("tags")?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();

    let entry_type_str: String = row.get("type")?;
    let entry_type: EntryType = serde_json::from_value(serde_json::Value::String(entry_type_str))
        .unwrap_or(EntryType::Note);

    let source_str: String = row.get("source")?;
    let source: MemorySource = serde_json::from_value(serde_json::Value::String(source_str))
        .unwrap_or(MemorySource::Human);

    let layer_val: i32 = row.get("layer")?;
    let confidence: f64 = row.get("confidence").unwrap_or(DEFAULT_CONFIDENCE as f64);
    let topic_key: Option<String> = row.get("topic_key").ok();
    // A10 `verified` — SQLite stores bool as INTEGER 0/1.
    let verified: bool = row
        .get::<_, i32>("verified")
        .map(|n| n != 0)
        .unwrap_or(false);
    let verified_at: Option<DateTime<Utc>> = row
        .get::<_, Option<String>>("verified_at")
        .ok()
        .flatten()
        .map(parse_dt);
    let author: Option<String> = row.get::<_, Option<String>>("author").ok().flatten();
    let verified_by: Option<String> = row.get::<_, Option<String>>("verified_by").ok().flatten();

    Ok(MemoryEntry {
        id: row.get::<_, String>("id")?.parse().unwrap_or_default(),
        title: row.get("title")?,
        content: row.get("content")?,
        entry_type,
        source,
        tags,
        namespace: row.get("namespace")?,
        project_id: row.get("project_id")?,
        topic_key,
        layer: MemoryLayer::from(layer_val as u8),
        importance: 0.5,
        decay_score: 1.0,
        access_count: row.get("access_count")?,
        confidence: confidence as f32,
        embedding: None,
        verified,
        verified_at,
        author,
        verified_by,
        created_at: parse_dt(row.get::<_, String>("created_at")?),
        updated_at: parse_dt(row.get::<_, String>("updated_at")?),
        last_accessed_at: row
            .get::<_, Option<String>>("last_accessed_at")?
            .map(parse_dt),
        deleted_at: row.get::<_, Option<String>>("deleted_at")?.map(parse_dt),
    })
}

fn parse_dt(s: String) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| s.parse::<chrono::DateTime<Utc>>())
        .unwrap_or_else(|_| Utc::now())
}

fn parse_opt_ts(s: Option<String>) -> Option<chrono::DateTime<Utc>> {
    s.and_then(|raw| {
        chrono::DateTime::parse_from_rfc3339(&raw)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    })
}

fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<Session> {
    let status_str: String = row.get("status")?;
    let status: SessionStatus = serde_json::from_value(serde_json::Value::String(status_str))
        .unwrap_or(SessionStatus::Active);

    Ok(Session {
        id: row.get::<_, String>("id")?.parse().unwrap_or_default(),
        namespace: row.get("namespace")?,
        project_id: row.get("project_id")?,
        tool: row.get("tool")?,
        goal: row.get("goal")?,
        summary: row.get("summary")?,
        discoveries: row
            .get::<_, Option<String>>("discoveries")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        files_modified: row
            .get::<_, Option<String>>("files_modified")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        status,
        started_at: parse_dt(row.get::<_, String>("started_at")?),
        ended_at: row.get::<_, Option<String>>("ended_at")?.map(parse_dt),
    })
}

#[async_trait]
impl MemoryStorage for SqliteAdapter {
    async fn initialize(&self) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Init(e.to_string()))?;
        Self::run_migrations(&db)
    }

    async fn close(&self) -> StorageResult<()> {
        Ok(())
    }

    async fn save(&self, input: MemoryEntryInput, namespace: &str) -> StorageResult<SaveResult> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let tags_json = serde_json::to_string(&input.tags).unwrap_or_else(|_| "[]".into());
        let source = input.source.unwrap_or(MemorySource::Human);
        let ns = if namespace.is_empty() {
            &self.default_namespace
        } else {
            namespace
        };

        let confidence = input
            .confidence
            .unwrap_or(DEFAULT_CONFIDENCE)
            .clamp(0.0, 1.0);

        // Exact-duplicate guard: identical content in the same namespace
        // short-circuits before the topic_key branch, so an unchanged
        // recrawl never soft-deletes-and-reinserts its own predecessor.
        let hash = crate::storage::content_hash(&input.title, &input.content);
        let existing_dup: Option<String> = db
            .query_row(
                "SELECT id FROM memory_entries
                 WHERE namespace = ?1 AND content_hash = ?2 AND deleted_at IS NULL
                 LIMIT 1",
                params![ns, hash],
                |row| row.get(0),
            )
            .ok();
        if let Some(dup_id) = existing_dup {
            db.execute(
                "UPDATE memory_entries SET updated_at = ?1 WHERE id = ?2",
                params![now, dup_id],
            )
            .map_err(db_err)?;
            return Ok(SaveResult {
                id: dup_id.parse().unwrap_or_default(),
                action: SaveAction::Duplicate,
                superseded: None,
            });
        }

        // Phase 5.1.2 — topicKey upsert. If a live entry with the same
        // (namespace, topic_key) exists, soft-delete it and surface its
        // metadata so the caller can report supersession.
        let superseded = match input.topic_key.as_deref() {
            Some(tk) if !tk.is_empty() => {
                let existing: Option<(String, String, String)> = db
                    .query_row(
                        "SELECT id, title, created_at FROM memory_entries
                         WHERE namespace = ?1 AND topic_key = ?2 AND deleted_at IS NULL
                         ORDER BY created_at DESC
                         LIMIT 1",
                        params![ns, tk],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        },
                    )
                    .ok();

                if let Some((old_id_str, old_title, old_created)) = existing {
                    db.execute(
                        "UPDATE memory_entries SET deleted_at = ?1 WHERE id = ?2",
                        params![now, old_id_str],
                    )
                    .map_err(db_err)?;

                    Some(SupersededEntry {
                        id: old_id_str.parse().unwrap_or_default(),
                        title: old_title,
                        created_at: parse_dt(old_created),
                    })
                } else {
                    None
                }
            }
            _ => None,
        };

        db.execute(
            "INSERT INTO memory_entries (id, namespace, title, content, type, tags, project_id, source, layer, access_count, confidence, topic_key, author, content_hash, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?12, ?13, ?14, ?14)",
            params![
                id.to_string(),
                ns,
                input.title,
                input.content,
                input.entry_type.as_str(),
                tags_json,
                input.project_id,
                serde_json::to_value(source).unwrap().as_str().unwrap_or("human"),
                MemoryLayer::WORKING.value(),
                confidence as f64,
                input.topic_key,
                input.author,
                hash,
                now,
            ],
        )
        .map_err(db_err)?;

        let action = if superseded.is_some() {
            SaveAction::Updated
        } else {
            SaveAction::Created
        };

        Ok(SaveResult {
            id,
            action,
            superseded,
        })
    }

    async fn get(&self, id: Uuid) -> StorageResult<MemoryEntry> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        db.query_row(
            "SELECT * FROM memory_entries WHERE id = ?1 AND deleted_at IS NULL",
            params![id.to_string()],
            row_to_entry,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StorageError::NotFound(id),
            _ => db_err(e),
        })
    }

    async fn update(&self, id: Uuid, updates: serde_json::Value) -> StorageResult<MemoryEntry> {
        {
            let db = self
                .db
                .lock()
                .map_err(|e| StorageError::Database(e.to_string()))?;
            let now = Utc::now().to_rfc3339();

            if let Some(obj) = updates.as_object() {
                let mut sets = vec!["updated_at = ?1".to_string()];
                let mut idx = 2usize;

                for key in obj.keys() {
                    match key.as_str() {
                        "title" | "content" | "tags" | "layer" | "access_count"
                        | "last_accessed_at" => {
                            sets.push(format!("{key} = ?{idx}"));
                            idx += 1;
                        }
                        _ => {}
                    }
                }

                let sql = format!(
                    "UPDATE memory_entries SET {} WHERE id = ?{idx} AND deleted_at IS NULL",
                    sets.join(", ")
                );

                let mut stmt = db.prepare(&sql).map_err(db_err)?;

                let mut param_idx = 1;
                stmt.raw_bind_parameter(param_idx, &now).map_err(db_err)?;
                param_idx += 1;

                for key in obj.keys() {
                    match key.as_str() {
                        "title" | "content" | "tags" | "last_accessed_at" => {
                            let val = obj[key].as_str().unwrap_or_default();
                            stmt.raw_bind_parameter(param_idx, val).map_err(db_err)?;
                            param_idx += 1;
                        }
                        "layer" | "access_count" => {
                            let val = obj[key].as_i64().unwrap_or(0);
                            stmt.raw_bind_parameter(param_idx, val).map_err(db_err)?;
                            param_idx += 1;
                        }
                        _ => {}
                    }
                }

                stmt.raw_bind_parameter(param_idx, id.to_string())
                    .map_err(db_err)?;
                stmt.raw_execute().map_err(db_err)?;

                // Content changed → the dedup hash must follow it, or the
                // stale hash would both miss real duplicates and collide
                // with rows that no longer match.
                if obj.contains_key("title") || obj.contains_key("content") {
                    let fresh: Option<(String, String)> = db
                        .query_row(
                            "SELECT title, content FROM memory_entries WHERE id = ?1",
                            params![id.to_string()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .ok();
                    if let Some((t, c)) = fresh {
                        let hash = crate::storage::content_hash(&t, &c);
                        db.execute(
                            "UPDATE memory_entries SET content_hash = ?1 WHERE id = ?2",
                            params![hash, id.to_string()],
                        )
                        .map_err(db_err)?;
                    }
                }
            }
        } // MutexGuard dropped here before await

        self.get(id).await
    }

    async fn delete(&self, id: Uuid) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        db.execute(
            "UPDATE memory_entries SET deleted_at = ?1 WHERE id = ?2",
            params![now, id.to_string()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    async fn list(&self, filters: ListFilters) -> StorageResult<Vec<MemoryEntry>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let ns = filters
            .namespace
            .as_deref()
            .unwrap_or(&self.default_namespace);
        let limit = filters.limit.unwrap_or(50);
        let offset = filters.offset.unwrap_or(0);

        let mut sql = String::from("SELECT * FROM memory_entries WHERE namespace = ?1");
        if !filters.include_deleted {
            sql.push_str(" AND deleted_at IS NULL");
        }
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(ns.to_string())];

        if let Some(ref t) = filters.entry_type {
            params_vec.push(Box::new(t.as_str().to_string()));
            sql.push_str(&format!(" AND type = ?{}", params_vec.len()));
        }
        if let Some(ref pid) = filters.project_id {
            params_vec.push(Box::new(pid.clone()));
            sql.push_str(&format!(" AND project_id = ?{}", params_vec.len()));
        }

        sql.push_str(&format!(
            " ORDER BY created_at DESC LIMIT {} OFFSET {}",
            limit, offset
        ));

        let mut stmt = db.prepare(&sql).map_err(db_err)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let entries = stmt
            .query_map(params_refs.as_slice(), row_to_entry)
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(entries)
    }

    async fn save_embedding(&self, entry_id: Uuid, embedding: &[f32]) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let emb_json = serde_json::to_string(embedding)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        db.execute(
            "INSERT OR REPLACE INTO memory_embeddings (entry_id, embedding, created_at)
             VALUES (?1, ?2, ?3)",
            params![entry_id.to_string(), emb_json, now],
        )
        .map_err(db_err)?;
        Ok(())
    }

    async fn search(&self, query: SearchQuery) -> StorageResult<Vec<MemoryEntry>> {
        self.fts_search(query).await
    }

    async fn semantic_search(
        &self,
        query_embedding: &[f32],
        filters: SearchQuery,
    ) -> StorageResult<Vec<MemoryEntry>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;

        let ns = match filters.namespace.as_deref() {
            Some(n) if !n.is_empty() => n,
            _ => &self.default_namespace,
        };
        let limit = filters.limit.unwrap_or(10);

        // Load candidate entries with embeddings for cosine similarity,
        // applying the same predicates as fts_search so both arms of the
        // fused search see an identically scoped corpus.
        let mut sql = String::from(
            "SELECT e.*, em.embedding
             FROM memory_entries e
             JOIN memory_embeddings em ON e.id = em.entry_id
             WHERE e.namespace = ?1 AND e.deleted_at IS NULL",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(ns.to_string())];
        if let Some(ref t) = filters.entry_type {
            sql.push_str(&format!(" AND e.type = ?{}", args.len() + 1));
            args.push(Box::new(t.as_str().to_string()));
        }
        if let Some(ref pid) = filters.project_id {
            sql.push_str(&format!(" AND e.project_id = ?{}", args.len() + 1));
            args.push(Box::new(pid.clone()));
        }

        let mut stmt = db.prepare(&sql).map_err(db_err)?;
        let params_ref: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();

        let rows: Vec<(MemoryEntry, Vec<f32>)> = stmt
            .query_map(params_ref.as_slice(), |row| {
                let entry = row_to_entry(row)?;
                let emb_json: String = row.get("embedding")?;
                let emb: Vec<f32> = serde_json::from_str(&emb_json).unwrap_or_default();
                Ok((entry, emb))
            })
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        let mut scored: Vec<(f64, MemoryEntry)> = rows
            .into_iter()
            .map(|(entry, emb)| {
                let score = cosine_similarity(query_embedding, &emb);
                (score, entry)
            })
            // Same relevance floor as the postgres arm (`>= 0.65`), so both
            // backends feed identically-filtered candidates into RRF fusion.
            .filter(|(score, _)| *score >= 0.65)
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }

    async fn fts_search(&self, query: SearchQuery) -> StorageResult<Vec<MemoryEntry>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let ns = query
            .namespace
            .as_deref()
            .unwrap_or(&self.default_namespace);
        let limit = query.limit.unwrap_or(10);

        let mut sql = String::from(
            "SELECT e.* FROM memory_entries e
             JOIN memory_fts ON memory_fts.rowid = e.rowid
             WHERE memory_fts MATCH ?1
               AND e.namespace = ?2
               AND e.deleted_at IS NULL",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(query.query.clone()),
            Box::new(ns.to_string()),
        ];
        if let Some(ref t) = query.entry_type {
            sql.push_str(&format!(" AND e.type = ?{}", args.len() + 1));
            args.push(Box::new(t.as_str().to_string()));
        }
        if let Some(ref pid) = query.project_id {
            sql.push_str(&format!(" AND e.project_id = ?{}", args.len() + 1));
            args.push(Box::new(pid.clone()));
        }
        sql.push_str(&format!(" ORDER BY rank LIMIT ?{}", args.len() + 1));
        args.push(Box::new(limit as i64));

        let mut stmt = db.prepare(&sql).map_err(db_err)?;
        let params_ref: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();

        let entries = stmt
            .query_map(params_ref.as_slice(), row_to_entry)
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(entries)
    }

    // ── Sessions ───────────────────────────────────────────────

    async fn create_session(&self, input: SessionInput, namespace: &str) -> StorageResult<Session> {
        let id = Uuid::new_v4();
        {
            let db = self
                .db
                .lock()
                .map_err(|e| StorageError::Database(e.to_string()))?;
            let now = Utc::now().to_rfc3339();
            let ns = if namespace.is_empty() {
                &self.default_namespace
            } else {
                namespace
            };

            db.execute(
                "INSERT INTO sessions (id, namespace, project_id, tool, goal, status, started_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?6, ?6)",
                params![
                    id.to_string(),
                    ns,
                    input.project_id,
                    input.tool,
                    input.goal,
                    now,
                ],
            )
            .map_err(db_err)?;
        } // MutexGuard dropped before await

        self.get_session(id).await
    }

    async fn get_session(&self, id: Uuid) -> StorageResult<Session> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        db.query_row(
            "SELECT * FROM sessions WHERE id = ?1",
            params![id.to_string()],
            row_to_session,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StorageError::NotFound(id),
            _ => db_err(e),
        })
    }

    async fn update_session(&self, id: Uuid, update: SessionUpdate) -> StorageResult<Session> {
        {
            let db = self
                .db
                .lock()
                .map_err(|e| StorageError::Database(e.to_string()))?;
            let now = Utc::now().to_rfc3339();

            if let Some(status) = update.status {
                let status_str = serde_json::to_value(status)
                    .unwrap()
                    .as_str()
                    .unwrap_or("active")
                    .to_string();
                db.execute(
                    "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
                    params![status_str, now, id.to_string()],
                )
                .map_err(db_err)?;
            }
            if let Some(ref summary) = update.summary {
                db.execute(
                    "UPDATE sessions SET summary = ?1, updated_at = ?2 WHERE id = ?3",
                    params![summary, now, id.to_string()],
                )
                .map_err(db_err)?;
            }
            if let Some(ref ended_at) = update.ended_at {
                db.execute(
                    "UPDATE sessions SET ended_at = ?1, updated_at = ?2 WHERE id = ?3",
                    params![ended_at.to_rfc3339(), now, id.to_string()],
                )
                .map_err(db_err)?;
            }
            if let Some(ref files) = update.files_modified {
                let encoded = serde_json::to_string(files)
                    .map_err(|e| StorageError::Database(e.to_string()))?;
                db.execute(
                    "UPDATE sessions SET files_modified = ?1, updated_at = ?2 WHERE id = ?3",
                    params![encoded, now, id.to_string()],
                )
                .map_err(db_err)?;
            }
            if let Some(ref goal) = update.goal {
                db.execute(
                    "UPDATE sessions SET goal = ?1, updated_at = ?2 WHERE id = ?3",
                    params![goal, now, id.to_string()],
                )
                .map_err(db_err)?;
            }
            if let Some(ref discoveries) = update.discoveries {
                let encoded = serde_json::to_string(discoveries)
                    .map_err(|e| StorageError::Database(e.to_string()))?;
                db.execute(
                    "UPDATE sessions SET discoveries = ?1, updated_at = ?2 WHERE id = ?3",
                    params![encoded, now, id.to_string()],
                )
                .map_err(db_err)?;
            }
        } // MutexGuard dropped before await

        self.get_session(id).await
    }

    async fn list_sessions(&self, namespace: &str, limit: usize) -> StorageResult<Vec<Session>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let ns = if namespace.is_empty() {
            &self.default_namespace
        } else {
            namespace
        };

        let mut stmt = db
            .prepare(
                "SELECT * FROM sessions WHERE namespace = ?1 ORDER BY started_at DESC LIMIT ?2",
            )
            .map_err(db_err)?;

        let sessions = stmt
            .query_map(params![ns, limit as i64], row_to_session)
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(sessions)
    }

    // ── Edges ──────────────────────────────────────────────────

    async fn save_edge(&self, input: MemoryEdgeInput) -> StorageResult<MemoryEdge> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let edge_type_str = serde_json::to_value(input.edge_type)
            .unwrap()
            .as_str()
            .unwrap_or("related")
            .to_string();

        db.execute(
            "INSERT INTO memory_edges (id, from_id, to_id, type, strength, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.to_string(),
                input.from_id.to_string(),
                input.to_id.to_string(),
                edge_type_str,
                input.strength,
                now,
            ],
        )
        .map_err(db_err)?;

        Ok(MemoryEdge {
            id,
            from_id: input.from_id,
            to_id: input.to_id,
            edge_type: input.edge_type,
            strength: input.strength,
            created_at: Utc::now(),
        })
    }

    async fn get_edges(
        &self,
        entry_id: Uuid,
        direction: Option<&str>,
    ) -> StorageResult<Vec<MemoryEdge>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let id_str = entry_id.to_string();

        let sql = match direction.unwrap_or("both") {
            "from" => "SELECT * FROM memory_edges WHERE from_id = ?1",
            "to" => "SELECT * FROM memory_edges WHERE to_id = ?1",
            _ => "SELECT * FROM memory_edges WHERE from_id = ?1 OR to_id = ?1",
        };

        let mut stmt = db.prepare(sql).map_err(db_err)?;
        let edges = stmt
            .query_map(params![id_str], |row| {
                let edge_type_str: String = row.get("type")?;
                let edge_type: EdgeType =
                    serde_json::from_value(serde_json::Value::String(edge_type_str))
                        .unwrap_or(EdgeType::Related);

                Ok(MemoryEdge {
                    id: row.get::<_, String>("id")?.parse().unwrap_or_default(),
                    from_id: row.get::<_, String>("from_id")?.parse().unwrap_or_default(),
                    to_id: row.get::<_, String>("to_id")?.parse().unwrap_or_default(),
                    edge_type,
                    strength: row.get("strength")?,
                    created_at: parse_dt(row.get::<_, String>("created_at")?),
                })
            })
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(edges)
    }

    async fn delete_edge(&self, id: Uuid) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        db.execute(
            "DELETE FROM memory_edges WHERE id = ?1",
            params![id.to_string()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    // ── Debug ──────────────────────────────────────────────────

    async fn write_debug_log(&self, input: DebugLogInput) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let event_str = serde_json::to_value(input.event)
            .unwrap()
            .as_str()
            .unwrap_or("search_scoring")
            .to_string();
        let data_str = serde_json::to_string(&input.data).unwrap_or_else(|_| "{}".into());

        db.execute(
            "INSERT INTO debug_log (id, event, entry_id, data, duration_ms, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.to_string(),
                event_str,
                input.entry_id.map(|id| id.to_string()),
                data_str,
                input.duration_ms,
                now,
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    async fn query_debug_log(&self, query: DebugLogQuery) -> StorageResult<Vec<DebugLogEntry>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let limit = query.limit.unwrap_or(20);

        let mut sql = String::from("SELECT * FROM debug_log WHERE 1=1");
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];

        if let Some(ref event) = query.event {
            let event_str = serde_json::to_value(event)
                .unwrap()
                .as_str()
                .unwrap_or("")
                .to_string();
            params_vec.push(Box::new(event_str));
            sql.push_str(&format!(" AND event = ?{}", params_vec.len()));
        }
        if let Some(ref entry_id) = query.entry_id {
            params_vec.push(Box::new(entry_id.to_string()));
            sql.push_str(&format!(" AND entry_id = ?{}", params_vec.len()));
        }
        if let Some(ref since) = query.since {
            params_vec.push(Box::new(since.to_rfc3339()));
            sql.push_str(&format!(" AND created_at >= ?{}", params_vec.len()));
        }

        sql.push_str(&format!(" ORDER BY created_at DESC LIMIT {limit}"));

        let mut stmt = db.prepare(&sql).map_err(db_err)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let entries = stmt
            .query_map(params_refs.as_slice(), |row| {
                let event_str: String = row.get("event")?;
                let event: DebugEvent =
                    serde_json::from_value(serde_json::Value::String(event_str))
                        .unwrap_or(DebugEvent::SearchScoring);
                let data_str: String = row.get("data")?;
                let data: serde_json::Value =
                    serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null);

                Ok(DebugLogEntry {
                    id: row.get::<_, String>("id")?.parse().unwrap_or_default(),
                    event,
                    entry_id: row
                        .get::<_, Option<String>>("entry_id")?
                        .and_then(|s| s.parse().ok()),
                    data,
                    duration_ms: row.get::<_, Option<f64>>("duration_ms")?,
                    created_at: parse_dt(row.get::<_, String>("created_at")?),
                })
            })
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(entries)
    }

    async fn prune_debug_log(&self, older_than_days: i64) -> StorageResult<i64> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let cutoff = (Utc::now() - chrono::Duration::days(older_than_days)).to_rfc3339();
        let deleted = db
            .execute(
                "DELETE FROM debug_log WHERE created_at < ?1",
                params![cutoff],
            )
            .map_err(db_err)?;
        Ok(deleted as i64)
    }

    // ── Stats ──────────────────────────────────────────────────

    async fn get_stats(&self, namespace: &str) -> StorageResult<MemoryStats> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let ns = if namespace.is_empty() {
            &self.default_namespace
        } else {
            namespace
        };

        let total_entries: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM memory_entries WHERE namespace = ?1 AND deleted_at IS NULL",
                params![ns],
                |row| row.get(0),
            )
            .map_err(db_err)?;

        let total_sessions: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE namespace = ?1",
                params![ns],
                |row| row.get(0),
            )
            .map_err(db_err)?;

        let mut stmt = db
            .prepare(
                "SELECT type, COUNT(*) as cnt FROM memory_entries
                 WHERE namespace = ?1 AND deleted_at IS NULL GROUP BY type",
            )
            .map_err(db_err)?;
        let entries_by_type: Vec<(String, i64)> = stmt
            .query_map(params![ns], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        let mut stmt = db
            .prepare(
                "SELECT layer, COUNT(*) as cnt FROM memory_entries
                 WHERE namespace = ?1 AND deleted_at IS NULL GROUP BY layer",
            )
            .map_err(db_err)?;
        let entries_by_layer: Vec<(u8, i64)> = stmt
            .query_map(params![ns], |row| {
                let layer: i32 = row.get(0)?;
                Ok((layer as u8, row.get(1)?))
            })
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        let mut stmt = db
            .prepare("SELECT DISTINCT namespace FROM memory_entries WHERE deleted_at IS NULL")
            .map_err(db_err)?;
        let namespaces: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(MemoryStats {
            total_entries,
            total_sessions,
            entries_by_type,
            entries_by_layer,
            namespaces,
        })
    }

    async fn get_stats_all(&self) -> StorageResult<GlobalStats> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;

        let total_entries: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM memory_entries WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)?;
        let total_sessions: i64 = db
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .map_err(db_err)?;

        let mut stmt = db
            .prepare(
                "SELECT type, COUNT(*) FROM memory_entries
                 WHERE deleted_at IS NULL GROUP BY type ORDER BY COUNT(*) DESC",
            )
            .map_err(db_err)?;
        let entries_by_type: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        let mut stmt = db
            .prepare(
                "SELECT layer, COUNT(*) FROM memory_entries
                 WHERE deleted_at IS NULL GROUP BY layer ORDER BY layer",
            )
            .map_err(db_err)?;
        let entries_by_layer: Vec<(u8, i64)> = stmt
            .query_map([], |row| {
                let layer: i32 = row.get(0)?;
                Ok((layer as u8, row.get(1)?))
            })
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        // Entries and sessions grouped per namespace, merged in Rust.
        let mut by_ns: std::collections::BTreeMap<String, NamespaceStats> = Default::default();
        let mut stmt = db
            .prepare(
                "SELECT namespace, COUNT(*) FROM memory_entries
                 WHERE deleted_at IS NULL GROUP BY namespace",
            )
            .map_err(db_err)?;
        for (ns, n) in stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
            .map_err(db_err)?
            .filter_map(|r| r.ok())
        {
            by_ns
                .entry(ns.clone())
                .or_insert(NamespaceStats { namespace: ns, entries: 0, sessions: 0 })
                .entries = n;
        }
        let mut stmt = db
            .prepare("SELECT namespace, COUNT(*) FROM sessions GROUP BY namespace")
            .map_err(db_err)?;
        for (ns, n) in stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
            .map_err(db_err)?
            .filter_map(|r| r.ok())
        {
            by_ns
                .entry(ns.clone())
                .or_insert(NamespaceStats { namespace: ns, entries: 0, sessions: 0 })
                .sessions = n;
        }

        let mut by_namespace: Vec<NamespaceStats> = by_ns.into_values().collect();
        by_namespace.sort_by(|a, b| b.entries.cmp(&a.entries));

        Ok(GlobalStats {
            total_entries,
            total_sessions,
            entries_by_type,
            entries_by_layer,
            by_namespace,
        })
    }

    // ── Admin ──────────────────────────────────────────────────

    async fn count_project_namespace(&self, source: &str) -> StorageResult<MergeCounts> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;

        let entries: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM memory_entries
                 WHERE (project_id = ?1 OR namespace = ?1) AND deleted_at IS NULL",
                params![source],
                |row| row.get(0),
            )
            .map_err(db_err)?;

        let sessions: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sessions
                 WHERE project_id = ?1 OR namespace = ?1",
                params![source],
                |row| row.get(0),
            )
            .map_err(db_err)?;

        Ok(MergeCounts { entries, sessions })
    }

    async fn merge_project_namespace(
        &self,
        source: &str,
        target: &str,
    ) -> StorageResult<MergeCounts> {
        if source == target {
            return Ok(MergeCounts::default());
        }

        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let now = Utc::now().to_rfc3339();

        let entries = db
            .execute(
                "UPDATE memory_entries
                 SET project_id = ?1, namespace = ?1, updated_at = ?2
                 WHERE (project_id = ?3 OR namespace = ?3) AND deleted_at IS NULL",
                params![target, now, source],
            )
            .map_err(db_err)? as i64;

        let sessions = db
            .execute(
                "UPDATE sessions
                 SET project_id = ?1, namespace = ?1, updated_at = ?2
                 WHERE project_id = ?3 OR namespace = ?3",
                params![target, now, source],
            )
            .map_err(db_err)? as i64;

        Ok(MergeCounts { entries, sessions })
    }

    // ── Pending Observations ──────────────────────────────────────

    async fn enqueue_observation(
        &self,
        obs: ObservationInput,
        namespace: &str,
    ) -> StorageResult<Uuid> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let session_id_str = obs.session_id.map(|u| u.to_string());
        let tool_input_str = serde_json::to_string(&obs.tool_input)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        let tool_response_str = serde_json::to_string(&obs.tool_response)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        db.execute(
            "INSERT INTO pending_observations
             (id, namespace, session_id, project_id, tool_name,
              tool_input, tool_response, content_hash, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)",
            params![
                id.to_string(),
                namespace,
                session_id_str,
                obs.project_id,
                obs.tool_name,
                tool_input_str,
                tool_response_str,
                obs.content_hash,
                now,
            ],
        )
        .map_err(db_err)?;

        Ok(id)
    }

    async fn claim_observations(
        &self,
        namespace: &str,
        session_id: Option<Uuid>,
        max: usize,
    ) -> StorageResult<Vec<PendingObservation>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let session_str = session_id.map(|u| u.to_string());

        // SQLite lacks SKIP LOCKED; the Mutex<Connection> serializes writers.
        let select_sql = if session_str.is_some() {
            "SELECT id FROM pending_observations
             WHERE namespace = ?1 AND status = 'pending' AND session_id = ?2
             ORDER BY created_at
             LIMIT ?3"
        } else {
            "SELECT id FROM pending_observations
             WHERE namespace = ?1 AND status = 'pending'
             ORDER BY created_at
             LIMIT ?2"
        };

        let ids: Vec<String> = if let Some(ref sid) = session_str {
            let mut stmt = db.prepare(select_sql).map_err(db_err)?;
            let rows = stmt
                .query_map(params![namespace, sid, max as i64], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(db_err)?;
            rows.filter_map(|r| r.ok()).collect()
        } else {
            let mut stmt = db.prepare(select_sql).map_err(db_err)?;
            let rows = stmt
                .query_map(params![namespace, max as i64], |r| r.get::<_, String>(0))
                .map_err(db_err)?;
            rows.filter_map(|r| r.ok()).collect()
        };

        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // Bulk-claim by primary key — transactional in a single statement.
        let placeholders = vec!["?"; ids.len()].join(",");
        let update_sql = format!(
            "UPDATE pending_observations
             SET status = 'processing', claimed_at = ?, attempt_count = attempt_count + 1
             WHERE id IN ({placeholders})"
        );
        let mut update_params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
        update_params.push(&now);
        for id in &ids {
            update_params.push(id);
        }
        db.execute(&update_sql, update_params.as_slice())
            .map_err(db_err)?;

        // Fetch claimed rows
        let select_full_sql = format!(
            "SELECT id, namespace, session_id, project_id, tool_name,
                    tool_input, tool_response, content_hash, status,
                    attempt_count, claimed_at, created_at, confirmed_at
             FROM pending_observations
             WHERE id IN ({placeholders})
             ORDER BY created_at"
        );
        let id_params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut stmt = db.prepare(&select_full_sql).map_err(db_err)?;
        let observations: Vec<PendingObservation> = stmt
            .query_map(id_params.as_slice(), row_to_pending_observation)
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(observations)
    }

    async fn confirm_observations(&self, ids: &[Uuid]) -> StorageResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let id_strs: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "UPDATE pending_observations
             SET status = 'confirmed', confirmed_at = ?
             WHERE id IN ({placeholders})"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
        params.push(&now);
        for s in &id_strs {
            params.push(s);
        }
        db.execute(&sql, params.as_slice()).map_err(db_err)?;
        Ok(())
    }

    async fn recover_stale_observations(&self, older_than_secs: i64) -> StorageResult<i64> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let cutoff = (Utc::now() - chrono::Duration::seconds(older_than_secs)).to_rfc3339();
        let affected = db
            .execute(
                "UPDATE pending_observations
                 SET status = 'pending', claimed_at = NULL
                 WHERE status = 'processing' AND claimed_at < ?1",
                params![cutoff],
            )
            .map_err(db_err)?;
        Ok(affected as i64)
    }

    async fn check_observation_duplicate(
        &self,
        content_hash: &str,
        window_secs: i64,
    ) -> StorageResult<bool> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let cutoff = (Utc::now() - chrono::Duration::seconds(window_secs)).to_rfc3339();
        let exists: bool = db
            .query_row(
                "SELECT 1 FROM pending_observations
                 WHERE content_hash = ?1 AND created_at > ?2
                 LIMIT 1",
                params![content_hash, cutoff],
                |_| Ok(true),
            )
            .unwrap_or(false);
        Ok(exists)
    }

    async fn touch_entries(&self, ids: &[Uuid]) -> StorageResult<i64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let placeholders: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 2)).collect();
        let sql = format!(
            "UPDATE memory_entries
             SET access_count = access_count + 1,
                 last_accessed_at = ?1,
                 updated_at = ?1,
                 layer = CASE WHEN layer > {episodic} THEN {episodic} ELSE layer END
             WHERE id IN ({}) AND deleted_at IS NULL",
            placeholders.join(", "),
            episodic = MemoryLayer::EPISODIC.value(),
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now)];
        for id in ids {
            args.push(Box::new(id.to_string()));
        }
        let params_ref: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let touched = db.execute(&sql, params_ref.as_slice()).map_err(db_err)?;
        Ok(touched as i64)
    }

    // ── Two-stage GC ──────────────────────────────────────────

    async fn soft_delete_stale_crawl(
        &self,
        namespace: &str,
        age_days: i64,
        max: usize,
        dry_run: bool,
    ) -> StorageResult<Vec<Uuid>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let cutoff = (Utc::now() - chrono::Duration::days(age_days)).to_rfc3339();
        let now = Utc::now().to_rfc3339();

        let ids: Vec<String> = {
            let mut stmt = db
                .prepare(
                    "SELECT id FROM memory_entries
                     WHERE namespace = ?1
                       AND deleted_at IS NULL
                       AND source = 'scout'
                       AND verified = 0
                       AND access_count = 0
                       AND last_accessed_at IS NULL
                       AND created_at < ?2
                     ORDER BY created_at
                     LIMIT ?3",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params![namespace, cutoff, max as i64], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(db_err)?;
            rows.filter_map(|r| r.ok()).collect()
        };

        if !dry_run && !ids.is_empty() {
            // Chunked: a single IN list can exceed SQLite's bound-parameter
            // limit (32,766) on real backlogs.
            for chunk in ids.chunks(500) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                let sql = format!(
                    "UPDATE memory_entries SET deleted_at = ?1, updated_at = ?1
                     WHERE id IN ({placeholders})"
                );
                let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now.clone())];
                for id in chunk {
                    args.push(Box::new(id.clone()));
                }
                let params_ref: Vec<&dyn rusqlite::ToSql> =
                    args.iter().map(|b| b.as_ref()).collect();
                db.execute(&sql, params_ref.as_slice()).map_err(db_err)?;
            }
        }

        Ok(ids.iter().filter_map(|s| s.parse().ok()).collect())
    }

    async fn purge_soft_deleted(
        &self,
        namespace: Option<&str>,
        older_than_days: i64,
        max: usize,
        dry_run: bool,
    ) -> StorageResult<Vec<Uuid>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let cutoff = (Utc::now() - chrono::Duration::days(older_than_days)).to_rfc3339();

        let ns_clause = if namespace.is_some() { " AND namespace = ?2" } else { "" };
        let select_sql = format!(
            "SELECT id FROM memory_entries
             WHERE deleted_at IS NOT NULL AND deleted_at < ?1{ns_clause}
             ORDER BY deleted_at
             LIMIT {max}"
        );
        let ids: Vec<String> = {
            let mut stmt = db.prepare(&select_sql).map_err(db_err)?;
            let rows = if let Some(ns) = namespace {
                stmt.query_map(params![cutoff, ns], |r| r.get::<_, String>(0))
                    .map_err(db_err)?
                    .filter_map(|r| r.ok())
                    .collect()
            } else {
                stmt.query_map(params![cutoff], |r| r.get::<_, String>(0))
                    .map_err(db_err)?
                    .filter_map(|r| r.ok())
                    .collect()
            };
            rows
        };

        if !dry_run && !ids.is_empty() {
            // Hard DELETE: fires the guarded memory_ad trigger (skips FTS
            // for already-de-indexed tombstones) and FK CASCADE removes
            // embeddings + edges. Chunked to stay under SQLite's
            // bound-parameter limit.
            for chunk in ids.chunks(500) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                let sql = format!("DELETE FROM memory_entries WHERE id IN ({placeholders})");
                let args: Vec<Box<dyn rusqlite::ToSql>> = chunk
                    .iter()
                    .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::ToSql>)
                    .collect();
                let params_ref: Vec<&dyn rusqlite::ToSql> =
                    args.iter().map(|b| b.as_ref()).collect();
                db.execute(&sql, params_ref.as_slice()).map_err(db_err)?;
            }

            // Queue hygiene rides along: confirmed observations are fully
            // processed and may predate enqueue-side redaction — purge the
            // ones older than the same grace window.
            db.execute(
                "DELETE FROM pending_observations
                 WHERE status = 'confirmed' AND created_at < ?1",
                params![cutoff],
            )
            .map_err(db_err)?;
        }

        Ok(ids.iter().filter_map(|s| s.parse().ok()).collect())
    }

    // ── Content-hash maintenance ──────────────────────────────

    async fn list_missing_content_hash(
        &self,
        limit: usize,
    ) -> StorageResult<Vec<(Uuid, String, String)>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let mut stmt = db
            .prepare(
                "SELECT id, title, content FROM memory_entries
                 WHERE content_hash IS NULL
                 LIMIT ?1",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .filter_map(|(id, title, content)| id.parse().ok().map(|u| (u, title, content)))
            .collect();
        Ok(rows)
    }

    async fn redact_entry_row(
        &self,
        id: Uuid,
        title: &str,
        content: &str,
        tags: &[String],
    ) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let tags_json =
            serde_json::to_string(tags).map_err(|e| StorageError::Serialization(e.to_string()))?;
        let hash = crate::storage::content_hash(title, content);
        db.execute(
            "UPDATE memory_entries
             SET title = ?1, content = ?2, tags = ?3, content_hash = ?4, updated_at = ?5
             WHERE id = ?6",
            params![title, content, tags_json, hash, now, id.to_string()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    async fn set_content_hash(&self, id: Uuid, hash: &str) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        db.execute(
            "UPDATE memory_entries SET content_hash = ?1 WHERE id = ?2",
            params![hash, id.to_string()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    async fn find_duplicate_clusters(
        &self,
        namespace: Option<&str>,
    ) -> StorageResult<Vec<DuplicateCluster>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;

        let ns_clause = if namespace.is_some() { " AND namespace = ?1" } else { "" };
        let sql = format!(
            "SELECT e.content_hash, e.id, e.namespace, e.title, e.access_count, e.verified, e.created_at
             FROM memory_entries e
             JOIN (SELECT namespace, content_hash FROM memory_entries
                   WHERE deleted_at IS NULL AND content_hash IS NOT NULL{ns_clause}
                   GROUP BY namespace, content_hash HAVING COUNT(*) > 1) d
               ON e.namespace = d.namespace AND e.content_hash = d.content_hash
             WHERE e.deleted_at IS NULL
             ORDER BY e.namespace, e.content_hash, e.access_count DESC, e.created_at DESC"
        );
        let mut stmt = db.prepare(&sql).map_err(db_err)?;

        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<(String, DupMember)> {
            let hash: String = row.get(0)?;
            let id: String = row.get(1)?;
            let namespace: String = row.get(2)?;
            let title: String = row.get(3)?;
            let access_count: i64 = row.get(4)?;
            let verified: bool = row.get::<_, i64>(5)? != 0;
            let created_at: String = row.get(6)?;
            Ok((
                hash,
                DupMember {
                    id: id.parse().unwrap_or_default(),
                    namespace,
                    title,
                    access_count,
                    verified,
                    created_at: parse_dt(created_at),
                },
            ))
        };

        let rows: Vec<(String, DupMember)> = if let Some(ns) = namespace {
            stmt.query_map(params![ns], map_row)
                .map_err(db_err)?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            stmt.query_map([], map_row)
                .map_err(db_err)?
                .filter_map(|r| r.ok())
                .collect()
        };

        // Group into clusters keyed by (namespace, hash); rows arrive sorted.
        let mut clusters: Vec<DuplicateCluster> = Vec::new();
        for (hash, member) in rows {
            match clusters.last_mut() {
                Some(c)
                    if c.content_hash == hash
                        && c.entries.first().map(|e| e.namespace.as_str())
                            == Some(member.namespace.as_str()) =>
                {
                    c.entries.push(member);
                }
                _ => clusters.push(DuplicateCluster {
                    content_hash: hash,
                    entries: vec![member],
                }),
            }
        }
        Ok(clusters)
    }

    // ── Verify ────────────────────────────────────────────────

    async fn mark_verified(
        &self,
        id: Uuid,
        verified_by: Option<&str>,
    ) -> StorageResult<MemoryEntry> {
        let now = Utc::now().to_rfc3339();
        let id_str = id.to_string();
        let verified_by_owned = verified_by.map(|s| s.to_string());
        {
            let db = self
                .db
                .lock()
                .map_err(|e| StorageError::Database(e.to_string()))?;
            let updated = db
                .execute(
                    "UPDATE memory_entries
                     SET verified = 1, verified_at = ?1, verified_by = ?2, updated_at = ?3
                     WHERE id = ?4 AND deleted_at IS NULL",
                    params![now, verified_by_owned, now, id_str],
                )
                .map_err(db_err)?;
            if updated == 0 {
                return Err(StorageError::NotFound(id));
            }
        }
        self.get(id).await
    }

    // ── Export / Import (A6) ──────────────────────────────────

    async fn evict_stale(
        &self,
        namespace: &str,
        age_days: i64,
        conf_cap: f32,
        max: usize,
    ) -> StorageResult<Vec<Uuid>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let cutoff = (Utc::now() - chrono::Duration::days(age_days)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        let archival = MemoryLayer::ARCHIVAL.value() as i32;

        // Select victim ids first; SQLite lacks UPDATE ... RETURNING across
        // versions so we do a two-step.
        let ids: Vec<String> = {
            let mut stmt = db
                .prepare(
                    "SELECT id FROM memory_entries
                     WHERE namespace = ?1
                       AND deleted_at IS NULL
                       AND layer = ?2
                       AND verified = 0
                       AND access_count = 0
                       AND confidence < ?3
                       AND COALESCE(last_accessed_at, created_at) < ?4
                     ORDER BY COALESCE(last_accessed_at, created_at)
                     LIMIT ?5",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map(
                    params![namespace, archival, conf_cap as f64, cutoff, max as i64],
                    |r| r.get::<_, String>(0),
                )
                .map_err(db_err)?;
            rows.filter_map(|r| r.ok()).collect()
        };

        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "UPDATE memory_entries
             SET deleted_at = ?, updated_at = ?
             WHERE id IN ({placeholders})"
        );
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 2);
        params_vec.push(&now);
        params_vec.push(&now);
        for s in &ids {
            params_vec.push(s);
        }
        db.execute(&sql, params_vec.as_slice()).map_err(db_err)?;

        Ok(ids.iter().filter_map(|s| s.parse().ok()).collect())
    }

    async fn list_all_edges(&self, limit: usize) -> StorageResult<Vec<MemoryEdge>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let mut stmt = db
            .prepare(
                "SELECT id, from_id, to_id, type, strength, created_at
                 FROM memory_edges
                 ORDER BY created_at
                 LIMIT ?1",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let type_str: String = row.get("type")?;
                let edge_type: EdgeType =
                    serde_json::from_value(serde_json::Value::String(type_str))
                        .unwrap_or(EdgeType::Related);
                let id_str: String = row.get("id")?;
                let from_str: String = row.get("from_id")?;
                let to_str: String = row.get("to_id")?;
                let strength: f64 = row.get("strength")?;
                let created_str: String = row.get("created_at")?;
                Ok(MemoryEdge {
                    id: id_str.parse().unwrap_or_default(),
                    from_id: from_str.parse().unwrap_or_default(),
                    to_id: to_str.parse().unwrap_or_default(),
                    edge_type,
                    strength,
                    created_at: parse_dt(created_str),
                })
            })
            .map_err(db_err)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    async fn import_edge(&self, edge: MemoryEdge) -> StorageResult<bool> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let edge_type_str = serde_json::to_value(edge.edge_type)
            .unwrap()
            .as_str()
            .unwrap_or("related")
            .to_string();
        let affected = db
            .execute(
                "INSERT OR IGNORE INTO memory_edges (id, from_id, to_id, type, strength, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    edge.id.to_string(),
                    edge.from_id.to_string(),
                    edge.to_id.to_string(),
                    edge_type_str,
                    edge.strength,
                    edge.created_at.to_rfc3339(),
                ],
            )
            .map_err(db_err)?;
        Ok(affected == 1)
    }

    async fn import_session(&self, session: Session) -> StorageResult<bool> {
        // Same invariant repair as import_entry: sessions live in
        // namespace == project_id.
        let mut session = session;
        if let Some(ref pid) = session.project_id {
            if !pid.is_empty() && session.namespace != *pid {
                session.namespace = pid.clone();
            }
        }
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let status_str = serde_json::to_value(session.status)
            .unwrap()
            .as_str()
            .unwrap_or("active")
            .to_string();
        let now = Utc::now().to_rfc3339();
        let affected = db
            .execute(
                "INSERT OR IGNORE INTO sessions
                    (id, namespace, project_id, tool, goal, summary, discoveries, files_modified,
                     status, started_at, ended_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    session.id.to_string(),
                    session.namespace,
                    session.project_id,
                    session.tool,
                    session.goal,
                    session.summary,
                    serde_json::to_string(&session.discoveries).unwrap_or_else(|_| "[]".into()),
                    serde_json::to_string(&session.files_modified).unwrap_or_else(|_| "[]".into()),
                    status_str,
                    session.started_at.to_rfc3339(),
                    session.ended_at.map(|t| t.to_rfc3339()),
                    now.clone(),
                    now,
                ],
            )
            .map_err(db_err)?;
        Ok(affected == 1)
    }

    // ── Phase 5.6 — Sync (outbox + state + conflicts) ─────────

    async fn enqueue_outbox(&self, op: crate::types::OutboxInput) -> StorageResult<Uuid> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let payload = serde_json::to_string(&op.row_payload)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
        db.execute(
            "INSERT INTO sync_outbox (id, entry_id, op_kind, row_payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id.to_string(),
                op.entry_id.to_string(),
                op.op_kind.as_str(),
                payload,
                now
            ],
        )
        .map_err(db_err)?;
        Ok(id)
    }

    async fn claim_outbox(&self, max: usize) -> StorageResult<Vec<crate::types::OutboxRow>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        // Two-step claim: select FIFO ids, then UPDATE claimed_at.
        // SQLite doesn't support `RETURNING` everywhere, so do it
        // inside an immediate transaction for atomicity.
        let mut rows: Vec<crate::types::OutboxRow> = Vec::with_capacity(max);
        let tx = db.unchecked_transaction().map_err(db_err)?;
        let ids: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id FROM sync_outbox
                     WHERE confirmed_at IS NULL AND claimed_at IS NULL
                     ORDER BY created_at ASC
                     LIMIT ?1",
                )
                .map_err(db_err)?;
            let mapped = stmt
                .query_map(params![max as i64], |r| r.get::<_, String>(0))
                .map_err(db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_err)?;
            mapped
        };
        for id in &ids {
            tx.execute(
                "UPDATE sync_outbox SET claimed_at = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(db_err)?;
        }
        if !ids.is_empty() {
            let placeholders = ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT id, entry_id, op_kind, row_payload, attempts,
                        last_error, claimed_at, confirmed_at, created_at
                 FROM sync_outbox WHERE id IN ({})",
                placeholders
            );
            let mut stmt = tx.prepare(&sql).map_err(db_err)?;
            let params_vec: Vec<&dyn rusqlite::ToSql> =
                ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let mapped = stmt
                .query_map(params_vec.as_slice(), |row| {
                    let id_str: String = row.get(0)?;
                    let entry_id_str: String = row.get(1)?;
                    let op_kind_str: String = row.get(2)?;
                    let payload_str: String = row.get(3)?;
                    let attempts: i32 = row.get(4)?;
                    let last_error: Option<String> = row.get(5)?;
                    let claimed_at: Option<String> = row.get(6)?;
                    let confirmed_at: Option<String> = row.get(7)?;
                    let created_at: String = row.get(8)?;
                    Ok((
                        id_str,
                        entry_id_str,
                        op_kind_str,
                        payload_str,
                        attempts,
                        last_error,
                        claimed_at,
                        confirmed_at,
                        created_at,
                    ))
                })
                .map_err(db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_err)?;
            for (id_s, eid_s, op_s, payload_s, attempts, last_err, claimed, confirmed, created) in
                mapped
            {
                let id =
                    Uuid::parse_str(&id_s).map_err(|e| StorageError::Database(e.to_string()))?;
                let entry_id =
                    Uuid::parse_str(&eid_s).map_err(|e| StorageError::Database(e.to_string()))?;
                let op_kind = crate::types::OutboxOp::parse(&op_s)
                    .ok_or_else(|| StorageError::Database(format!("bad op_kind {op_s}")))?;
                let row_payload: serde_json::Value = serde_json::from_str(&payload_s)
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                rows.push(crate::types::OutboxRow {
                    id,
                    entry_id,
                    op_kind,
                    row_payload,
                    attempts,
                    last_error: last_err,
                    claimed_at: claimed.and_then(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .ok()
                            .map(|t| t.with_timezone(&Utc))
                    }),
                    confirmed_at: confirmed.and_then(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .ok()
                            .map(|t| t.with_timezone(&Utc))
                    }),
                    created_at: DateTime::parse_from_rfc3339(&created)
                        .map(|t| t.with_timezone(&Utc))
                        .map_err(|e| StorageError::Database(e.to_string()))?,
                });
            }
        }
        tx.commit().map_err(db_err)?;
        // Preserve FIFO order from `ids` (SELECT IN list doesn't keep it).
        rows.sort_by_key(|a| a.created_at);
        Ok(rows)
    }

    async fn confirm_outbox(&self, ids: &[Uuid]) -> StorageResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        let tx = db.unchecked_transaction().map_err(db_err)?;
        for id in ids {
            tx.execute(
                "UPDATE sync_outbox SET confirmed_at = ?1 WHERE id = ?2",
                params![now, id.to_string()],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    async fn fail_outbox(&self, id: Uuid, err: &str) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        db.execute(
            "UPDATE sync_outbox
             SET attempts = attempts + 1, last_error = ?1, claimed_at = NULL
             WHERE id = ?2",
            params![err, id.to_string()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    async fn outbox_depth(&self) -> StorageResult<usize> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM sync_outbox WHERE confirmed_at IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        Ok(count as usize)
    }

    async fn gc_outbox(&self, older_than_secs: i64) -> StorageResult<i64> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let cutoff = (Utc::now() - chrono::Duration::seconds(older_than_secs)).to_rfc3339();
        let affected = db
            .execute(
                "DELETE FROM sync_outbox
                 WHERE confirmed_at IS NOT NULL AND confirmed_at < ?1",
                params![cutoff],
            )
            .map_err(db_err)?;
        Ok(affected as i64)
    }

    async fn read_sync_state(&self) -> StorageResult<crate::types::SyncState> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let row: Result<
            (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<i32>,
                Option<i32>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
            _,
        > = db.query_row(
            "SELECT last_pulled_updated_at, last_pulled_session_at,
                    last_pulled_edge_at, last_push_at, last_pull_at,
                    local_dim, remote_dim,
                    local_schema_version, remote_schema_version,
                    initialized_at
             FROM sync_state WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        );
        match row {
            Ok((lpu, lps, lpe, lpush, lpull, ld, rd, lsv, rsv, init)) => {
                Ok(crate::types::SyncState {
                    last_pulled_updated_at: parse_opt_ts(lpu),
                    last_pulled_session_at: parse_opt_ts(lps),
                    last_pulled_edge_at: parse_opt_ts(lpe),
                    last_push_at: parse_opt_ts(lpush),
                    last_pull_at: parse_opt_ts(lpull),
                    local_dim: ld,
                    remote_dim: rd,
                    local_schema_version: lsv,
                    remote_schema_version: rsv,
                    initialized_at: parse_opt_ts(init),
                })
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(crate::types::SyncState::default()),
            Err(e) => Err(db_err(e)),
        }
    }

    async fn write_sync_state(&self, state: &crate::types::SyncState) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        db.execute(
            "INSERT INTO sync_state (id, last_pulled_updated_at, last_pulled_session_at,
                last_pulled_edge_at, last_push_at, last_pull_at,
                local_dim, remote_dim, local_schema_version,
                remote_schema_version, initialized_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                last_pulled_updated_at = excluded.last_pulled_updated_at,
                last_pulled_session_at = excluded.last_pulled_session_at,
                last_pulled_edge_at = excluded.last_pulled_edge_at,
                last_push_at = excluded.last_push_at,
                last_pull_at = excluded.last_pull_at,
                local_dim = excluded.local_dim,
                remote_dim = excluded.remote_dim,
                local_schema_version = excluded.local_schema_version,
                remote_schema_version = excluded.remote_schema_version,
                initialized_at = excluded.initialized_at",
            params![
                state.last_pulled_updated_at.map(|t| t.to_rfc3339()),
                state.last_pulled_session_at.map(|t| t.to_rfc3339()),
                state.last_pulled_edge_at.map(|t| t.to_rfc3339()),
                state.last_push_at.map(|t| t.to_rfc3339()),
                state.last_pull_at.map(|t| t.to_rfc3339()),
                state.local_dim,
                state.remote_dim,
                state.local_schema_version,
                state.remote_schema_version,
                state.initialized_at.map(|t| t.to_rfc3339()),
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    async fn record_conflict(&self, c: &crate::types::SyncConflict) -> StorageResult<()> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let direction = serde_json::to_value(c.direction)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "push".into());
        let policy = serde_json::to_value(c.policy)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "lww".into());
        let winner = serde_json::to_value(c.winner_side)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "remote".into());
        db.execute(
            "INSERT OR IGNORE INTO sync_conflicts
                (id, entry_id, direction, policy, winner_side,
                 local_updated_at, remote_updated_at,
                 local_payload, remote_payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                c.id.to_string(),
                c.entry_id.to_string(),
                direction,
                policy,
                winner,
                c.local_updated_at.map(|t| t.to_rfc3339()),
                c.remote_updated_at.map(|t| t.to_rfc3339()),
                c.local_payload
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".into())),
                c.remote_payload
                    .as_ref()
                    .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".into())),
                c.created_at.to_rfc3339(),
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_conflicts(&self, limit: usize) -> StorageResult<Vec<crate::types::SyncConflict>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let mut stmt = db
            .prepare(
                "SELECT id, entry_id, direction, policy, winner_side,
                        local_updated_at, remote_updated_at,
                        local_payload, remote_payload, created_at
                 FROM sync_conflicts
                 ORDER BY created_at DESC
                 LIMIT ?1",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let id: String = row.get(0)?;
                let entry_id: String = row.get(1)?;
                let direction: String = row.get(2)?;
                let policy: String = row.get(3)?;
                let winner: String = row.get(4)?;
                let lupd: Option<String> = row.get(5)?;
                let rupd: Option<String> = row.get(6)?;
                let lpay: Option<String> = row.get(7)?;
                let rpay: Option<String> = row.get(8)?;
                let created: String = row.get(9)?;
                Ok((
                    id, entry_id, direction, policy, winner, lupd, rupd, lpay, rpay, created,
                ))
            })
            .map_err(db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_err)?;

        let mut out = Vec::with_capacity(rows.len());
        for (id, entry_id, direction, policy, winner, lupd, rupd, lpay, rpay, created) in rows {
            out.push(crate::types::SyncConflict {
                id: Uuid::parse_str(&id).map_err(|e| StorageError::Database(e.to_string()))?,
                entry_id: Uuid::parse_str(&entry_id)
                    .map_err(|e| StorageError::Database(e.to_string()))?,
                direction: serde_json::from_value(serde_json::Value::String(direction))
                    .map_err(|e| StorageError::Serialization(e.to_string()))?,
                policy: serde_json::from_value(serde_json::Value::String(policy))
                    .map_err(|e| StorageError::Serialization(e.to_string()))?,
                winner_side: serde_json::from_value(serde_json::Value::String(winner))
                    .map_err(|e| StorageError::Serialization(e.to_string()))?,
                local_updated_at: parse_opt_ts(lupd),
                remote_updated_at: parse_opt_ts(rupd),
                local_payload: lpay
                    .map(|s| serde_json::from_str(&s))
                    .transpose()
                    .map_err(|e| StorageError::Serialization(e.to_string()))?,
                remote_payload: rpay
                    .map(|s| serde_json::from_str(&s))
                    .transpose()
                    .map_err(|e| StorageError::Serialization(e.to_string()))?,
                created_at: DateTime::parse_from_rfc3339(&created)
                    .map(|t| t.with_timezone(&Utc))
                    .map_err(|e| StorageError::Database(e.to_string()))?,
            });
        }
        Ok(out)
    }

    async fn list_changed_since(
        &self,
        after: Option<chrono::DateTime<chrono::Utc>>,
        clock_skew_secs: i64,
        limit: usize,
        project_filter: Option<&str>,
    ) -> StorageResult<Vec<MemoryEntry>> {
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let upper_bound =
            (chrono::Utc::now() - chrono::Duration::seconds(clock_skew_secs)).to_rfc3339();

        // Three SQL variants depending on after / project_filter
        // combinations.  SQLite doesn't support ANY/$N parameter
        // arrays the way PG does, so just bind plain ?N.
        let rows: Vec<MemoryEntry> = match (after, project_filter) {
            (Some(cursor), Some(project)) => {
                let mut stmt = db
                    .prepare(
                        "SELECT * FROM memory_entries
                         WHERE updated_at > ?1
                           AND updated_at <= ?2
                           AND project_id = ?3
                         ORDER BY updated_at ASC, id ASC
                         LIMIT ?4",
                    )
                    .map_err(db_err)?;
                let mapped = stmt
                    .query_map(
                        params![cursor.to_rfc3339(), upper_bound, project, limit as i64],
                        row_to_entry,
                    )
                    .map_err(db_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                mapped
            }
            (Some(cursor), None) => {
                let mut stmt = db
                    .prepare(
                        "SELECT * FROM memory_entries
                         WHERE updated_at > ?1
                           AND updated_at <= ?2
                         ORDER BY updated_at ASC, id ASC
                         LIMIT ?3",
                    )
                    .map_err(db_err)?;
                let mapped = stmt
                    .query_map(
                        params![cursor.to_rfc3339(), upper_bound, limit as i64],
                        row_to_entry,
                    )
                    .map_err(db_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                mapped
            }
            (None, Some(project)) => {
                let mut stmt = db
                    .prepare(
                        "SELECT * FROM memory_entries
                         WHERE updated_at <= ?1
                           AND project_id = ?2
                         ORDER BY updated_at ASC, id ASC
                         LIMIT ?3",
                    )
                    .map_err(db_err)?;
                let mapped = stmt
                    .query_map(params![upper_bound, project, limit as i64], row_to_entry)
                    .map_err(db_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                mapped
            }
            (None, None) => {
                let mut stmt = db
                    .prepare(
                        "SELECT * FROM memory_entries
                         WHERE updated_at <= ?1
                         ORDER BY updated_at ASC, id ASC
                         LIMIT ?2",
                    )
                    .map_err(db_err)?;
                let mapped = stmt
                    .query_map(params![upper_bound, limit as i64], row_to_entry)
                    .map_err(db_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                mapped
            }
        };

        Ok(rows)
    }

    async fn apply_remote_entry(
        &self,
        entry: MemoryEntry,
    ) -> StorageResult<crate::types::ApplyOutcome> {
        use crate::sync::conflict::{build_audit, resolve, Resolution};
        use crate::types::{ConflictDirection, ConflictWinner};

        let existing = match self.get(entry.id).await {
            Ok(e) => Some(e),
            Err(StorageError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };

        let decision = resolve(existing.as_ref(), &entry);

        match &decision {
            Resolution::Insert => {
                self.import_entry(entry.clone()).await?;
            }
            Resolution::Update { policy, audit } => {
                self.replace_remote_entry(&entry).await?;
                if *audit {
                    let conflict = build_audit(
                        ConflictDirection::Pull,
                        *policy,
                        ConflictWinner::Remote,
                        existing.as_ref(),
                        Some(&entry),
                    );
                    let _ = self.record_conflict(&conflict).await;
                }
            }
            Resolution::Skip { policy, audit } => {
                if *audit {
                    let conflict = build_audit(
                        ConflictDirection::Pull,
                        *policy,
                        ConflictWinner::Local,
                        existing.as_ref(),
                        Some(&entry),
                    );
                    let _ = self.record_conflict(&conflict).await;
                }
            }
        }

        Ok(decision.outcome())
    }

    async fn import_entry(&self, entry: MemoryEntry) -> StorageResult<bool> {
        // Re-apply the migration-012 invariant on import: exports taken
        // before the backfill carry namespace='default' with a project_id,
        // and 012 never re-runs — restoring such rows verbatim would make
        // them unreachable through project-scoped reads.
        let mut entry = entry;
        if let Some(ref pid) = entry.project_id {
            if !pid.is_empty() && entry.namespace != *pid {
                entry.namespace = pid.clone();
            }
        }
        let db = self
            .db
            .lock()
            .map_err(|e| StorageError::Database(e.to_string()))?;
        let source_str = serde_json::to_value(entry.source)
            .unwrap()
            .as_str()
            .unwrap_or("human")
            .to_string();
        let type_str = entry.entry_type.as_str();
        let tags_json = serde_json::to_string(&entry.tags).unwrap_or_else(|_| "[]".into());
        let layer_val = entry.layer.value() as i32;
        let confidence = entry.confidence.clamp(0.0, 1.0) as f64;
        let verified = if entry.verified { 1 } else { 0 };

        let affected = db
            .execute(
                "INSERT OR IGNORE INTO memory_entries
                    (id, namespace, title, content, type, tags, project_id,
                     source, layer, confidence, topic_key, access_count,
                     verified, verified_at, author, verified_by,
                     created_at, updated_at, last_accessed_at, deleted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                         ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                params![
                    entry.id.to_string(),
                    entry.namespace,
                    entry.title,
                    entry.content,
                    type_str,
                    tags_json,
                    entry.project_id,
                    source_str,
                    layer_val,
                    confidence,
                    entry.topic_key,
                    entry.access_count,
                    verified,
                    entry.verified_at.map(|t| t.to_rfc3339()),
                    entry.author,
                    entry.verified_by,
                    entry.created_at.to_rfc3339(),
                    entry.updated_at.to_rfc3339(),
                    entry.last_accessed_at.map(|t| t.to_rfc3339()),
                    entry.deleted_at.map(|t| t.to_rfc3339()),
                ],
            )
            .map_err(db_err)?;
        Ok(affected == 1)
    }
}

fn row_to_pending_observation(row: &rusqlite::Row) -> rusqlite::Result<PendingObservation> {
    let id_str: String = row.get("id")?;
    let namespace: String = row.get("namespace")?;
    let session_id_str: Option<String> = row.get("session_id")?;
    let project_id: Option<String> = row.get("project_id")?;
    let tool_name: String = row.get("tool_name")?;
    let tool_input_str: String = row.get("tool_input")?;
    let tool_response_str: String = row.get("tool_response")?;
    let content_hash: String = row.get("content_hash")?;
    let status_str: String = row.get("status")?;
    let attempt_count: i32 = row.get("attempt_count")?;
    let claimed_at_str: Option<String> = row.get("claimed_at")?;
    let created_at_str: String = row.get("created_at")?;
    let confirmed_at_str: Option<String> = row.get("confirmed_at")?;

    let status = match status_str.as_str() {
        "processing" => PendingStatus::Processing,
        "confirmed" => PendingStatus::Confirmed,
        _ => PendingStatus::Pending,
    };

    let parse_ts = |s: &str| DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc));

    Ok(PendingObservation {
        id: id_str.parse().unwrap_or_default(),
        namespace,
        session_id: session_id_str.and_then(|s| s.parse().ok()),
        project_id,
        tool_name,
        tool_input: serde_json::from_str(&tool_input_str).unwrap_or_default(),
        tool_response: serde_json::from_str(&tool_response_str).unwrap_or_default(),
        content_hash,
        status,
        attempt_count,
        claimed_at: claimed_at_str.and_then(|s| parse_ts(&s).ok()),
        created_at: parse_ts(&created_at_str).unwrap_or_else(|_| Utc::now()),
        confirmed_at: confirmed_at_str.and_then(|s| parse_ts(&s).ok()),
    })
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum();
    let norm_a: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fts_search_applies_type_and_project_predicates() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        for (title, entry_type, pid) in [
            ("alpha gateway decision", EntryType::Decision, "proj_a"),
            ("alpha gateway note", EntryType::Note, "proj_a"),
            ("alpha gateway decision other", EntryType::Decision, "proj_b"),
        ] {
            adapter
                .save(
                    MemoryEntryInput {
                        title: title.into(),
                        content: "alpha gateway content".into(),
                        entry_type,
                        project_id: Some(pid.into()),
                        ..Default::default()
                    },
                    "shared-ns",
                )
                .await
                .unwrap();
        }

        // Type predicate.
        let hits = adapter
            .fts_search(SearchQuery {
                query: "alpha gateway".into(),
                namespace: Some("shared-ns".into()),
                entry_type: Some(EntryType::Decision),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "two decisions expected");
        assert!(hits.iter().all(|e| e.entry_type == EntryType::Decision));

        // Project predicate.
        let hits = adapter
            .fts_search(SearchQuery {
                query: "alpha gateway".into(),
                namespace: Some("shared-ns".into()),
                project_id: Some("proj_a".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "two proj_a rows expected");
        assert!(hits.iter().all(|e| e.project_id.as_deref() == Some("proj_a")));

        // Combined.
        let hits = adapter
            .fts_search(SearchQuery {
                query: "alpha gateway".into(),
                namespace: Some("shared-ns".into()),
                entry_type: Some(EntryType::Decision),
                project_id: Some("proj_b".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn gc_stage1_targets_only_old_unaccessed_scout_rows() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        // Fixtures: (title, source, verified, accessed, old)
        let fixtures = [
            ("old scout unaccessed", "scout", false, false, true), // ✓ only victim
            ("old human unaccessed", "human", false, false, true),
            ("verified scout", "scout", true, false, true),
            ("recent scout", "scout", false, false, false),
            ("accessed scout", "scout", false, true, true),
        ];
        let mut victim_id = None;
        for (title, source, verified, accessed, old) in fixtures {
            let src = match source {
                "scout" => MemorySource::Scout,
                _ => MemorySource::Human,
            };
            let r = adapter
                .save(
                    MemoryEntryInput {
                        title: title.into(),
                        content: format!("{title} content"),
                        entry_type: EntryType::Context,
                        source: Some(src),
                        ..Default::default()
                    },
                    "test",
                )
                .await
                .unwrap();
            let db = adapter.db.lock().unwrap();
            if old {
                db.execute(
                    "UPDATE memory_entries SET created_at = '2026-01-01T00:00:00Z' WHERE id = ?1",
                    params![r.id.to_string()],
                )
                .unwrap();
            }
            if verified {
                db.execute(
                    "UPDATE memory_entries SET verified = 1 WHERE id = ?1",
                    params![r.id.to_string()],
                )
                .unwrap();
            }
            if accessed {
                db.execute(
                    "UPDATE memory_entries SET access_count = 3, last_accessed_at = '2026-02-01T00:00:00Z' WHERE id = ?1",
                    params![r.id.to_string()],
                )
                .unwrap();
            }
            if title == "old scout unaccessed" {
                victim_id = Some(r.id);
            }
        }

        // Dry run reports without mutating.
        let dry = adapter
            .soft_delete_stale_crawl("test", 60, 1000, true)
            .await
            .unwrap();
        assert_eq!(dry, vec![victim_id.unwrap()]);
        assert!(adapter.get(victim_id.unwrap()).await.is_ok());

        // Real run soft-deletes exactly the one candidate.
        let deleted = adapter
            .soft_delete_stale_crawl("test", 60, 1000, false)
            .await
            .unwrap();
        assert_eq!(deleted, vec![victim_id.unwrap()]);
        assert!(matches!(
            adapter.get(victim_id.unwrap()).await,
            Err(StorageError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn gc_purge_hard_deletes_old_tombstones_with_cascade() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let a = adapter
            .save(
                MemoryEntryInput {
                    title: "purge me".into(),
                    content: "old tombstone".into(),
                    entry_type: EntryType::Note,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        let b = adapter
            .save(
                MemoryEntryInput {
                    title: "keep me".into(),
                    content: "live row".into(),
                    entry_type: EntryType::Note,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        adapter.save_embedding(a.id, &[0.1, 0.2]).await.unwrap();
        adapter
            .save_edge(MemoryEdgeInput {
                from_id: a.id,
                to_id: b.id,
                edge_type: EdgeType::Related,
                strength: 0.5,
            })
            .await
            .unwrap();

        // Soft-delete `a` and backdate the tombstone past the grace window.
        adapter.delete(a.id).await.unwrap();
        {
            let db = adapter.db.lock().unwrap();
            db.execute(
                "UPDATE memory_entries SET deleted_at = '2026-01-01T00:00:00Z' WHERE id = ?1",
                params![a.id.to_string()],
            )
            .unwrap();
        }

        // Dry run: candidate listed, nothing removed.
        let dry = adapter.purge_soft_deleted(None, 30, 1000, true).await.unwrap();
        assert_eq!(dry, vec![a.id]);

        let purged = adapter.purge_soft_deleted(None, 30, 1000, false).await.unwrap();
        assert_eq!(purged, vec![a.id]);

        {
            let db = adapter.db.lock().unwrap();
            let rows: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM memory_entries WHERE id = ?1",
                    params![a.id.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(rows, 0, "row gone");
            let emb: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM memory_embeddings WHERE entry_id = ?1",
                    params![a.id.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(emb, 0, "embedding cascaded");
            let edges: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM memory_edges WHERE from_id = ?1 OR to_id = ?1",
                    params![a.id.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(edges, 0, "edges cascaded");
            // FTS index consistent after the hard delete of a tombstone.
            db.execute(
                "INSERT INTO memory_fts(memory_fts, rank) VALUES('integrity-check', 0)",
                [],
            )
            .unwrap();
        }
        assert!(adapter.get(b.id).await.is_ok(), "live row untouched");
    }

    #[tokio::test]
    async fn evict_stale_reaches_null_last_accessed_rows() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let r = adapter
            .save(
                MemoryEntryInput {
                    title: "never touched archival".into(),
                    content: "old and cold".into(),
                    entry_type: EntryType::Context,
                    confidence: Some(0.5),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        {
            let db = adapter.db.lock().unwrap();
            db.execute(
                "UPDATE memory_entries SET layer = 4, created_at = '2026-01-01T00:00:00Z' WHERE id = ?1",
                params![r.id.to_string()],
            )
            .unwrap();
        }

        // Regression: the old predicate required last_accessed_at IS NOT
        // NULL, so never-accessed rows were immortal.
        let evicted = adapter.evict_stale("test", 60, 0.8, 100).await.unwrap();
        assert_eq!(evicted, vec![r.id]);
    }

    #[tokio::test]
    async fn touch_entries_atomic_increment_and_promotion() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let cold = adapter
            .save(
                MemoryEntryInput {
                    title: "cold archival entry".into(),
                    content: "rarely used".into(),
                    entry_type: EntryType::Note,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        let hot = adapter
            .save(
                MemoryEntryInput {
                    title: "hot working entry".into(),
                    content: "in active use".into(),
                    entry_type: EntryType::Note,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        {
            let db = adapter.db.lock().unwrap();
            db.execute(
                "UPDATE memory_entries SET layer = 4 WHERE id = ?1",
                params![cold.id.to_string()],
            )
            .unwrap();
            db.execute(
                "UPDATE memory_entries SET layer = 1 WHERE id = ?1",
                params![hot.id.to_string()],
            )
            .unwrap();
        }

        let touched = adapter.touch_entries(&[cold.id, hot.id]).await.unwrap();
        assert_eq!(touched, 2);
        let touched = adapter.touch_entries(&[cold.id, hot.id]).await.unwrap();
        assert_eq!(touched, 2);

        let cold_entry = adapter.get(cold.id).await.unwrap();
        assert_eq!(cold_entry.access_count, 2);
        assert!(cold_entry.last_accessed_at.is_some(), "timestamp must accompany count");
        assert_eq!(cold_entry.layer, MemoryLayer::EPISODIC, "cold layer promoted to EPISODIC");

        let hot_entry = adapter.get(hot.id).await.unwrap();
        assert_eq!(hot_entry.access_count, 2);
        assert_eq!(hot_entry.layer, MemoryLayer::WORKING, "hotter layers stay put");
    }

    #[tokio::test]
    async fn get_stats_global_aggregates_across_namespaces() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        for (ns, title) in [("proj_a", "a1"), ("proj_a", "a2"), ("proj_b", "b1")] {
            adapter
                .save(
                    MemoryEntryInput {
                        title: title.into(),
                        content: format!("{title} content"),
                        entry_type: EntryType::Note,
                        ..Default::default()
                    },
                    ns,
                )
                .await
                .unwrap();
        }
        adapter
            .create_session(
                SessionInput {
                    goal: Some("g".into()),
                    project_id: Some("proj_a".into()),
                    tool: None,
                },
                "proj_a",
            )
            .await
            .unwrap();

        let stats = adapter.get_stats_all().await.unwrap();
        assert_eq!(stats.total_entries, 3);
        assert_eq!(stats.total_sessions, 1);
        let a = stats.by_namespace.iter().find(|n| n.namespace == "proj_a").unwrap();
        assert_eq!((a.entries, a.sessions), (2, 1));
        let b = stats.by_namespace.iter().find(|n| n.namespace == "proj_b").unwrap();
        assert_eq!((b.entries, b.sessions), (1, 0));

        // Scoped stats keep their old single-namespace shape.
        let scoped = adapter.get_stats("proj_a").await.unwrap();
        assert_eq!(scoped.total_entries, 2);
        assert_eq!(scoped.total_sessions, 1);
    }

    #[tokio::test]
    async fn debug_log_round_trips_duration_and_prunes() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        adapter
            .write_debug_log(DebugLogInput {
                event: DebugEvent::Injection,
                entry_id: None,
                data: serde_json::json!({"entryCount": 5}),
                duration_ms: Some(12.5),
            })
            .await
            .unwrap();
        adapter
            .write_debug_log(DebugLogInput {
                event: DebugEvent::SearchScoring,
                entry_id: None,
                data: serde_json::json!({}),
                duration_ms: None,
            })
            .await
            .unwrap();

        let injections = adapter
            .query_debug_log(DebugLogQuery {
                event: Some(DebugEvent::Injection),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].duration_ms, Some(12.5));

        // Backdate one row 8 days; prune(7) removes exactly it.
        {
            let db = adapter.db.lock().unwrap();
            let old = (Utc::now() - chrono::Duration::days(8)).to_rfc3339();
            db.execute(
                "UPDATE debug_log SET created_at = ?1 WHERE event = 'search_scoring'",
                params![old],
            )
            .unwrap();
        }
        let pruned = adapter.prune_debug_log(7).await.unwrap();
        assert_eq!(pruned, 1);
        let remaining = adapter
            .query_debug_log(DebugLogQuery::default())
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn duplicate_content_short_circuits_save() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let input = || MemoryEntryInput {
            title: "Gateway retry policy".into(),
            content: "Retries use exponential backoff.".into(),
            entry_type: EntryType::Decision,
            ..Default::default()
        };

        let first = adapter.save(input(), "test").await.unwrap();
        assert_eq!(first.action, SaveAction::Created);

        let second = adapter.save(input(), "test").await.unwrap();
        assert_eq!(second.action, SaveAction::Duplicate);
        assert_eq!(second.id, first.id, "duplicate must return the existing id");
        assert!(second.superseded.is_none());

        // Whitespace/case variants collapse too.
        let third = adapter
            .save(
                MemoryEntryInput {
                    title: "  gateway RETRY policy ".into(),
                    content: "retries   use exponential\nbackoff.".into(),
                    entry_type: EntryType::Decision,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        assert_eq!(third.action, SaveAction::Duplicate);
        assert_eq!(third.id, first.id);

        // Same namespace still has exactly one live row.
        let rows = adapter
            .list(ListFilters {
                namespace: Some("test".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);

        // Different namespace is NOT deduped against.
        let other = adapter.save(input(), "other-ns").await.unwrap();
        assert_eq!(other.action, SaveAction::Created);
    }

    #[tokio::test]
    async fn duplicate_check_beats_topic_key_supersession() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let input = || MemoryEntryInput {
            title: "arch summary".into(),
            content: "same content".into(),
            entry_type: EntryType::Architecture,
            topic_key: Some("scout:arch:test".into()),
            ..Default::default()
        };

        let first = adapter.save(input(), "test").await.unwrap();
        assert_eq!(first.action, SaveAction::Created);

        // Unchanged recrawl: must NOT soft-delete + reinsert via topic_key.
        let second = adapter.save(input(), "test").await.unwrap();
        assert_eq!(second.action, SaveAction::Duplicate);
        assert_eq!(second.id, first.id);

        // Changed content with same topic_key: supersession as before.
        let third = adapter
            .save(
                MemoryEntryInput {
                    content: "updated content".into(),
                    ..input()
                },
                "test",
            )
            .await
            .unwrap();
        assert_eq!(third.action, SaveAction::Updated);
        assert_eq!(third.superseded.as_ref().unwrap().id, first.id);
    }

    /// Raw-FTS trigger behavior across every live/deleted transition.
    #[tokio::test]
    async fn fts_triggers_track_soft_delete_lifecycle() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let result = adapter
            .save(
                MemoryEntryInput {
                    title: "zebra unique token".into(),
                    content: "quagga content".into(),
                    entry_type: EntryType::Note,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        let id = result.id.to_string();

        let raw_fts_count = |db: &rusqlite::Connection| -> i64 {
            db.query_row(
                "SELECT COUNT(*) FROM memory_fts WHERE memory_fts MATCH 'zebra'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };

        {
            let db = adapter.db.lock().unwrap();
            assert_eq!(raw_fts_count(&db), 1, "live row indexed");

            // Soft delete → de-indexed immediately.
            db.execute(
                "UPDATE memory_entries SET deleted_at = '2026-01-01T00:00:00Z' WHERE id = ?1",
                params![id],
            )
            .unwrap();
            assert_eq!(raw_fts_count(&db), 0, "soft-deleted row must leave the index");

            // Editing a tombstone is a no-op on FTS (no corruption).
            db.execute(
                "UPDATE memory_entries SET title = 'zebra renamed' WHERE id = ?1",
                params![id],
            )
            .unwrap();
            assert_eq!(raw_fts_count(&db), 0);

            // Undelete → re-indexed with current values.
            db.execute(
                "UPDATE memory_entries SET deleted_at = NULL WHERE id = ?1",
                params![id],
            )
            .unwrap();
            assert_eq!(raw_fts_count(&db), 1, "undeleted row re-indexed");

            // Soft delete again, then hard delete: memory_ad must skip FTS
            // (row already de-indexed) and leave the index consistent.
            db.execute(
                "UPDATE memory_entries SET deleted_at = '2026-01-01T00:00:00Z' WHERE id = ?1",
                params![id],
            )
            .unwrap();
            db.execute("DELETE FROM memory_entries WHERE id = ?1", params![id])
                .unwrap();
            assert_eq!(raw_fts_count(&db), 0);

            // FTS5 self-check: errors if the external-content index diverged.
            db.execute(
                "INSERT INTO memory_fts(memory_fts, rank) VALUES('integrity-check', 0)",
                [],
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn find_duplicate_clusters_groups_by_namespace_and_hash() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        // Two identical legacy rows (bypass save's dedup via raw insert,
        // simulating the pre-013 corpus), plus one unique row.
        {
            let db = adapter.db.lock().unwrap();
            for (id, title) in [
                ("aaaaaaaa-0000-0000-0000-000000000001", "dup title"),
                ("aaaaaaaa-0000-0000-0000-000000000002", "dup title"),
                ("aaaaaaaa-0000-0000-0000-000000000003", "unique title"),
            ] {
                db.execute(
                    "INSERT INTO memory_entries (id, namespace, title, content, type, tags, created_at, updated_at)
                     VALUES (?1, 'test', ?2, 'shared body', 'note', '[]', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                    params![id, title],
                )
                .unwrap();
            }
        }

        // Backfill hashes through the maintenance API.
        loop {
            let batch = adapter.list_missing_content_hash(10).await.unwrap();
            if batch.is_empty() {
                break;
            }
            for (id, title, content) in batch {
                let h = crate::storage::content_hash(&title, &content);
                adapter.set_content_hash(id, &h).await.unwrap();
            }
        }

        let clusters = adapter.find_duplicate_clusters(Some("test")).await.unwrap();
        assert_eq!(clusters.len(), 1, "one cluster of the two identical rows");
        assert_eq!(clusters[0].entries.len(), 2);
    }

    #[tokio::test]
    async fn migration_012_backfills_namespace_from_project_id() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        // Simulate legacy rows written before the invariant existed:
        // namespace 'default' with a project_id set.
        {
            let db = adapter.db.lock().unwrap();
            db.execute(
                "INSERT INTO memory_entries (id, namespace, title, content, type, tags, project_id, created_at, updated_at)
                 VALUES ('11111111-1111-1111-1111-111111111111', 'default', 'legacy', 'legacy row', 'note', '[]', 'proj_a', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT INTO sessions (id, namespace, project_id, status, started_at, created_at, updated_at)
                 VALUES ('22222222-2222-2222-2222-222222222222', 'default', 'proj_a', 'completed', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();

            db.execute_batch(include_str!("sql/012_scope_project_namespaces.sql"))
                .unwrap();

            let ns: String = db
                .query_row(
                    "SELECT namespace FROM memory_entries WHERE id = '11111111-1111-1111-1111-111111111111'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(ns, "proj_a");

            let sns: String = db
                .query_row(
                    "SELECT namespace FROM sessions WHERE id = '22222222-2222-2222-2222-222222222222'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(sns, "proj_a");
        }
    }

    #[tokio::test]
    async fn test_save_and_get() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let result = adapter
            .save(
                MemoryEntryInput {
                    title: "Test entry".into(),
                    content: "This is a test memory entry".into(),
                    entry_type: EntryType::Note,
                    tags: vec!["test".into()],
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        assert_eq!(result.action, SaveAction::Created);

        let entry = adapter.get(result.id).await.unwrap();
        assert_eq!(entry.title, "Test entry");
        assert_eq!(entry.content, "This is a test memory entry");
        assert_eq!(entry.entry_type, EntryType::Note);
        assert_eq!(entry.tags, vec!["test"]);
        // Phase 5.7 — no author passed → column should round-trip as None.
        assert!(entry.author.is_none());
        assert!(entry.verified_by.is_none());
    }

    /// Phase 5.7 — author flows through save → get round-trip when the
    /// caller stamps it on the input. Also verifies `mark_verified`
    /// records `verified_by` independently.
    #[tokio::test]
    async fn author_and_verified_by_round_trip() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let result = adapter
            .save(
                MemoryEntryInput {
                    title: "Authored entry".into(),
                    content: "stamped at save time".into(),
                    entry_type: EntryType::Note,
                    author: Some("Alice".into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let entry = adapter.get(result.id).await.unwrap();
        assert_eq!(entry.author.as_deref(), Some("Alice"));
        assert!(entry.verified_by.is_none());

        let verified = adapter.mark_verified(result.id, Some("Bob")).await.unwrap();
        assert!(verified.verified);
        assert_eq!(verified.verified_by.as_deref(), Some("Bob"));
        // Original author must not be overwritten by verify.
        assert_eq!(verified.author.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn test_fts_search() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        adapter
            .save(
                MemoryEntryInput {
                    title: "Authentication decision".into(),
                    content: "We chose JWT with refresh tokens for the auth flow".into(),
                    entry_type: EntryType::Decision,
                    tags: vec!["auth".into()],
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let results = adapter
            .fts_search(SearchQuery {
                query: "JWT authentication".into(),
                namespace: Some("test".into()),
                limit: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Authentication decision");
    }

    #[tokio::test]
    async fn test_sessions() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let session = adapter
            .create_session(
                SessionInput {
                    goal: Some("Fix auth bug".into()),
                    project_id: None,
                    tool: Some("claude-code".into()),
                },
                "test",
            )
            .await
            .unwrap();

        assert_eq!(session.status, SessionStatus::Active);
        assert_eq!(session.goal, Some("Fix auth bug".into()));

        let updated = adapter
            .update_session(
                session.id,
                SessionUpdate {
                    status: Some(SessionStatus::Completed),
                    summary: Some("Fixed the JWT refresh bug".into()),
                    ended_at: Some(Utc::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.status, SessionStatus::Completed);
        assert_eq!(updated.summary, Some("Fixed the JWT refresh bug".into()));
    }

    #[tokio::test]
    async fn test_session_files_modified_update() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let session = adapter
            .create_session(
                SessionInput {
                    goal: Some("Track file mods".into()),
                    project_id: Some("demo".into()),
                    tool: Some("claude-code".into()),
                },
                "test",
            )
            .await
            .unwrap();

        let files = vec!["src/auth.rs".to_string(), "src/token.rs".to_string()];
        adapter
            .update_session(
                session.id,
                SessionUpdate {
                    files_modified: Some(files.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let row_json: String = {
            let db = adapter.db.lock().unwrap();
            db.query_row(
                "SELECT files_modified FROM sessions WHERE id = ?1",
                params![session.id.to_string()],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
        };
        let persisted: Vec<String> = serde_json::from_str(&row_json).unwrap();
        assert_eq!(persisted, files);
    }

    #[tokio::test]
    async fn test_edges() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let e1 = adapter
            .save(
                MemoryEntryInput {
                    title: "Entry 1".into(),
                    content: "First entry".into(),
                    entry_type: EntryType::Note,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let e2 = adapter
            .save(
                MemoryEntryInput {
                    title: "Entry 2".into(),
                    content: "Second entry".into(),
                    entry_type: EntryType::Note,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let edge = adapter
            .save_edge(MemoryEdgeInput {
                from_id: e1.id,
                to_id: e2.id,
                edge_type: EdgeType::Related,
                strength: 0.8,
            })
            .await
            .unwrap();

        assert_eq!(edge.edge_type, EdgeType::Related);

        let edges = adapter.get_edges(e1.id, None).await.unwrap();
        assert_eq!(edges.len(), 1);
    }

    #[tokio::test]
    async fn test_stats() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        adapter
            .save(
                MemoryEntryInput {
                    title: "A note".into(),
                    content: "Some content".into(),
                    entry_type: EntryType::Note,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let stats = adapter.get_stats("test").await.unwrap();
        assert_eq!(stats.total_entries, 1);
    }

    #[tokio::test]
    async fn test_confidence_default_on_save() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let result = adapter
            .save(
                MemoryEntryInput {
                    title: "No confidence".into(),
                    content: "default observed".into(),
                    entry_type: EntryType::Note,
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let entry = adapter.get(result.id).await.unwrap();
        assert!((entry.confidence - DEFAULT_CONFIDENCE).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_confidence_custom_value_is_clamped() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let result = adapter
            .save(
                MemoryEntryInput {
                    title: "Speculative".into(),
                    content: "low confidence entry".into(),
                    entry_type: EntryType::Note,
                    confidence: Some(1.9),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let entry = adapter.get(result.id).await.unwrap();
        assert!((entry.confidence - 1.0).abs() < f32::EPSILON);

        let result = adapter
            .save(
                MemoryEntryInput {
                    title: "Speculative 2".into(),
                    content: "low confidence entry".into(),
                    entry_type: EntryType::Note,
                    confidence: Some(0.4),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let entry = adapter.get(result.id).await.unwrap();
        assert!((entry.confidence - 0.4).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_topic_key_insert_returns_created_no_superseded() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let result = adapter
            .save(
                MemoryEntryInput {
                    title: "First save".into(),
                    content: "first content".into(),
                    entry_type: EntryType::Decision,
                    topic_key: Some("decision:jwt-refresh".into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        assert_eq!(result.action, SaveAction::Created);
        assert!(result.superseded.is_none());
    }

    #[tokio::test]
    async fn test_topic_key_upsert_returns_superseded() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let first = adapter
            .save(
                MemoryEntryInput {
                    title: "JWT token expiry issue".into(),
                    content: "initial analysis".into(),
                    entry_type: EntryType::Bug,
                    topic_key: Some("bug:jwt-refresh-token-expiry".into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let second = adapter
            .save(
                MemoryEntryInput {
                    title: "JWT refresh token expiry".into(),
                    content: "root-cause nailed down".into(),
                    entry_type: EntryType::Bug,
                    topic_key: Some("bug:jwt-refresh-token-expiry".into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        assert_eq!(second.action, SaveAction::Updated);
        let sup = second.superseded.expect("should report superseded entry");
        assert_eq!(sup.id, first.id);
        assert_eq!(sup.title, "JWT token expiry issue");
        assert_ne!(sup.id, second.id);

        // Old entry is soft-deleted
        let err = adapter.get(first.id).await.unwrap_err();
        matches!(err, StorageError::NotFound(_));

        // Only the new entry is live on subsequent topicKey lookup
        let third = adapter
            .save(
                MemoryEntryInput {
                    title: "JWT refresh token expiry (final)".into(),
                    content: "ship it".into(),
                    entry_type: EntryType::Bug,
                    topic_key: Some("bug:jwt-refresh-token-expiry".into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        assert_eq!(third.action, SaveAction::Updated);
        assert_eq!(
            third.superseded.as_ref().unwrap().id,
            second.id,
            "must supersede the most recent live entry, not the original"
        );
    }

    #[tokio::test]
    async fn test_topic_key_persisted_on_entry() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let result = adapter
            .save(
                MemoryEntryInput {
                    title: "Topic key round-trip".into(),
                    content: "persisted column".into(),
                    entry_type: EntryType::Note,
                    topic_key: Some("note:topic-key-round-trip".into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();

        let entry = adapter.get(result.id).await.unwrap();
        assert_eq!(
            entry.topic_key.as_deref(),
            Some("note:topic-key-round-trip")
        );
    }

    #[tokio::test]
    async fn test_merge_project_namespace_counts_and_updates() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        for i in 0..3 {
            adapter
                .save(
                    MemoryEntryInput {
                        title: format!("legacy entry {i}"),
                        content: "content".into(),
                        entry_type: EntryType::Note,
                        project_id: Some("runar_forge".into()),
                        ..Default::default()
                    },
                    "runar_forge",
                )
                .await
                .unwrap();
        }

        adapter
            .create_session(
                SessionInput {
                    goal: Some("legacy session".into()),
                    project_id: Some("runar_forge".into()),
                    tool: Some("claude-code".into()),
                },
                "runar_forge",
            )
            .await
            .unwrap();

        let preview = adapter
            .count_project_namespace("runar_forge")
            .await
            .unwrap();
        assert_eq!(preview.entries, 3);
        assert_eq!(preview.sessions, 1);

        let counts = adapter
            .merge_project_namespace("runar_forge", "runar-forge")
            .await
            .unwrap();
        assert_eq!(counts.entries, 3);
        assert_eq!(counts.sessions, 1);

        let after = adapter
            .count_project_namespace("runar_forge")
            .await
            .unwrap();
        assert_eq!(after.entries, 0);
        assert_eq!(after.sessions, 0);

        let target = adapter
            .count_project_namespace("runar-forge")
            .await
            .unwrap();
        assert_eq!(target.entries, 3);
        assert_eq!(target.sessions, 1);
    }

    #[tokio::test]
    async fn test_merge_project_namespace_same_target_noop() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        adapter
            .save(
                MemoryEntryInput {
                    title: "entry".into(),
                    content: "content".into(),
                    entry_type: EntryType::Note,
                    project_id: Some("runar-forge".into()),
                    ..Default::default()
                },
                "runar-forge",
            )
            .await
            .unwrap();

        let counts = adapter
            .merge_project_namespace("runar-forge", "runar-forge")
            .await
            .unwrap();
        assert_eq!(counts.entries, 0);
        assert_eq!(counts.sessions, 0);
    }

    #[tokio::test]
    async fn test_merge_project_namespace_missing_source_zero_counts() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let counts = adapter
            .merge_project_namespace("ghost", "runar-forge")
            .await
            .unwrap();
        assert_eq!(counts.entries, 0);
        assert_eq!(counts.sessions, 0);
    }

    #[tokio::test]
    async fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);

        let c = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &c).abs() < 1e-6);
    }

    // ── Pending-observation queue tests ────────────────────────

    fn sample_obs(tool: &str, hash: &str, project: &str) -> ObservationInput {
        ObservationInput {
            session_id: None,
            project_id: Some(project.to_string()),
            tool_name: tool.to_string(),
            tool_input: serde_json::json!({"file_path": "/tmp/x.ts"}),
            tool_response: serde_json::json!({}),
            content_hash: hash.to_string(),
        }
    }

    #[tokio::test]
    async fn test_enqueue_and_claim_round_trip() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let id1 = adapter
            .enqueue_observation(sample_obs("Edit", "h1", "proj"), "proj")
            .await
            .unwrap();
        let id2 = adapter
            .enqueue_observation(sample_obs("Write", "h2", "proj"), "proj")
            .await
            .unwrap();

        let claimed = adapter.claim_observations("proj", None, 10).await.unwrap();
        assert_eq!(claimed.len(), 2);
        assert!(claimed
            .iter()
            .all(|p| p.status == PendingStatus::Processing));
        assert!(claimed.iter().any(|p| p.id == id1));
        assert!(claimed.iter().any(|p| p.id == id2));

        // Re-claim should return empty — rows are in `processing`.
        let second = adapter.claim_observations("proj", None, 10).await.unwrap();
        assert!(second.is_empty(), "already-claimed rows must not re-claim");

        adapter.confirm_observations(&[id1, id2]).await.unwrap();
    }

    #[tokio::test]
    async fn test_check_observation_duplicate() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        assert!(
            !adapter.check_observation_duplicate("hx", 30).await.unwrap(),
            "empty queue: no duplicate"
        );

        adapter
            .enqueue_observation(sample_obs("Edit", "hx", "proj"), "proj")
            .await
            .unwrap();

        assert!(
            adapter.check_observation_duplicate("hx", 30).await.unwrap(),
            "same hash within window: duplicate"
        );
        assert!(
            !adapter.check_observation_duplicate("hy", 30).await.unwrap(),
            "different hash: not a duplicate"
        );
    }

    #[tokio::test]
    async fn test_recover_stale_observations() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        adapter
            .enqueue_observation(sample_obs("Edit", "h1", "proj"), "proj")
            .await
            .unwrap();
        let _ = adapter.claim_observations("proj", None, 10).await.unwrap();

        // Age the row's `claimed_at` to beyond the stale window.
        {
            let db = adapter.db.lock().unwrap();
            let far_past = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
            db.execute(
                "UPDATE pending_observations SET claimed_at = ?1 WHERE status = 'processing'",
                params![far_past],
            )
            .unwrap();
        }

        let recovered = adapter.recover_stale_observations(60).await.unwrap();
        assert_eq!(recovered, 1);

        let reclaimed = adapter.claim_observations("proj", None, 10).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(
            reclaimed[0].attempt_count, 2,
            "attempt_count increments on reclaim"
        );
    }

    // ── Phase 5.6.1 — sync trait tests ─────────────────────────

    fn dummy_outbox_input(idx: u8) -> crate::types::OutboxInput {
        crate::types::OutboxInput {
            entry_id: Uuid::new_v4(),
            op_kind: crate::types::OutboxOp::Insert,
            row_payload: serde_json::json!({"idx": idx}),
        }
    }

    #[tokio::test]
    async fn outbox_fifo_claim_and_confirm() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        for i in 0..5 {
            adapter.enqueue_outbox(dummy_outbox_input(i)).await.unwrap();
        }
        assert_eq!(adapter.outbox_depth().await.unwrap(), 5);

        let claimed = adapter.claim_outbox(3).await.unwrap();
        assert_eq!(claimed.len(), 3, "claim batch respected");
        // Subsequent claim only sees unclaimed rows.
        let claimed_again = adapter.claim_outbox(10).await.unwrap();
        assert_eq!(claimed_again.len(), 2);

        let ids: Vec<Uuid> = claimed.iter().map(|r| r.id).collect();
        adapter.confirm_outbox(&ids).await.unwrap();
        assert_eq!(adapter.outbox_depth().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn outbox_fail_increments_attempts_and_releases_claim() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();
        adapter.enqueue_outbox(dummy_outbox_input(0)).await.unwrap();

        let claimed = adapter.claim_outbox(1).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(claimed[0].claimed_at.is_some());

        adapter.fail_outbox(claimed[0].id, "boom").await.unwrap();

        let reclaim = adapter.claim_outbox(1).await.unwrap();
        assert_eq!(reclaim.len(), 1, "row re-claimable after fail");
        assert_eq!(reclaim[0].attempts, 1);
        assert_eq!(reclaim[0].last_error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn outbox_gc_skips_pending() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        // 1 confirmed (aged), 1 pending
        let confirmed_id = adapter.enqueue_outbox(dummy_outbox_input(0)).await.unwrap();
        adapter.enqueue_outbox(dummy_outbox_input(1)).await.unwrap();
        adapter.confirm_outbox(&[confirmed_id]).await.unwrap();

        // older_than_secs = 0 means "anything past now()" — both eligible
        // but only confirmed should be deleted.
        let deleted = adapter.gc_outbox(0).await.unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(
            adapter.outbox_depth().await.unwrap(),
            1,
            "pending preserved"
        );
    }

    #[tokio::test]
    async fn sync_state_round_trip() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        // Empty by default.
        let blank = adapter.read_sync_state().await.unwrap();
        assert!(blank.initialized_at.is_none());
        assert!(blank.local_dim.is_none());

        let now = Utc::now();
        let state = crate::types::SyncState {
            initialized_at: Some(now),
            local_dim: Some(384),
            remote_dim: Some(384),
            local_schema_version: Some("9-009_add_verified".into()),
            remote_schema_version: Some("9-009_add_verified".into()),
            ..Default::default()
        };
        adapter.write_sync_state(&state).await.unwrap();

        let read = adapter.read_sync_state().await.unwrap();
        assert!(read.initialized_at.is_some());
        assert_eq!(read.local_dim, Some(384));
        assert_eq!(
            read.local_schema_version.as_deref(),
            Some("9-009_add_verified")
        );

        // Upsert overwrites.
        let updated = crate::types::SyncState {
            initialized_at: Some(now),
            local_dim: Some(1536),
            ..Default::default()
        };
        adapter.write_sync_state(&updated).await.unwrap();
        assert_eq!(
            adapter.read_sync_state().await.unwrap().local_dim,
            Some(1536)
        );
    }

    #[tokio::test]
    async fn record_and_list_conflicts() {
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let conflict = crate::types::SyncConflict {
            id: Uuid::new_v4(),
            entry_id: Uuid::new_v4(),
            direction: crate::types::ConflictDirection::Push,
            policy: crate::types::ConflictPolicy::VerifiedWins,
            winner_side: crate::types::ConflictWinner::Remote,
            local_updated_at: Some(Utc::now()),
            remote_updated_at: Some(Utc::now()),
            local_payload: Some(serde_json::json!({"verified": false})),
            remote_payload: Some(serde_json::json!({"verified": true})),
            created_at: Utc::now(),
        };
        adapter.record_conflict(&conflict).await.unwrap();

        let listed = adapter.list_conflicts(10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, conflict.id);
        assert_eq!(listed[0].policy, crate::types::ConflictPolicy::VerifiedWins);
        assert_eq!(listed[0].direction, crate::types::ConflictDirection::Push);
    }

    #[tokio::test]
    async fn list_changed_since_filters_by_cursor() {
        use crate::types::{EntryType, MemoryLayer, MemorySource};
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let now = Utc::now();
        let mut entries = Vec::new();
        // Three rows aged 60s, 30s, 10s — all old enough that
        // clock_skew_secs=2 includes them.
        for offset in [60, 30, 10] {
            let mut e = MemoryEntry {
                id: Uuid::new_v4(),
                namespace: "test".into(),
                title: format!("e-{offset}"),
                content: "x".into(),
                entry_type: EntryType::Note,
                tags: vec![],
                project_id: None,
                embedding: None,
                source: MemorySource::Human,
                layer: MemoryLayer::WORKING,
                importance: 0.5,
                decay_score: 0.5,
                access_count: 0,
                last_accessed_at: None,
                confidence: 0.9,
                topic_key: None,
                verified: false,
                verified_at: None,
                author: None,
                verified_by: None,
                created_at: now - chrono::Duration::seconds(offset),
                updated_at: now - chrono::Duration::seconds(offset),
                deleted_at: None,
            };
            // Insert via direct path (we need exact updated_at).
            adapter.import_entry(e.clone()).await.unwrap();
            // import_entry sets created_at/updated_at from the input, so OK.
            e.confidence = 0.9; // touchup just to silence "unused"
            entries.push(e);
        }

        // Cursor = 35s ago should return only the 30s + 10s rows.
        let cursor = now - chrono::Duration::seconds(35);
        let listed = adapter
            .list_changed_since(Some(cursor), 2, 100, None)
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
        // Ordered ASC by updated_at — older first.
        assert!(listed[0].updated_at < listed[1].updated_at);

        // Cursor = None should return all 3.
        let all = adapter
            .list_changed_since(None, 2, 100, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn apply_remote_entry_inserts_or_skips() {
        use crate::types::{EntryType, MemoryLayer, MemorySource};
        let adapter = SqliteAdapter::in_memory("test").unwrap();
        adapter.initialize().await.unwrap();

        let entry = MemoryEntry {
            id: Uuid::new_v4(),
            namespace: "test".into(),
            title: "Remote-origin row".into(),
            content: "from upstream".into(),
            entry_type: EntryType::Note,
            tags: vec![],
            project_id: None,
            embedding: None,
            source: MemorySource::Human,
            layer: MemoryLayer::WORKING,
            importance: 0.5,
            decay_score: 0.5,
            access_count: 0,
            last_accessed_at: None,
            confidence: 0.9,
            topic_key: None,
            verified: false,
            verified_at: None,
            author: None,
            verified_by: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
        };
        let outcome = adapter.apply_remote_entry(entry.clone()).await.unwrap();
        assert_eq!(outcome, crate::types::ApplyOutcome::Inserted);

        let outcome2 = adapter.apply_remote_entry(entry).await.unwrap();
        assert_eq!(outcome2, crate::types::ApplyOutcome::SkippedNewerLocal);
    }
}
