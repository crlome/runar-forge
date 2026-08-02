use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::{Config, Pool, Runtime};
use pgvector::Vector;
use tokio_postgres::NoTls;
use uuid::Uuid;

use super::{MemoryStorage, StorageError, StorageResult};
use crate::types::*;

/// Per-pool-acquire wait. Prevents a dead/unreachable PG from stalling a
/// request indefinitely (see Phase 4.8 item 4.8.17).
fn pool_wait_timeout() -> Duration {
    let ms: u64 = std::env::var("RUNAR_DB_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8_000);
    Duration::from_millis(ms)
}

pub const PG_MIGRATIONS: &[(&str, &str)] = &[
    (
        "000_migration_table",
        include_str!("pg_sql/005_add_migration_table.sql"),
    ),
    (
        "001_initial_schema",
        include_str!("pg_sql/001_initial_schema.sql"),
    ),
    (
        "002_add_sessions",
        include_str!("pg_sql/002_add_sessions.sql"),
    ),
    ("003_add_edges", include_str!("pg_sql/003_add_edges.sql")),
    (
        "004_add_debug_log",
        include_str!("pg_sql/004_add_debug_log.sql"),
    ),
    (
        "006_add_confidence",
        include_str!("pg_sql/006_add_confidence.sql"),
    ),
    (
        "007_add_topic_key",
        include_str!("pg_sql/007_add_topic_key.sql"),
    ),
    (
        "008_add_pending_observations",
        include_str!("pg_sql/008_add_pending_observations.sql"),
    ),
    (
        "009_add_verified",
        include_str!("pg_sql/009_add_verified.sql"),
    ),
    (
        "010_add_sync_outbox",
        include_str!("pg_sql/010_add_sync_outbox.sql"),
    ),
    ("011_add_author", include_str!("pg_sql/011_add_author.sql")),
    (
        "012_scope_project_namespaces",
        include_str!("pg_sql/012_scope_project_namespaces.sql"),
    ),
    (
        "013_content_hash",
        include_str!("pg_sql/013_content_hash.sql"),
    ),
    (
        "014_injection_counters",
        include_str!("pg_sql/014_injection_counters.sql"),
    ),
];

pub struct PostgresAdapter {
    pool: Pool,
    default_namespace: String,
}

impl PostgresAdapter {
    pub fn new(database_url: &str, namespace: &str) -> StorageResult<Self> {
        let mut cfg = Config::new();
        cfg.url = Some(database_url.to_string());

        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(|e| StorageError::Init(e.to_string()))?;

        Ok(Self {
            pool,
            default_namespace: namespace.to_string(),
        })
    }

    async fn get_client(&self) -> StorageResult<deadpool_postgres::Client> {
        match tokio::time::timeout(pool_wait_timeout(), self.pool.get()).await {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(e)) => Err(StorageError::Database(format!("pool error: {e}"))),
            Err(_) => Err(StorageError::Database(format!(
                "pool acquire timed out after {}ms (RUNAR_DB_CONNECT_TIMEOUT_MS)",
                pool_wait_timeout().as_millis()
            ))),
        }
    }

    /// Phase 5.6.2 — full-row replacement used by `apply_remote_entry`
    /// when the resolver picks Update. Distinct from `update()`
    /// (partial JSON patch) and `import_entry()` (insert-or-skip).
    async fn replace_remote_entry(&self, entry: &MemoryEntry) -> StorageResult<()> {
        let client = self.get_client().await?;
        let source_str = serde_json::to_value(entry.source)
            .unwrap()
            .as_str()
            .unwrap_or("human")
            .to_string();
        let type_str = entry.entry_type.as_str();
        let layer_val = entry.layer.value() as i32;
        let confidence = entry.confidence.clamp(0.0, 1.0);

        client
            .execute(
                "INSERT INTO muninn.memory_entries
                    (id, namespace, title, content, type, tags, project_id,
                     source, layer, confidence, topic_key, access_count,
                     verified, verified_at, author, verified_by,
                     created_at, updated_at, last_accessed_at, deleted_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                         $13, $14, $15, $16, $17, $18, $19, $20)
                 ON CONFLICT (id) DO UPDATE SET
                    namespace = EXCLUDED.namespace,
                    title = EXCLUDED.title,
                    content = EXCLUDED.content,
                    type = EXCLUDED.type,
                    tags = EXCLUDED.tags,
                    project_id = EXCLUDED.project_id,
                    source = EXCLUDED.source,
                    layer = EXCLUDED.layer,
                    confidence = EXCLUDED.confidence,
                    topic_key = EXCLUDED.topic_key,
                    access_count = EXCLUDED.access_count,
                    verified = EXCLUDED.verified,
                    verified_at = EXCLUDED.verified_at,
                    author = EXCLUDED.author,
                    verified_by = EXCLUDED.verified_by,
                    updated_at = EXCLUDED.updated_at,
                    last_accessed_at = EXCLUDED.last_accessed_at,
                    deleted_at = EXCLUDED.deleted_at",
                &[
                    &entry.id.to_string(),
                    &entry.namespace,
                    &entry.title,
                    &entry.content,
                    &type_str,
                    &entry.tags,
                    &entry.project_id,
                    &source_str,
                    &layer_val,
                    &confidence,
                    &entry.topic_key,
                    &entry.access_count,
                    &entry.verified,
                    &entry.verified_at,
                    &entry.author,
                    &entry.verified_by,
                    &entry.created_at,
                    &entry.updated_at,
                    &entry.last_accessed_at,
                    &entry.deleted_at,
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

/// Turn a driver error into something a human can act on.
///
/// `tokio_postgres::Error::to_string()` renders the literal string
/// `"db error"` for anything the server rejected — every detail the
/// server sent lives in the attached `DbError`. Reporting only the
/// outer string is why one sync_outbox row reached **64,634 attempts**
/// without anyone being able to say what was wrong with it.
///
/// Format: `SQLSTATE message (detail; constraint on table.column)`,
/// omitting parts the server did not send.
fn db_err(e: tokio_postgres::Error) -> StorageError {
    let Some(db) = e.as_db_error() else {
        // Connection/protocol/TLS failures have no DbError; the outer
        // message is genuinely the whole story there.
        return StorageError::Database(e.to_string());
    };

    StorageError::Database(format_db_error(
        db.code().code(),
        db.message(),
        db.detail(),
        db.hint(),
        db.table(),
        db.column(),
        db.constraint(),
    ))
}

/// Cap on the server-supplied `detail` string. See `format_db_error`.
const MAX_DETAIL_CHARS: usize = 300;

/// Truncate on a char boundary — `detail` carries row content, which is
/// arbitrary UTF-8, and slicing it by byte index panics mid-codepoint.
fn truncate_detail(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}… [truncated]")
}

/// Split out from `db_err` because `tokio_postgres::error::DbError` has
/// no public constructor, so the formatting is otherwise untestable.
#[allow(clippy::too_many_arguments)]
fn format_db_error(
    code: &str,
    message: &str,
    detail: Option<&str>,
    hint: Option<&str>,
    table: Option<&str>,
    column: Option<&str>,
    constraint: Option<&str>,
) -> String {
    let mut msg = format!("[{code}] {message}");
    let mut context: Vec<String> = Vec::new();
    if let Some(detail) = detail {
        // Postgres puts the ENTIRE failing row in `detail` for a
        // constraint violation. Entries here run to hundreds of KB, and
        // this string is persisted to `sync_outbox.last_error` — which
        // never passes through `redact::scrub`. Keep enough to identify
        // the row, not enough to duplicate its body.
        context.push(truncate_detail(detail, MAX_DETAIL_CHARS));
    }
    if let Some(hint) = hint {
        context.push(format!("hint: {hint}"));
    }
    match (table, column) {
        (Some(t), Some(c)) => context.push(format!("at {t}.{c}")),
        (Some(t), None) => context.push(format!("at {t}")),
        _ => {}
    }
    if let Some(constraint) = constraint {
        context.push(format!("constraint {constraint}"));
    }
    if !context.is_empty() {
        msg.push_str(&format!(" ({})", context.join("; ")));
    }
    msg
}

fn row_to_entry(row: &tokio_postgres::Row) -> MemoryEntry {
    let type_str: &str = row.get("type");
    let entry_type: EntryType =
        serde_json::from_value(serde_json::Value::String(type_str.to_string()))
            .unwrap_or(EntryType::Note);

    let source_str: &str = row.get("source");
    let source: MemorySource =
        serde_json::from_value(serde_json::Value::String(source_str.to_string()))
            .unwrap_or(MemorySource::Human);

    let tags: Vec<String> = row.get("tags");
    let layer_val: i32 = row.get("layer");
    let id_str: &str = row.get("id");
    let confidence: f32 = row
        .try_get::<_, f32>("confidence")
        .unwrap_or(DEFAULT_CONFIDENCE);
    let topic_key: Option<String> = row.try_get::<_, Option<String>>("topic_key").ok().flatten();
    let verified: bool = row.try_get::<_, bool>("verified").unwrap_or(false);
    let verified_at: Option<DateTime<Utc>> = row
        .try_get::<_, Option<DateTime<Utc>>>("verified_at")
        .ok()
        .flatten();
    let author: Option<String> = row.try_get::<_, Option<String>>("author").ok().flatten();
    let verified_by: Option<String> = row
        .try_get::<_, Option<String>>("verified_by")
        .ok()
        .flatten();

    MemoryEntry {
        id: id_str.parse().unwrap_or_default(),
        title: row.get("title"),
        content: row.get("content"),
        entry_type,
        source,
        tags,
        namespace: row.get("namespace"),
        project_id: row.get("project_id"),
        topic_key,
        layer: MemoryLayer::from(layer_val as u8),
        importance: 0.5,
        decay_score: 1.0,
        access_count: row.get("access_count"),
        injected_count: row.try_get("injected_count").unwrap_or(0),
        last_injected_at: row.try_get("last_injected_at").ok().flatten(),
        confidence,
        embedding: None,
        verified,
        verified_at,
        author,
        verified_by,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        last_accessed_at: row.get("last_accessed_at"),
        deleted_at: row.get("deleted_at"),
    }
}

fn row_to_session(row: &tokio_postgres::Row) -> Session {
    let status_str: &str = row.get("status");
    let status: SessionStatus =
        serde_json::from_value(serde_json::Value::String(status_str.to_string()))
            .unwrap_or(SessionStatus::Active);

    let id_str: &str = row.get("id");

    Session {
        id: id_str.parse().unwrap_or_default(),
        namespace: row.get("namespace"),
        project_id: row.get("project_id"),
        tool: row.get("tool"),
        goal: row.get("goal"),
        summary: row.get("summary"),
        discoveries: row.get("discoveries"),
        files_modified: row.get("files_modified"),
        status,
        started_at: row.get("started_at"),
        ended_at: row.get("ended_at"),
    }
}

#[async_trait]
impl MemoryStorage for PostgresAdapter {
    async fn initialize(&self) -> StorageResult<()> {
        let client = self.get_client().await?;

        // Ensure migration table exists first
        client
            .batch_execute(PG_MIGRATIONS[0].1)
            .await
            .map_err(db_err)?;

        let rows = client
            .query(
                "SELECT version FROM muninn.schema_migrations ORDER BY version",
                &[],
            )
            .await
            .unwrap_or_default();

        let applied: Vec<String> = rows.iter().map(|r| r.get(0)).collect();

        for (version, sql) in PG_MIGRATIONS.iter().skip(1) {
            if applied.contains(&version.to_string()) {
                continue;
            }
            tracing::info!(version, "applying PostgreSQL migration");
            client.batch_execute(sql).await.map_err(db_err)?;
            client
                .execute(
                    "INSERT INTO muninn.schema_migrations (version) VALUES ($1) ON CONFLICT DO NOTHING",
                    &[&version.to_string()],
                )
                .await
                .map_err(db_err)?;
        }

        Ok(())
    }

    async fn close(&self) -> StorageResult<()> {
        Ok(())
    }

    async fn save(&self, input: MemoryEntryInput, namespace: &str) -> StorageResult<SaveResult> {
        let client = self.get_client().await?;
        let id = Uuid::new_v4();
        let ns = if namespace.is_empty() {
            &self.default_namespace
        } else {
            namespace
        };
        let source_str = serde_json::to_value(input.source.unwrap_or(MemorySource::Human))
            .unwrap()
            .as_str()
            .unwrap_or("human")
            .to_string();

        let confidence = input
            .confidence
            .unwrap_or(DEFAULT_CONFIDENCE)
            .clamp(0.0, 1.0);

        // Exact-duplicate guard: identical content in the same namespace
        // short-circuits before the topic_key branch, so an unchanged
        // recrawl never soft-deletes-and-reinserts its own predecessor.
        let hash = crate::storage::content_hash(&input.title, &input.content);
        let dup_row = client
            .query_opt(
                "SELECT id FROM muninn.memory_entries
                 WHERE namespace = $1 AND content_hash = $2 AND deleted_at IS NULL
                 LIMIT 1",
                &[&ns, &hash],
            )
            .await
            .map_err(db_err)?;
        if let Some(row) = dup_row {
            let dup_id: &str = row.get("id");
            client
                .execute(
                    "UPDATE muninn.memory_entries SET updated_at = NOW() WHERE id = $1",
                    &[&dup_id.to_string()],
                )
                .await
                .map_err(db_err)?;
            return Ok(SaveResult {
                id: dup_id.parse().unwrap_or_default(),
                action: SaveAction::Duplicate,
                superseded: None,
            });
        }

        // Phase 5.1.2 — topicKey upsert: soft-delete prior live entry with
        // the same (namespace, topic_key) and capture its metadata for the
        // supersession response.
        let superseded = match input.topic_key.as_deref() {
            Some(tk) if !tk.is_empty() => {
                let row_opt = client
                    .query_opt(
                        "SELECT id, title, created_at FROM muninn.memory_entries
                         WHERE namespace = $1 AND topic_key = $2 AND deleted_at IS NULL
                         ORDER BY created_at DESC
                         LIMIT 1",
                        &[&ns, &tk],
                    )
                    .await
                    .map_err(db_err)?;

                if let Some(row) = row_opt {
                    let old_id_str: &str = row.get("id");
                    let old_title: String = row.get("title");
                    let old_created: chrono::DateTime<chrono::Utc> = row.get("created_at");

                    client
                        .execute(
                            "UPDATE muninn.memory_entries SET deleted_at = NOW() WHERE id = $1",
                            &[&old_id_str.to_string()],
                        )
                        .await
                        .map_err(db_err)?;

                    Some(SupersededEntry {
                        id: old_id_str.parse().unwrap_or_default(),
                        title: old_title,
                        created_at: old_created,
                    })
                } else {
                    None
                }
            }
            _ => None,
        };

        client
            .execute(
                "INSERT INTO muninn.memory_entries
                    (id, namespace, title, content, type, tags, project_id, source, layer, confidence, topic_key, author, content_hash)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                &[
                    &id.to_string(),
                    &ns,
                    &input.title,
                    &input.content,
                    &input.entry_type.as_str(),
                    &input.tags,
                    &input.project_id,
                    &source_str,
                    &(MemoryLayer::WORKING.value() as i32),
                    &confidence,
                    &input.topic_key,
                    &input.author,
                    &hash,
                ],
            )
            .await
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
        let client = self.get_client().await?;
        let row = client
            .query_opt(
                "SELECT * FROM muninn.memory_entries WHERE id = $1 AND deleted_at IS NULL",
                &[&id.to_string()],
            )
            .await
            .map_err(db_err)?
            .ok_or(StorageError::NotFound(id))?;

        Ok(row_to_entry(&row))
    }

    async fn get_including_deleted(&self, id: Uuid) -> StorageResult<MemoryEntry> {
        let client = self.get_client().await?;
        let row = client
            .query_opt(
                "SELECT * FROM muninn.memory_entries WHERE id = $1",
                &[&id.to_string()],
            )
            .await
            .map_err(db_err)?
            .ok_or(StorageError::NotFound(id))?;

        Ok(row_to_entry(&row))
    }

    async fn get_by_topic_key(
        &self,
        namespace: &str,
        topic_key: &str,
    ) -> StorageResult<Option<MemoryEntry>> {
        let client = self.get_client().await?;
        let row = client
            .query_opt(
                "SELECT * FROM muninn.memory_entries
                 WHERE namespace = $1 AND topic_key = $2 AND deleted_at IS NULL
                 ORDER BY created_at DESC
                 LIMIT 1",
                &[&namespace, &topic_key],
            )
            .await
            .map_err(db_err)?;

        Ok(row.as_ref().map(row_to_entry))
    }

    async fn update(&self, id: Uuid, updates: serde_json::Value) -> StorageResult<MemoryEntry> {
        let client = self.get_client().await?;

        if let Some(obj) = updates.as_object() {
            // One dynamic UPDATE for all recognized fields. The old
            // per-field autocommit statements could be interrupted between
            // statements, leaving partial writes (access_count bumped,
            // last_accessed_at still NULL).
            let mut sets: Vec<String> = vec!["updated_at = NOW()".to_string()];
            let mut str_vals: Vec<(String, String)> = Vec::new();
            let mut int_vals: Vec<(String, i32)> = Vec::new();
            // tags is TEXT[] in postgres — accept either a JSON-encoded
            // array string or a JSON array value, and bind a real array.
            let mut tags_val: Option<Vec<String>> = None;

            for (key, value) in obj {
                match key.as_str() {
                    "title" | "content" => {
                        str_vals.push((key.clone(), value.as_str().unwrap_or("").to_string()));
                    }
                    "tags" => {
                        tags_val = match value {
                            serde_json::Value::String(s) => {
                                serde_json::from_str::<Vec<String>>(s).ok()
                            }
                            other => serde_json::from_value(other.clone()).ok(),
                        };
                    }
                    "last_accessed_at" => {
                        str_vals.push((key.clone(), value.as_str().unwrap_or("").to_string()));
                    }
                    "layer" => int_vals.push((key.clone(), value.as_i64().unwrap_or(3) as i32)),
                    "access_count" => {
                        int_vals.push((key.clone(), value.as_i64().unwrap_or(0) as i32))
                    }
                    _ => {}
                }
            }

            let mut args: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
            for (key, val) in &str_vals {
                args.push(val);
                if key == "last_accessed_at" {
                    sets.push(format!("{key} = ${}::timestamptz", args.len()));
                } else {
                    sets.push(format!("{key} = ${}", args.len()));
                }
            }
            for (key, val) in &int_vals {
                args.push(val);
                sets.push(format!("{key} = ${}", args.len()));
            }
            if let Some(ref tags) = tags_val {
                args.push(tags);
                sets.push(format!("tags = ${}", args.len()));
            }

            let id_str = id.to_string();
            args.push(&id_str);
            let sql = format!(
                "UPDATE muninn.memory_entries SET {} WHERE id = ${} AND deleted_at IS NULL",
                sets.join(", "),
                args.len()
            );
            client.execute(&sql, &args).await.map_err(db_err)?;

            // Content changed → keep the dedup hash in sync.
            if obj.contains_key("title") || obj.contains_key("content") {
                if let Some(row) = client
                    .query_opt(
                        "SELECT title, content FROM muninn.memory_entries WHERE id = $1",
                        &[&id_str],
                    )
                    .await
                    .map_err(db_err)?
                {
                    let t: String = row.get("title");
                    let c: String = row.get("content");
                    let hash = crate::storage::content_hash(&t, &c);
                    client
                        .execute(
                            "UPDATE muninn.memory_entries SET content_hash = $1 WHERE id = $2",
                            &[&hash, &id_str],
                        )
                        .await
                        .map_err(db_err)?;
                }
            }
        }

        self.get(id).await
    }

    async fn delete(&self, id: Uuid) -> StorageResult<()> {
        let client = self.get_client().await?;
        client
            .execute(
                "UPDATE muninn.memory_entries SET deleted_at = NOW() WHERE id = $1",
                &[&id.to_string()],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn list(&self, filters: ListFilters) -> StorageResult<Vec<MemoryEntry>> {
        let client = self.get_client().await?;
        let ns = filters
            .namespace
            .as_deref()
            .unwrap_or(&self.default_namespace)
            .to_string();
        let limit = filters.limit.unwrap_or(50) as i64;
        let offset = filters.offset.unwrap_or(0) as i64;
        let type_filter = filters.entry_type.map(|t| t.as_str().to_string());
        let project_filter = filters.project_id.clone();

        let mut sql = String::from("SELECT * FROM muninn.memory_entries WHERE namespace = $1");
        if !filters.include_deleted {
            sql.push_str(" AND deleted_at IS NULL");
        }
        let mut args: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&ns];
        if let Some(ref t) = type_filter {
            args.push(t);
            sql.push_str(&format!(" AND type = ${}", args.len()));
        }
        if let Some(ref p) = project_filter {
            args.push(p);
            sql.push_str(&format!(" AND project_id = ${}", args.len()));
        }
        args.push(&limit);
        sql.push_str(&format!(" ORDER BY created_at DESC LIMIT ${}", args.len()));
        args.push(&offset);
        sql.push_str(&format!(" OFFSET ${}", args.len()));

        let rows = client.query(&sql, &args).await.map_err(db_err)?;

        Ok(rows.iter().map(row_to_entry).collect())
    }

    async fn save_embedding(&self, entry_id: Uuid, embedding: &[f32]) -> StorageResult<()> {
        let client = self.get_client().await?;
        let vec = Vector::from(embedding.to_vec());

        client
            .execute(
                "UPDATE muninn.memory_entries SET embedding = $1 WHERE id = $2",
                &[&vec, &entry_id.to_string()],
            )
            .await
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
        let client = self.get_client().await?;
        let ns = match filters.namespace.as_deref() {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => self.default_namespace.clone(),
        };
        let limit = filters.limit.unwrap_or(10) as i64;
        let vec = Vector::from(query_embedding.to_vec());
        let type_str = filters.entry_type.as_ref().map(|t| t.as_str().to_string());

        // Same predicates as fts_search so both fused-search arms see an
        // identically scoped corpus.
        let mut sql = String::from(
            "SELECT *, 1 - (embedding <=> $1) AS similarity
             FROM muninn.memory_entries
             WHERE namespace = $2
               AND deleted_at IS NULL
               AND embedding IS NOT NULL
               AND 1 - (embedding <=> $1) >= 0.65",
        );
        let mut args: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&vec, &ns];
        if let Some(ref t) = type_str {
            args.push(t);
            sql.push_str(&format!(" AND type = ${}", args.len()));
        }
        if let Some(ref pid) = filters.project_id {
            args.push(pid);
            sql.push_str(&format!(" AND project_id = ${}", args.len()));
        }
        args.push(&limit);
        sql.push_str(&format!(" ORDER BY embedding <=> $1 LIMIT ${}", args.len()));

        let rows = client.query(&sql, &args).await.map_err(db_err)?;

        Ok(rows.iter().map(row_to_entry).collect())
    }

    async fn fts_search(&self, query: SearchQuery) -> StorageResult<Vec<MemoryEntry>> {
        let client = self.get_client().await?;
        let ns = query
            .namespace
            .as_deref()
            .unwrap_or(&self.default_namespace)
            .to_string();
        let limit = query.limit.unwrap_or(10) as i64;
        let type_str = query.entry_type.as_ref().map(|t| t.as_str().to_string());

        let mut sql = String::from(
            "SELECT *, ts_rank(fts_vector, plainto_tsquery('english', $1)) AS rank
             FROM muninn.memory_entries
             WHERE namespace = $2
               AND deleted_at IS NULL
               AND fts_vector @@ plainto_tsquery('english', $1)",
        );
        let mut args: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&query.query, &ns];
        if let Some(ref t) = type_str {
            args.push(t);
            sql.push_str(&format!(" AND type = ${}", args.len()));
        }
        if let Some(ref pid) = query.project_id {
            args.push(pid);
            sql.push_str(&format!(" AND project_id = ${}", args.len()));
        }
        args.push(&limit);
        sql.push_str(&format!(" ORDER BY rank DESC LIMIT ${}", args.len()));

        let rows = client.query(&sql, &args).await.map_err(db_err)?;

        Ok(rows.iter().map(row_to_entry).collect())
    }

    // ── Sessions ───────────────────────────────────────────────

    async fn create_session(&self, input: SessionInput, namespace: &str) -> StorageResult<Session> {
        let client = self.get_client().await?;
        let id = Uuid::new_v4();
        let ns = if namespace.is_empty() {
            &self.default_namespace
        } else {
            namespace
        };

        client
            .execute(
                "INSERT INTO muninn.sessions (id, namespace, project_id, tool, goal, status)
                 VALUES ($1, $2, $3, $4, $5, 'active')",
                &[
                    &id.to_string(),
                    &ns,
                    &input.project_id,
                    &input.tool,
                    &input.goal,
                ],
            )
            .await
            .map_err(db_err)?;

        self.get_session(id).await
    }

    async fn get_session(&self, id: Uuid) -> StorageResult<Session> {
        let client = self.get_client().await?;
        let row = client
            .query_opt(
                "SELECT * FROM muninn.sessions WHERE id = $1",
                &[&id.to_string()],
            )
            .await
            .map_err(db_err)?
            .ok_or(StorageError::NotFound(id))?;

        Ok(row_to_session(&row))
    }

    async fn update_session(&self, id: Uuid, update: SessionUpdate) -> StorageResult<Session> {
        let client = self.get_client().await?;

        if let Some(status) = update.status {
            let status_str = serde_json::to_value(status)
                .unwrap()
                .as_str()
                .unwrap_or("active")
                .to_string();
            client
                .execute(
                    "UPDATE muninn.sessions SET status = $1 WHERE id = $2",
                    &[&status_str, &id.to_string()],
                )
                .await
                .map_err(db_err)?;
        }
        if let Some(ref summary) = update.summary {
            client
                .execute(
                    "UPDATE muninn.sessions SET summary = $1 WHERE id = $2",
                    &[summary, &id.to_string()],
                )
                .await
                .map_err(db_err)?;
        }
        if let Some(ref ended_at) = update.ended_at {
            client
                .execute(
                    "UPDATE muninn.sessions SET ended_at = $1 WHERE id = $2",
                    &[ended_at, &id.to_string()],
                )
                .await
                .map_err(db_err)?;
        }
        if let Some(ref files) = update.files_modified {
            client
                .execute(
                    "UPDATE muninn.sessions SET files_modified = $1 WHERE id = $2",
                    &[files, &id.to_string()],
                )
                .await
                .map_err(db_err)?;
        }
        if let Some(ref goal) = update.goal {
            client
                .execute(
                    "UPDATE muninn.sessions SET goal = $1 WHERE id = $2",
                    &[goal, &id.to_string()],
                )
                .await
                .map_err(db_err)?;
        }
        if let Some(ref discoveries) = update.discoveries {
            client
                .execute(
                    "UPDATE muninn.sessions SET discoveries = $1 WHERE id = $2",
                    &[discoveries, &id.to_string()],
                )
                .await
                .map_err(db_err)?;
        }

        self.get_session(id).await
    }

    async fn list_sessions(&self, namespace: &str, limit: usize) -> StorageResult<Vec<Session>> {
        let client = self.get_client().await?;
        let ns = if namespace.is_empty() {
            &self.default_namespace
        } else {
            namespace
        };

        let rows = client
            .query(
                "SELECT * FROM muninn.sessions WHERE namespace = $1 ORDER BY started_at DESC LIMIT $2",
                &[&ns, &(limit as i64)],
            )
            .await
            .map_err(db_err)?;

        Ok(rows.iter().map(row_to_session).collect())
    }

    // ── Edges ──────────────────────────────────────────────────

    async fn save_edge(&self, input: MemoryEdgeInput) -> StorageResult<MemoryEdge> {
        let client = self.get_client().await?;
        let id = Uuid::new_v4();
        let edge_type_str = serde_json::to_value(input.edge_type)
            .unwrap()
            .as_str()
            .unwrap_or("related")
            .to_string();

        client
            .execute(
                "INSERT INTO muninn.memory_edges (id, from_id, to_id, type, strength)
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &id.to_string(),
                    &input.from_id.to_string(),
                    &input.to_id.to_string(),
                    &edge_type_str,
                    &(input.strength as f32),
                ],
            )
            .await
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
        let client = self.get_client().await?;
        let id_str = entry_id.to_string();

        let sql = match direction.unwrap_or("both") {
            "from" => "SELECT * FROM muninn.memory_edges WHERE from_id = $1",
            "to" => "SELECT * FROM muninn.memory_edges WHERE to_id = $1",
            _ => "SELECT * FROM muninn.memory_edges WHERE from_id = $1 OR to_id = $1",
        };

        let rows = client.query(sql, &[&id_str]).await.map_err(db_err)?;

        Ok(rows
            .iter()
            .map(|row| {
                let type_str: &str = row.get("type");
                let edge_type: EdgeType =
                    serde_json::from_value(serde_json::Value::String(type_str.to_string()))
                        .unwrap_or(EdgeType::Related);

                let id_str: &str = row.get("id");
                let from_str: &str = row.get("from_id");
                let to_str: &str = row.get("to_id");
                let strength: f32 = row.get("strength");

                MemoryEdge {
                    id: id_str.parse().unwrap_or_default(),
                    from_id: from_str.parse().unwrap_or_default(),
                    to_id: to_str.parse().unwrap_or_default(),
                    edge_type,
                    strength: strength as f64,
                    created_at: row.get("created_at"),
                }
            })
            .collect())
    }

    async fn delete_edge(&self, id: Uuid) -> StorageResult<()> {
        let client = self.get_client().await?;
        client
            .execute(
                "DELETE FROM muninn.memory_edges WHERE id = $1",
                &[&id.to_string()],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    // ── Debug ──────────────────────────────────────────────────

    async fn write_debug_log(&self, input: DebugLogInput) -> StorageResult<()> {
        let client = self.get_client().await?;
        let id = Uuid::new_v4();
        let event_str = serde_json::to_value(input.event)
            .unwrap()
            .as_str()
            .unwrap_or("search_scoring")
            .to_string();

        client
            .execute(
                "INSERT INTO muninn.debug_log (id, event, entry_id, data, duration_ms)
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &id.to_string(),
                    &event_str,
                    &input.entry_id.map(|id| id.to_string()),
                    &input.data,
                    // Column is REAL (float4); f64 would mismatch the wire type.
                    &input.duration_ms.map(|v| v as f32),
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn query_debug_log(&self, query: DebugLogQuery) -> StorageResult<Vec<DebugLogEntry>> {
        let client = self.get_client().await?;
        let limit = query.limit.unwrap_or(20) as i64;

        // Apply the same predicates as the sqlite twin — muninn_debug
        // forwards event/entryId/since filters.
        let event_str = query.event.as_ref().map(|e| {
            serde_json::to_value(e)
                .unwrap()
                .as_str()
                .unwrap_or("")
                .to_string()
        });
        let entry_id_str = query.entry_id.map(|u| u.to_string());
        let since = query.since;

        let mut sql = String::from("SELECT * FROM muninn.debug_log WHERE TRUE");
        let mut args: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
        if let Some(ref e) = event_str {
            args.push(e);
            sql.push_str(&format!(" AND event = ${}", args.len()));
        }
        if let Some(ref id) = entry_id_str {
            args.push(id);
            sql.push_str(&format!(" AND entry_id = ${}", args.len()));
        }
        if let Some(ref s) = since {
            args.push(s);
            sql.push_str(&format!(" AND created_at >= ${}", args.len()));
        }
        args.push(&limit);
        sql.push_str(&format!(" ORDER BY created_at DESC LIMIT ${}", args.len()));

        let rows = client.query(&sql, &args).await.map_err(db_err)?;

        Ok(rows
            .iter()
            .map(|row| {
                let event_str: &str = row.get("event");
                let event: DebugEvent =
                    serde_json::from_value(serde_json::Value::String(event_str.to_string()))
                        .unwrap_or(DebugEvent::SearchScoring);
                let entry_id_opt: Option<&str> = row.get("entry_id");
                let id_str: &str = row.get("id");

                DebugLogEntry {
                    id: id_str.parse().unwrap_or_default(),
                    event,
                    entry_id: entry_id_opt.and_then(|s| s.parse().ok()),
                    data: row.get("data"),
                    duration_ms: row.get::<_, Option<f32>>("duration_ms").map(|v| v as f64),
                    created_at: row.get("created_at"),
                }
            })
            .collect())
    }

    async fn prune_debug_log(&self, older_than_days: i64) -> StorageResult<i64> {
        let client = self.get_client().await?;
        let cutoff = Utc::now() - chrono::Duration::days(older_than_days);
        let deleted = client
            .execute(
                "DELETE FROM muninn.debug_log WHERE created_at < $1",
                &[&cutoff],
            )
            .await
            .map_err(db_err)?;
        Ok(deleted as i64)
    }

    // ── Stats ──────────────────────────────────────────────────

    async fn get_stats(&self, namespace: &str) -> StorageResult<MemoryStats> {
        let client = self.get_client().await?;
        let ns = if namespace.is_empty() {
            &self.default_namespace
        } else {
            namespace
        };

        let total_entries: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM muninn.memory_entries WHERE namespace = $1 AND deleted_at IS NULL",
                &[&ns],
            )
            .await
            .map_err(db_err)?
            .get(0);

        let total_sessions: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM muninn.sessions WHERE namespace = $1",
                &[&ns],
            )
            .await
            .map_err(db_err)?
            .get(0);

        let type_rows = client
            .query(
                "SELECT type, COUNT(*) FROM muninn.memory_entries
                 WHERE namespace = $1 AND deleted_at IS NULL GROUP BY type",
                &[&ns],
            )
            .await
            .map_err(db_err)?;

        let entries_by_type: Vec<(String, i64)> = type_rows
            .iter()
            .map(|r| (r.get::<_, &str>(0).to_string(), r.get(1)))
            .collect();

        let layer_rows = client
            .query(
                "SELECT layer, COUNT(*) FROM muninn.memory_entries
                 WHERE namespace = $1 AND deleted_at IS NULL GROUP BY layer",
                &[&ns],
            )
            .await
            .map_err(db_err)?;

        let entries_by_layer: Vec<(u8, i64)> = layer_rows
            .iter()
            .map(|r| (r.get::<_, i32>(0) as u8, r.get(1)))
            .collect();

        let ns_rows = client
            .query(
                "SELECT DISTINCT namespace FROM muninn.memory_entries WHERE deleted_at IS NULL",
                &[],
            )
            .await
            .map_err(db_err)?;

        let namespaces: Vec<String> = ns_rows
            .iter()
            .map(|r| r.get::<_, &str>(0).to_string())
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
        let client = self.get_client().await?;

        let total_entries: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM muninn.memory_entries WHERE deleted_at IS NULL",
                &[],
            )
            .await
            .map_err(db_err)?
            .get(0);
        let total_sessions: i64 = client
            .query_one("SELECT COUNT(*) FROM muninn.sessions", &[])
            .await
            .map_err(db_err)?
            .get(0);

        let entries_by_type: Vec<(String, i64)> = client
            .query(
                "SELECT type, COUNT(*) FROM muninn.memory_entries
                 WHERE deleted_at IS NULL GROUP BY type ORDER BY COUNT(*) DESC",
                &[],
            )
            .await
            .map_err(db_err)?
            .iter()
            .map(|r| (r.get::<_, &str>(0).to_string(), r.get::<_, i64>(1)))
            .collect();

        let entries_by_layer: Vec<(u8, i64)> = client
            .query(
                "SELECT layer, COUNT(*) FROM muninn.memory_entries
                 WHERE deleted_at IS NULL GROUP BY layer ORDER BY layer",
                &[],
            )
            .await
            .map_err(db_err)?
            .iter()
            .map(|r| (r.get::<_, i32>(0) as u8, r.get::<_, i64>(1)))
            .collect();

        let mut by_ns: std::collections::BTreeMap<String, NamespaceStats> = Default::default();
        for row in client
            .query(
                "SELECT namespace, COUNT(*) FROM muninn.memory_entries
                 WHERE deleted_at IS NULL GROUP BY namespace",
                &[],
            )
            .await
            .map_err(db_err)?
        {
            let ns: String = row.get::<_, &str>(0).to_string();
            by_ns
                .entry(ns.clone())
                .or_insert(NamespaceStats {
                    namespace: ns,
                    entries: 0,
                    sessions: 0,
                })
                .entries = row.get::<_, i64>(1);
        }
        for row in client
            .query(
                "SELECT namespace, COUNT(*) FROM muninn.sessions GROUP BY namespace",
                &[],
            )
            .await
            .map_err(db_err)?
        {
            let ns: String = row.get::<_, &str>(0).to_string();
            by_ns
                .entry(ns.clone())
                .or_insert(NamespaceStats {
                    namespace: ns,
                    entries: 0,
                    sessions: 0,
                })
                .sessions = row.get::<_, i64>(1);
        }
        let mut by_namespace: Vec<NamespaceStats> = by_ns.into_values().collect();
        by_namespace.sort_by_key(|n| std::cmp::Reverse(n.entries));

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
        let client = self.get_client().await?;

        let entries: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM muninn.memory_entries
                 WHERE (project_id = $1 OR namespace = $1) AND deleted_at IS NULL",
                &[&source],
            )
            .await
            .map_err(db_err)?
            .get(0);

        let sessions: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM muninn.sessions
                 WHERE project_id = $1 OR namespace = $1",
                &[&source],
            )
            .await
            .map_err(db_err)?
            .get(0);

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

        let client = self.get_client().await?;

        let entries = client
            .execute(
                "UPDATE muninn.memory_entries
                 SET project_id = $1, namespace = $1
                 WHERE (project_id = $2 OR namespace = $2) AND deleted_at IS NULL",
                &[&target, &source],
            )
            .await
            .map_err(db_err)? as i64;

        let sessions = client
            .execute(
                "UPDATE muninn.sessions
                 SET project_id = $1, namespace = $1
                 WHERE project_id = $2 OR namespace = $2",
                &[&target, &source],
            )
            .await
            .map_err(db_err)? as i64;

        Ok(MergeCounts { entries, sessions })
    }

    // ── Pending Observations ──────────────────────────────────────

    async fn enqueue_observation(
        &self,
        obs: ObservationInput,
        namespace: &str,
    ) -> StorageResult<Uuid> {
        let client = self.get_client().await?;
        let id = Uuid::new_v4();
        let session_id_str = obs.session_id.map(|u| u.to_string());

        client
            .execute(
                "INSERT INTO muninn.pending_observations
                 (id, namespace, session_id, project_id, tool_name,
                  tool_input, tool_response, content_hash, status)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending')",
                &[
                    &id.to_string(),
                    &namespace,
                    &session_id_str,
                    &obs.project_id,
                    &obs.tool_name,
                    &obs.tool_input,
                    &obs.tool_response,
                    &obs.content_hash,
                ],
            )
            .await
            .map_err(db_err)?;

        Ok(id)
    }

    async fn claim_observations(
        &self,
        namespace: &str,
        session_id: Option<Uuid>,
        max: usize,
    ) -> StorageResult<Vec<PendingObservation>> {
        let client = self.get_client().await?;
        let session_str = session_id.map(|u| u.to_string());
        let limit = max as i64;

        // SKIP LOCKED keeps concurrent claimers from blocking each other.
        let rows = if session_str.is_some() {
            client
                .query(
                    "UPDATE muninn.pending_observations
                     SET status = 'processing', claimed_at = NOW(),
                         attempt_count = attempt_count + 1
                     WHERE id IN (
                         SELECT id FROM muninn.pending_observations
                         WHERE namespace = $1 AND status = 'pending'
                           AND session_id = $2
                         ORDER BY created_at
                         LIMIT $3
                         FOR UPDATE SKIP LOCKED
                     )
                     RETURNING id, namespace, session_id, project_id, tool_name,
                               tool_input, tool_response, content_hash, status,
                               attempt_count, claimed_at, created_at, confirmed_at",
                    &[&namespace, &session_str, &limit],
                )
                .await
                .map_err(db_err)?
        } else {
            client
                .query(
                    "UPDATE muninn.pending_observations
                     SET status = 'processing', claimed_at = NOW(),
                         attempt_count = attempt_count + 1
                     WHERE id IN (
                         SELECT id FROM muninn.pending_observations
                         WHERE namespace = $1 AND status = 'pending'
                         ORDER BY created_at
                         LIMIT $2
                         FOR UPDATE SKIP LOCKED
                     )
                     RETURNING id, namespace, session_id, project_id, tool_name,
                               tool_input, tool_response, content_hash, status,
                               attempt_count, claimed_at, created_at, confirmed_at",
                    &[&namespace, &limit],
                )
                .await
                .map_err(db_err)?
        };

        Ok(rows.iter().map(row_to_pending_observation).collect())
    }

    async fn confirm_observations(&self, ids: &[Uuid]) -> StorageResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let client = self.get_client().await?;
        let id_strs: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
        let id_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = id_strs
            .iter()
            .map(|s| s as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();

        let placeholders: Vec<String> = (1..=id_strs.len()).map(|i| format!("${i}")).collect();
        let sql = format!(
            "UPDATE muninn.pending_observations
             SET status = 'confirmed', confirmed_at = NOW()
             WHERE id IN ({})",
            placeholders.join(",")
        );

        client
            .execute(sql.as_str(), &id_refs)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn recover_stale_observations(&self, older_than_secs: i64) -> StorageResult<i64> {
        let client = self.get_client().await?;
        let affected = client
            .execute(
                "UPDATE muninn.pending_observations
                 SET status = 'pending', claimed_at = NULL
                 WHERE status = 'processing'
                   AND claimed_at < NOW() - ($1 || ' seconds')::interval",
                &[&older_than_secs.to_string()],
            )
            .await
            .map_err(db_err)?;
        Ok(affected as i64)
    }

    async fn check_observation_duplicate(
        &self,
        content_hash: &str,
        window_secs: i64,
    ) -> StorageResult<bool> {
        let client = self.get_client().await?;
        let row = client
            .query_opt(
                "SELECT 1 FROM muninn.pending_observations
                 WHERE content_hash = $1
                   AND created_at > NOW() - ($2 || ' seconds')::interval
                 LIMIT 1",
                &[&content_hash, &window_secs.to_string()],
            )
            .await
            .map_err(db_err)?;
        Ok(row.is_some())
    }

    async fn touch_entries(&self, ids: &[Uuid]) -> StorageResult<i64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let client = self.get_client().await?;
        let id_strs: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
        let touched = client
            .execute(
                &format!(
                    "UPDATE muninn.memory_entries
                     SET access_count = access_count + 1,
                         last_accessed_at = NOW(),
                         updated_at = NOW(),
                         layer = CASE WHEN layer > {episodic} THEN {episodic} ELSE layer END
                     WHERE id = ANY($1) AND deleted_at IS NULL",
                    episodic = MemoryLayer::EPISODIC.value(),
                ),
                &[&id_strs],
            )
            .await
            .map_err(db_err)?;
        Ok(touched as i64)
    }

    async fn mark_injected(&self, ids: &[Uuid]) -> StorageResult<i64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let client = self.get_client().await?;
        let id_strs: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
        // `updated_at` is intentionally left alone: it drives dedup and
        // supersession, and serving an entry is not editing it.
        let marked = client
            .execute(
                "UPDATE muninn.memory_entries
                 SET injected_count = injected_count + 1,
                     last_injected_at = NOW()
                 WHERE id = ANY($1) AND deleted_at IS NULL",
                &[&id_strs],
            )
            .await
            .map_err(db_err)?;
        Ok(marked as i64)
    }

    // ── Two-stage GC ──────────────────────────────────────────

    async fn soft_delete_stale_crawl(
        &self,
        namespace: &str,
        age_days: i64,
        max: usize,
        dry_run: bool,
    ) -> StorageResult<Vec<Uuid>> {
        let client = self.get_client().await?;

        if dry_run {
            let rows = client
                .query(
                    "SELECT id FROM muninn.memory_entries
                     WHERE namespace = $1
                       AND deleted_at IS NULL
                       AND source = 'scout'
                       AND verified = FALSE
                       AND access_count = 0
                       AND last_accessed_at IS NULL
                       AND created_at < NOW() - ($2 || ' days')::interval
                     ORDER BY created_at
                     LIMIT $3",
                    &[&namespace, &age_days.to_string(), &(max as i64)],
                )
                .await
                .map_err(db_err)?;
            return Ok(rows
                .iter()
                .filter_map(|r| r.get::<_, &str>("id").parse().ok())
                .collect());
        }

        let rows = client
            .query(
                "UPDATE muninn.memory_entries
                 SET deleted_at = NOW(), updated_at = NOW()
                 WHERE id IN (
                     SELECT id FROM muninn.memory_entries
                     WHERE namespace = $1
                       AND deleted_at IS NULL
                       AND source = 'scout'
                       AND verified = FALSE
                       AND access_count = 0
                       AND last_accessed_at IS NULL
                       AND created_at < NOW() - ($2 || ' days')::interval
                     ORDER BY created_at
                     LIMIT $3
                 )
                 RETURNING id",
                &[&namespace, &age_days.to_string(), &(max as i64)],
            )
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get::<_, &str>("id").parse().ok())
            .collect())
    }

    async fn purge_soft_deleted(
        &self,
        namespace: Option<&str>,
        older_than_days: i64,
        max: usize,
        dry_run: bool,
    ) -> StorageResult<Vec<Uuid>> {
        let client = self.get_client().await?;
        let ns_owned = namespace.map(|s| s.to_string());
        let days = older_than_days.to_string();
        let limit = max as i64;

        let ns_clause = if ns_owned.is_some() {
            " AND namespace = $2"
        } else {
            ""
        };
        let inner = format!(
            "SELECT id FROM muninn.memory_entries
             WHERE deleted_at IS NOT NULL
               AND deleted_at < NOW() - ($1 || ' days')::interval{ns_clause}
             ORDER BY deleted_at
             LIMIT {limit}"
        );

        let mut args: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&days];
        if let Some(ref ns) = ns_owned {
            args.push(ns);
        }

        if dry_run {
            let rows = client.query(&inner, &args).await.map_err(db_err)?;
            return Ok(rows
                .iter()
                .filter_map(|r| r.get::<_, &str>("id").parse().ok())
                .collect());
        }

        let sql = format!("DELETE FROM muninn.memory_entries WHERE id IN ({inner}) RETURNING id");
        let rows = client.query(&sql, &args).await.map_err(db_err)?;

        // Queue hygiene rides along: confirmed observations are fully
        // processed and may predate enqueue-side redaction.
        let _ = client
            .execute(
                "DELETE FROM muninn.pending_observations
                 WHERE status = 'confirmed'
                   AND created_at < NOW() - ($1 || ' days')::interval",
                &[&days],
            )
            .await;

        Ok(rows
            .iter()
            .filter_map(|r| r.get::<_, &str>("id").parse().ok())
            .collect())
    }

    // ── Content-hash maintenance ──────────────────────────────

    async fn list_missing_content_hash(
        &self,
        limit: usize,
    ) -> StorageResult<Vec<(Uuid, String, String)>> {
        let client = self.get_client().await?;
        let rows = client
            .query(
                "SELECT id, title, content FROM muninn.memory_entries
                 WHERE content_hash IS NULL
                 LIMIT $1",
                &[&(limit as i64)],
            )
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let id: &str = row.get("id");
                let title: String = row.get("title");
                let content: String = row.get("content");
                id.parse().ok().map(|u| (u, title, content))
            })
            .collect())
    }

    async fn redact_entry_row(
        &self,
        id: Uuid,
        title: &str,
        content: &str,
        tags: &[String],
    ) -> StorageResult<()> {
        let client = self.get_client().await?;
        let tags_vec: Vec<String> = tags.to_vec();
        let hash = crate::storage::content_hash(title, content);
        client
            .execute(
                "UPDATE muninn.memory_entries
                 SET title = $1, content = $2, tags = $3, content_hash = $4, updated_at = NOW()
                 WHERE id = $5",
                &[&title, &content, &tags_vec, &hash, &id.to_string()],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn set_content_hash(&self, id: Uuid, hash: &str) -> StorageResult<()> {
        let client = self.get_client().await?;
        client
            .execute(
                "UPDATE muninn.memory_entries SET content_hash = $1 WHERE id = $2",
                &[&hash, &id.to_string()],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn find_duplicate_clusters(
        &self,
        namespace: Option<&str>,
    ) -> StorageResult<Vec<DuplicateCluster>> {
        let client = self.get_client().await?;
        let ns_owned = namespace.map(|s| s.to_string());
        let ns_clause = if ns_owned.is_some() {
            " AND namespace = $1"
        } else {
            ""
        };
        let sql = format!(
            "SELECT e.content_hash, e.id, e.namespace, e.title, e.access_count, e.verified, e.created_at
             FROM muninn.memory_entries e
             JOIN (SELECT namespace, content_hash FROM muninn.memory_entries
                   WHERE deleted_at IS NULL AND content_hash IS NOT NULL{ns_clause}
                   GROUP BY namespace, content_hash HAVING COUNT(*) > 1) d
               ON e.namespace = d.namespace AND e.content_hash = d.content_hash
             WHERE e.deleted_at IS NULL
             ORDER BY e.namespace, e.content_hash, e.access_count DESC, e.created_at DESC"
        );
        let rows = if let Some(ref ns) = ns_owned {
            client.query(&sql, &[ns]).await.map_err(db_err)?
        } else {
            client.query(&sql, &[]).await.map_err(db_err)?
        };

        let mut clusters: Vec<DuplicateCluster> = Vec::new();
        for row in &rows {
            let hash: String = row.get("content_hash");
            let id: &str = row.get("id");
            let member = DupMember {
                id: id.parse().unwrap_or_default(),
                namespace: row.get("namespace"),
                title: row.get("title"),
                access_count: row.get::<_, i32>("access_count") as i64,
                verified: row.get("verified"),
                created_at: row.get("created_at"),
            };
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
        let client = self.get_client().await?;
        let id_str = id.to_string();
        let verified_by_owned = verified_by.map(|s| s.to_string());

        let updated = client
            .execute(
                "UPDATE muninn.memory_entries
                 SET verified = TRUE,
                     verified_at = NOW(),
                     verified_by = $2,
                     updated_at = NOW()
                 WHERE id = $1 AND deleted_at IS NULL",
                &[&id_str, &verified_by_owned],
            )
            .await
            .map_err(db_err)?;
        if updated == 0 {
            return Err(StorageError::NotFound(id));
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
        let client = self.get_client().await?;
        let archival = MemoryLayer::ARCHIVAL.value() as i32;

        // Single-shot UPDATE ... RETURNING id. Verified entries excluded.
        let rows = client
            .query(
                "UPDATE muninn.memory_entries
                 SET deleted_at = NOW(), updated_at = NOW()
                 WHERE id IN (
                     SELECT id FROM muninn.memory_entries
                     WHERE namespace = $1
                       AND deleted_at IS NULL
                       AND layer = $2
                       AND verified = FALSE
                       AND access_count = 0
                       AND confidence < $3
                       AND COALESCE(last_accessed_at, created_at) < NOW() - ($4 || ' days')::interval
                     ORDER BY COALESCE(last_accessed_at, created_at)
                     LIMIT $5
                 )
                 RETURNING id",
                &[
                    &namespace,
                    &archival,
                    &conf_cap,
                    &age_days.to_string(),
                    &(max as i64),
                ],
            )
            .await
            .map_err(db_err)?;

        Ok(rows
            .iter()
            .filter_map(|row| {
                let id_str: &str = row.get("id");
                id_str.parse().ok()
            })
            .collect())
    }

    async fn list_all_edges(&self, limit: usize) -> StorageResult<Vec<MemoryEdge>> {
        let client = self.get_client().await?;
        let rows = client
            .query(
                "SELECT id, from_id, to_id, type, strength, created_at
                 FROM muninn.memory_edges
                 ORDER BY created_at
                 LIMIT $1",
                &[&(limit as i64)],
            )
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .map(|row| {
                let type_str: &str = row.get("type");
                let edge_type: EdgeType =
                    serde_json::from_value(serde_json::Value::String(type_str.to_string()))
                        .unwrap_or(EdgeType::Related);
                let id_str: &str = row.get("id");
                let from_str: &str = row.get("from_id");
                let to_str: &str = row.get("to_id");
                let strength: f32 = row.get("strength");
                MemoryEdge {
                    id: id_str.parse().unwrap_or_default(),
                    from_id: from_str.parse().unwrap_or_default(),
                    to_id: to_str.parse().unwrap_or_default(),
                    edge_type,
                    strength: strength as f64,
                    created_at: row.get("created_at"),
                }
            })
            .collect())
    }

    async fn import_edge(&self, edge: MemoryEdge) -> StorageResult<bool> {
        let client = self.get_client().await?;
        let edge_type_str = serde_json::to_value(edge.edge_type)
            .unwrap()
            .as_str()
            .unwrap_or("related")
            .to_string();
        let affected = client
            .execute(
                "INSERT INTO muninn.memory_edges (id, from_id, to_id, type, strength, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (id) DO NOTHING",
                &[
                    &edge.id.to_string(),
                    &edge.from_id.to_string(),
                    &edge.to_id.to_string(),
                    &edge_type_str,
                    &(edge.strength as f32),
                    &edge.created_at,
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(affected == 1)
    }

    async fn import_session(&self, session: Session) -> StorageResult<bool> {
        // Same invariant repair as import_entry (see sqlite twin).
        let mut session = session;
        if let Some(ref pid) = session.project_id {
            if !pid.is_empty() && session.namespace != *pid {
                session.namespace = pid.clone();
            }
        }
        let client = self.get_client().await?;
        let status_str = serde_json::to_value(session.status)
            .unwrap()
            .as_str()
            .unwrap_or("active")
            .to_string();
        let affected = client
            .execute(
                "INSERT INTO muninn.sessions
                    (id, namespace, project_id, tool, goal, summary, discoveries, files_modified,
                     status, started_at, ended_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 ON CONFLICT (id) DO NOTHING",
                &[
                    &session.id.to_string(),
                    &session.namespace,
                    &session.project_id,
                    &session.tool,
                    &session.goal,
                    &session.summary,
                    &session.discoveries,
                    &session.files_modified,
                    &status_str,
                    &session.started_at,
                    &session.ended_at,
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(affected == 1)
    }

    // ── Phase 5.6 — Sync (outbox + state + conflicts) ─────────

    async fn enqueue_outbox(&self, op: crate::types::OutboxInput) -> StorageResult<Uuid> {
        let client = self.get_client().await?;
        let id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO muninn.sync_outbox
                    (id, entry_id, op_kind, row_payload)
                 VALUES ($1, $2, $3, $4)",
                &[
                    &id.to_string(),
                    &op.entry_id.to_string(),
                    &op.op_kind.as_str(),
                    &op.row_payload,
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(id)
    }

    async fn claim_outbox(
        &self,
        max: usize,
        max_attempts: i32,
    ) -> StorageResult<Vec<crate::types::OutboxRow>> {
        let mut client = self.get_client().await?;
        let tx = client.transaction().await.map_err(db_err)?;
        let claim_max = max as i64;
        // Atomic claim via UPDATE...RETURNING.
        let rows = tx
            .query(
                "UPDATE muninn.sync_outbox
                    SET claimed_at = NOW()
                    WHERE id IN (
                        SELECT id FROM muninn.sync_outbox
                        WHERE confirmed_at IS NULL AND claimed_at IS NULL
                          AND attempts < $2
                        ORDER BY created_at ASC
                        LIMIT $1
                        FOR UPDATE SKIP LOCKED
                    )
                    RETURNING id, entry_id, op_kind, row_payload, attempts,
                              last_error, claimed_at, confirmed_at, created_at",
                &[&claim_max, &max_attempts],
            )
            .await
            .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.get("id");
            let entry_id_str: String = row.get("entry_id");
            let op_kind_str: String = row.get("op_kind");
            let payload: serde_json::Value = row.get("row_payload");
            out.push(crate::types::OutboxRow {
                id: Uuid::parse_str(&id_str).map_err(|e| StorageError::Database(e.to_string()))?,
                entry_id: Uuid::parse_str(&entry_id_str)
                    .map_err(|e| StorageError::Database(e.to_string()))?,
                op_kind: crate::types::OutboxOp::parse(&op_kind_str)
                    .ok_or_else(|| StorageError::Database(format!("bad op_kind {op_kind_str}")))?,
                row_payload: payload,
                attempts: row.get("attempts"),
                last_error: row.get("last_error"),
                claimed_at: row.get("claimed_at"),
                confirmed_at: row.get("confirmed_at"),
                created_at: row.get("created_at"),
            });
        }
        tx.commit().await.map_err(db_err)?;
        out.sort_by_key(|a| a.created_at);
        Ok(out)
    }

    async fn confirm_outbox(&self, ids: &[Uuid]) -> StorageResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let client = self.get_client().await?;
        let id_strs: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
        client
            .execute(
                "UPDATE muninn.sync_outbox
                 SET confirmed_at = NOW()
                 WHERE id = ANY($1)",
                &[&id_strs],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn fail_outbox(&self, id: Uuid, err: &str) -> StorageResult<()> {
        let client = self.get_client().await?;
        client
            .execute(
                "UPDATE muninn.sync_outbox
                 SET attempts = attempts + 1, last_error = $1, claimed_at = NULL
                 WHERE id = $2",
                &[&err, &id.to_string()],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn reap_stale_claims(&self, older_than_secs: i64) -> StorageResult<u64> {
        let client = self.get_client().await?;
        let affected = client
            .execute(
                "UPDATE muninn.sync_outbox SET claimed_at = NULL
                 WHERE claimed_at IS NOT NULL
                   AND confirmed_at IS NULL
                   AND claimed_at < NOW() - make_interval(secs => $1::DOUBLE PRECISION)",
                &[&(older_than_secs as f64)],
            )
            .await
            .map_err(db_err)?;
        Ok(affected)
    }

    async fn release_outbox(&self, ids: &[Uuid]) -> StorageResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let client = self.get_client().await?;
        let id_strs: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
        client
            .execute(
                "UPDATE muninn.sync_outbox SET claimed_at = NULL
                 WHERE id = ANY($1) AND confirmed_at IS NULL",
                &[&id_strs],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn outbox_health(&self, max_attempts: i32) -> StorageResult<crate::types::OutboxHealth> {
        let client = self.get_client().await?;
        let row = client
            .query_one(
                "SELECT
                   COALESCE(count(*) FILTER (
                     WHERE claimed_at IS NULL AND attempts < $1), 0)::BIGINT AS pending,
                   COALESCE(count(*) FILTER (
                     WHERE claimed_at IS NOT NULL AND attempts < $1), 0)::BIGINT AS in_flight,
                   COALESCE(count(*) FILTER (WHERE attempts >= $1), 0)::BIGINT AS dead,
                   min(created_at) AS oldest,
                   COALESCE(max(attempts), 0)::INT AS max_seen
                 FROM muninn.sync_outbox WHERE confirmed_at IS NULL",
                &[&max_attempts],
            )
            .await
            .map_err(db_err)?;
        Ok(crate::types::OutboxHealth {
            pending: row.get::<_, i64>("pending") as u64,
            in_flight: row.get::<_, i64>("in_flight") as u64,
            dead_lettered: row.get::<_, i64>("dead") as u64,
            oldest_unconfirmed: row.get("oldest"),
            max_attempts_seen: row.get("max_seen"),
        })
    }

    async fn deleted_entries_without_tombstone(&self, limit: usize) -> StorageResult<Vec<Uuid>> {
        let client = self.get_client().await?;
        let rows = client
            .query(
                "SELECT e.id FROM muninn.memory_entries e
                 WHERE e.deleted_at IS NOT NULL
                   AND EXISTS (
                         SELECT 1 FROM muninn.sync_outbox o WHERE o.entry_id = e.id)
                   AND NOT EXISTS (
                         SELECT 1 FROM muninn.sync_outbox o
                         WHERE o.entry_id = e.id AND o.op_kind = 'delete')
                 ORDER BY e.deleted_at ASC
                 LIMIT $1",
                &[&(limit as i64)],
            )
            .await
            .map_err(db_err)?;
        rows.iter()
            .map(|r| {
                let s: String = r.get("id");
                Uuid::parse_str(&s).map_err(|e| StorageError::Database(e.to_string()))
            })
            .collect()
    }

    async fn malformed_delete_payloads(&self, limit: usize) -> StorageResult<Vec<(Uuid, Uuid)>> {
        let client = self.get_client().await?;
        let rows = client
            .query(
                "SELECT id, entry_id FROM muninn.sync_outbox
                 WHERE confirmed_at IS NULL
                   AND op_kind = 'delete'
                   AND row_payload->>'title' IS NULL
                 ORDER BY created_at ASC
                 LIMIT $1",
                &[&(limit as i64)],
            )
            .await
            .map_err(db_err)?;
        rows.iter()
            .map(|r| {
                let a: String = r.get("id");
                let b: String = r.get("entry_id");
                Ok((
                    Uuid::parse_str(&a).map_err(|e| StorageError::Database(e.to_string()))?,
                    Uuid::parse_str(&b).map_err(|e| StorageError::Database(e.to_string()))?,
                ))
            })
            .collect()
    }

    async fn rewrite_outbox_payload(
        &self,
        outbox_id: Uuid,
        payload: &serde_json::Value,
    ) -> StorageResult<()> {
        let client = self.get_client().await?;
        client
            .execute(
                "UPDATE muninn.sync_outbox
                 SET row_payload = $1, attempts = 0, last_error = NULL, claimed_at = NULL
                 WHERE id = $2 AND confirmed_at IS NULL",
                &[payload, &outbox_id.to_string()],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn outbox_depth(&self) -> StorageResult<usize> {
        let client = self.get_client().await?;
        let row = client
            .query_one(
                "SELECT count(*)::BIGINT AS c FROM muninn.sync_outbox WHERE confirmed_at IS NULL",
                &[],
            )
            .await
            .map_err(db_err)?;
        let count: i64 = row.get("c");
        Ok(count as usize)
    }

    async fn gc_outbox(&self, older_than_secs: i64) -> StorageResult<i64> {
        let client = self.get_client().await?;
        let interval = format!("{} seconds", older_than_secs);
        let affected = client
            .execute(
                "DELETE FROM muninn.sync_outbox
                 WHERE confirmed_at IS NOT NULL
                   AND confirmed_at < NOW() - $1::interval",
                &[&interval],
            )
            .await
            .map_err(db_err)?;
        Ok(affected as i64)
    }

    async fn read_sync_state(&self) -> StorageResult<crate::types::SyncState> {
        let client = self.get_client().await?;
        let row_opt = client
            .query_opt(
                "SELECT last_pulled_updated_at, last_pulled_session_at,
                        last_pulled_edge_at, last_push_at, last_pull_at,
                        local_dim, remote_dim,
                        local_schema_version, remote_schema_version,
                        initialized_at
                 FROM muninn.sync_state WHERE id = 1",
                &[],
            )
            .await
            .map_err(db_err)?;
        let Some(row) = row_opt else {
            return Ok(crate::types::SyncState::default());
        };
        Ok(crate::types::SyncState {
            last_pulled_updated_at: row.get("last_pulled_updated_at"),
            last_pulled_session_at: row.get("last_pulled_session_at"),
            last_pulled_edge_at: row.get("last_pulled_edge_at"),
            last_push_at: row.get("last_push_at"),
            last_pull_at: row.get("last_pull_at"),
            local_dim: row.get("local_dim"),
            remote_dim: row.get("remote_dim"),
            local_schema_version: row.get("local_schema_version"),
            remote_schema_version: row.get("remote_schema_version"),
            initialized_at: row.get("initialized_at"),
        })
    }

    async fn write_sync_state(&self, state: &crate::types::SyncState) -> StorageResult<()> {
        let client = self.get_client().await?;
        client
            .execute(
                "INSERT INTO muninn.sync_state
                    (id, last_pulled_updated_at, last_pulled_session_at,
                     last_pulled_edge_at, last_push_at, last_pull_at,
                     local_dim, remote_dim, local_schema_version,
                     remote_schema_version, initialized_at)
                 VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (id) DO UPDATE SET
                    last_pulled_updated_at = EXCLUDED.last_pulled_updated_at,
                    last_pulled_session_at = EXCLUDED.last_pulled_session_at,
                    last_pulled_edge_at = EXCLUDED.last_pulled_edge_at,
                    last_push_at = EXCLUDED.last_push_at,
                    last_pull_at = EXCLUDED.last_pull_at,
                    local_dim = EXCLUDED.local_dim,
                    remote_dim = EXCLUDED.remote_dim,
                    local_schema_version = EXCLUDED.local_schema_version,
                    remote_schema_version = EXCLUDED.remote_schema_version,
                    initialized_at = EXCLUDED.initialized_at",
                &[
                    &state.last_pulled_updated_at,
                    &state.last_pulled_session_at,
                    &state.last_pulled_edge_at,
                    &state.last_push_at,
                    &state.last_pull_at,
                    &state.local_dim,
                    &state.remote_dim,
                    &state.local_schema_version,
                    &state.remote_schema_version,
                    &state.initialized_at,
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn record_conflict(&self, c: &crate::types::SyncConflict) -> StorageResult<()> {
        let client = self.get_client().await?;
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
        client
            .execute(
                "INSERT INTO muninn.sync_conflicts
                    (id, entry_id, direction, policy, winner_side,
                     local_updated_at, remote_updated_at,
                     local_payload, remote_payload, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (id) DO NOTHING",
                &[
                    &c.id.to_string(),
                    &c.entry_id.to_string(),
                    &direction,
                    &policy,
                    &winner,
                    &c.local_updated_at,
                    &c.remote_updated_at,
                    &c.local_payload,
                    &c.remote_payload,
                    &c.created_at,
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn list_conflicts(&self, limit: usize) -> StorageResult<Vec<crate::types::SyncConflict>> {
        let client = self.get_client().await?;
        let rows = client
            .query(
                "SELECT id, entry_id, direction, policy, winner_side,
                        local_updated_at, remote_updated_at,
                        local_payload, remote_payload, created_at
                 FROM muninn.sync_conflicts
                 ORDER BY created_at DESC
                 LIMIT $1",
                &[&(limit as i64)],
            )
            .await
            .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.get("id");
            let entry_id_str: String = row.get("entry_id");
            let direction_str: String = row.get("direction");
            let policy_str: String = row.get("policy");
            let winner_str: String = row.get("winner_side");
            out.push(crate::types::SyncConflict {
                id: Uuid::parse_str(&id_str).map_err(|e| StorageError::Database(e.to_string()))?,
                entry_id: Uuid::parse_str(&entry_id_str)
                    .map_err(|e| StorageError::Database(e.to_string()))?,
                direction: serde_json::from_value(serde_json::Value::String(direction_str))
                    .map_err(|e| StorageError::Serialization(e.to_string()))?,
                policy: serde_json::from_value(serde_json::Value::String(policy_str))
                    .map_err(|e| StorageError::Serialization(e.to_string()))?,
                winner_side: serde_json::from_value(serde_json::Value::String(winner_str))
                    .map_err(|e| StorageError::Serialization(e.to_string()))?,
                local_updated_at: row.get("local_updated_at"),
                remote_updated_at: row.get("remote_updated_at"),
                local_payload: row.get("local_payload"),
                remote_payload: row.get("remote_payload"),
                created_at: row.get("created_at"),
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
        let client = self.get_client().await?;
        let upper_bound: DateTime<Utc> =
            chrono::Utc::now() - chrono::Duration::seconds(clock_skew_secs);

        let rows = match (after, project_filter) {
            (Some(cursor), Some(project)) => client
                .query(
                    "SELECT * FROM muninn.memory_entries
                         WHERE updated_at > $1 AND updated_at <= $2
                           AND project_id = $3
                         ORDER BY updated_at ASC, id ASC
                         LIMIT $4",
                    &[&cursor, &upper_bound, &project, &(limit as i64)],
                )
                .await
                .map_err(db_err)?,
            (Some(cursor), None) => client
                .query(
                    "SELECT * FROM muninn.memory_entries
                     WHERE updated_at > $1 AND updated_at <= $2
                     ORDER BY updated_at ASC, id ASC
                     LIMIT $3",
                    &[&cursor, &upper_bound, &(limit as i64)],
                )
                .await
                .map_err(db_err)?,
            (None, Some(project)) => client
                .query(
                    "SELECT * FROM muninn.memory_entries
                     WHERE updated_at <= $1 AND project_id = $2
                     ORDER BY updated_at ASC, id ASC
                     LIMIT $3",
                    &[&upper_bound, &project, &(limit as i64)],
                )
                .await
                .map_err(db_err)?,
            (None, None) => client
                .query(
                    "SELECT * FROM muninn.memory_entries
                     WHERE updated_at <= $1
                     ORDER BY updated_at ASC, id ASC
                     LIMIT $2",
                    &[&upper_bound, &(limit as i64)],
                )
                .await
                .map_err(db_err)?,
        };

        Ok(rows.iter().map(row_to_entry).collect())
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
        // Re-apply the migration-012 invariant on import (see sqlite twin).
        let mut entry = entry;
        if let Some(ref pid) = entry.project_id {
            if !pid.is_empty() && entry.namespace != *pid {
                entry.namespace = pid.clone();
            }
        }
        let client = self.get_client().await?;
        let source_str = serde_json::to_value(entry.source)
            .unwrap()
            .as_str()
            .unwrap_or("human")
            .to_string();
        let type_str = entry.entry_type.as_str();
        let layer_val = entry.layer.value() as i32;
        let confidence = entry.confidence.clamp(0.0, 1.0);

        let affected = client
            .execute(
                "INSERT INTO muninn.memory_entries
                    (id, namespace, title, content, type, tags, project_id,
                     source, layer, confidence, topic_key, access_count,
                     verified, verified_at, author, verified_by,
                     created_at, updated_at, last_accessed_at, deleted_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                         $13, $14, $15, $16, $17, $18, $19, $20)
                 ON CONFLICT (id) DO NOTHING",
                &[
                    &entry.id.to_string(),
                    &entry.namespace,
                    &entry.title,
                    &entry.content,
                    &type_str,
                    &entry.tags,
                    &entry.project_id,
                    &source_str,
                    &layer_val,
                    &confidence,
                    &entry.topic_key,
                    &entry.access_count,
                    &entry.verified,
                    &entry.verified_at,
                    &entry.author,
                    &entry.verified_by,
                    &entry.created_at,
                    &entry.updated_at,
                    &entry.last_accessed_at,
                    &entry.deleted_at,
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(affected == 1)
    }
}

fn row_to_pending_observation(row: &tokio_postgres::Row) -> PendingObservation {
    let id_str: &str = row.get("id");
    let namespace: String = row.get("namespace");
    let session_id_str: Option<String> = row.get("session_id");
    let project_id: Option<String> = row.get("project_id");
    let tool_name: String = row.get("tool_name");
    let tool_input: serde_json::Value = row.get("tool_input");
    let tool_response: serde_json::Value = row.get("tool_response");
    let content_hash: String = row.get("content_hash");
    let status_str: &str = row.get("status");
    let attempt_count: i32 = row.get("attempt_count");
    let claimed_at: Option<DateTime<Utc>> = row.get("claimed_at");
    let created_at: DateTime<Utc> = row.get("created_at");
    let confirmed_at: Option<DateTime<Utc>> = row.get("confirmed_at");

    let status = match status_str {
        "processing" => PendingStatus::Processing,
        "confirmed" => PendingStatus::Confirmed,
        _ => PendingStatus::Pending,
    };

    PendingObservation {
        id: id_str.parse().unwrap_or_default(),
        namespace,
        session_id: session_id_str.and_then(|s| s.parse().ok()),
        project_id,
        tool_name,
        tool_input,
        tool_response,
        content_hash,
        status,
        attempt_count,
        claimed_at,
        created_at,
        confirmed_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point: a server rejection must name itself. The driver's
    /// own `to_string()` renders "db error" and nothing else, which is how
    /// an outbox row reached 64,634 attempts undiagnosed.
    #[test]
    fn a_server_rejection_reports_what_the_server_said() {
        let msg = format_db_error(
            "23505",
            "duplicate key value violates unique constraint",
            Some("Key (id)=(abc) already exists."),
            None,
            Some("memory_entries"),
            None,
            Some("memory_entries_pkey"),
        );
        assert!(msg.contains("23505"), "SQLSTATE is the searchable part");
        assert!(msg.contains("duplicate key value"));
        assert!(msg.contains("Key (id)=(abc) already exists."));
        assert!(msg.contains("at memory_entries"));
        assert!(msg.contains("constraint memory_entries_pkey"));
        assert!(
            !msg.contains("db error"),
            "must not degrade to the driver's placeholder"
        );
    }

    /// `detail` carries the whole failing row. It must be bounded before
    /// it is persisted to `sync_outbox.last_error`, and it must not be
    /// sliced by byte index — row content is arbitrary UTF-8 and the
    /// repo has a history of truncation panics on multi-byte input.
    #[test]
    fn oversized_detail_is_bounded_on_a_char_boundary() {
        let short = truncate_detail("plenty short", MAX_DETAIL_CHARS);
        assert_eq!(short, "plenty short", "under the cap, keep it verbatim");

        let huge = "é".repeat(5_000); // 2 bytes per char
        let cut = truncate_detail(&huge, MAX_DETAIL_CHARS);
        assert!(cut.ends_with("… [truncated]"));
        assert_eq!(
            cut.chars().filter(|c| *c == 'é').count(),
            MAX_DETAIL_CHARS,
            "cap counts chars, not bytes"
        );

        // Exactly at the cap is not truncated.
        let exact = "x".repeat(MAX_DETAIL_CHARS);
        assert_eq!(truncate_detail(&exact, MAX_DETAIL_CHARS), exact);

        let msg = format_db_error(
            "23514",
            "violates check",
            Some(&huge),
            None,
            None,
            None,
            None,
        );
        assert!(msg.contains("[truncated]"));
        assert!(msg.len() < 1_000, "bounded, got {}", msg.len());
    }

    /// Fields the server omitted must not leave empty parens or stray
    /// separators behind.
    #[test]
    fn omitted_fields_leave_no_debris() {
        assert_eq!(
            format_db_error(
                "42P01",
                "relation does not exist",
                None,
                None,
                None,
                None,
                None
            ),
            "[42P01] relation does not exist"
        );
        assert_eq!(
            format_db_error(
                "22001",
                "value too long",
                None,
                None,
                Some("t"),
                Some("c"),
                None
            ),
            "[22001] value too long (at t.c)"
        );
        // column without table is meaningless on its own — drop it
        assert_eq!(
            format_db_error("22001", "value too long", None, None, None, Some("c"), None),
            "[22001] value too long"
        );
    }

    /// Broken URL → pool acquire must time out within the configured window
    /// instead of hanging. Guards Phase 4.8 item 4.8.17 regression.
    #[tokio::test]
    async fn get_client_times_out_when_host_unreachable() {
        // Host reserved for documentation (TEST-NET-1); packets drop silently.
        // Port 55999 is arbitrary closed.
        std::env::set_var("RUNAR_DB_CONNECT_TIMEOUT_MS", "300");
        let url = "postgresql://user:pw@192.0.2.1:55999/db";
        let adapter = PostgresAdapter::new(url, "test").expect("pool creates lazily");

        let start = std::time::Instant::now();
        let result = adapter.get_client().await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected timeout error, got Ok");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "get_client should fail fast, took {:?}",
            elapsed
        );
        std::env::remove_var("RUNAR_DB_CONNECT_TIMEOUT_MS");
    }
}
