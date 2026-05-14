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

fn db_err(e: tokio_postgres::Error) -> StorageError {
    StorageError::Database(e.to_string())
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
                    (id, namespace, title, content, type, tags, project_id, source, layer, confidence, topic_key, author)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
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

    async fn update(&self, id: Uuid, updates: serde_json::Value) -> StorageResult<MemoryEntry> {
        let client = self.get_client().await?;

        if let Some(obj) = updates.as_object() {
            // Apply each field update individually to avoid dynamic dispatch issues
            for (key, value) in obj {
                match key.as_str() {
                    "title" => {
                        let v = value.as_str().unwrap_or("");
                        client
                            .execute(
                                "UPDATE muninn.memory_entries SET title = $1 WHERE id = $2 AND deleted_at IS NULL",
                                &[&v, &id.to_string()],
                            )
                            .await
                            .map_err(db_err)?;
                    }
                    "content" => {
                        let v = value.as_str().unwrap_or("");
                        client
                            .execute(
                                "UPDATE muninn.memory_entries SET content = $1 WHERE id = $2 AND deleted_at IS NULL",
                                &[&v, &id.to_string()],
                            )
                            .await
                            .map_err(db_err)?;
                    }
                    "last_accessed_at" => {
                        let v = value.as_str().unwrap_or("");
                        client
                            .execute(
                                "UPDATE muninn.memory_entries SET last_accessed_at = $1::timestamptz WHERE id = $2 AND deleted_at IS NULL",
                                &[&v, &id.to_string()],
                            )
                            .await
                            .map_err(db_err)?;
                    }
                    "layer" => {
                        let v = value.as_i64().unwrap_or(3) as i32;
                        client
                            .execute(
                                "UPDATE muninn.memory_entries SET layer = $1 WHERE id = $2 AND deleted_at IS NULL",
                                &[&v, &id.to_string()],
                            )
                            .await
                            .map_err(db_err)?;
                    }
                    "access_count" => {
                        let v = value.as_i64().unwrap_or(0) as i32;
                        client
                            .execute(
                                "UPDATE muninn.memory_entries SET access_count = $1 WHERE id = $2 AND deleted_at IS NULL",
                                &[&v, &id.to_string()],
                            )
                            .await
                            .map_err(db_err)?;
                    }
                    _ => {}
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

        let rows = match (&type_filter, &project_filter) {
            (Some(t), Some(p)) => {
                client
                    .query(
                        "SELECT * FROM muninn.memory_entries
                     WHERE namespace = $1 AND deleted_at IS NULL AND type = $2 AND project_id = $3
                     ORDER BY created_at DESC LIMIT $4 OFFSET $5",
                        &[&ns, t, p, &limit, &offset],
                    )
                    .await
            }
            (Some(t), None) => {
                client
                    .query(
                        "SELECT * FROM muninn.memory_entries
                     WHERE namespace = $1 AND deleted_at IS NULL AND type = $2
                     ORDER BY created_at DESC LIMIT $3 OFFSET $4",
                        &[&ns, t, &limit, &offset],
                    )
                    .await
            }
            (None, Some(p)) => {
                client
                    .query(
                        "SELECT * FROM muninn.memory_entries
                     WHERE namespace = $1 AND deleted_at IS NULL AND project_id = $2
                     ORDER BY created_at DESC LIMIT $3 OFFSET $4",
                        &[&ns, p, &limit, &offset],
                    )
                    .await
            }
            (None, None) => {
                client
                    .query(
                        "SELECT * FROM muninn.memory_entries
                     WHERE namespace = $1 AND deleted_at IS NULL
                     ORDER BY created_at DESC LIMIT $2 OFFSET $3",
                        &[&ns, &limit, &offset],
                    )
                    .await
            }
        }
        .map_err(db_err)?;

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
        _query: &str,
        query_embedding: &[f32],
        limit: usize,
        namespace: &str,
    ) -> StorageResult<Vec<MemoryEntry>> {
        let client = self.get_client().await?;
        let ns = if namespace.is_empty() {
            &self.default_namespace
        } else {
            namespace
        };
        let vec = Vector::from(query_embedding.to_vec());

        let rows = client
            .query(
                "SELECT *, 1 - (embedding <=> $1) AS similarity
                 FROM muninn.memory_entries
                 WHERE namespace = $2
                   AND deleted_at IS NULL
                   AND embedding IS NOT NULL
                   AND 1 - (embedding <=> $1) >= 0.65
                 ORDER BY embedding <=> $1
                 LIMIT $3",
                &[&vec, &ns, &(limit as i64)],
            )
            .await
            .map_err(db_err)?;

        Ok(rows.iter().map(row_to_entry).collect())
    }

    async fn fts_search(&self, query: SearchQuery) -> StorageResult<Vec<MemoryEntry>> {
        let client = self.get_client().await?;
        let ns = query
            .namespace
            .as_deref()
            .unwrap_or(&self.default_namespace);
        let limit = query.limit.unwrap_or(10) as i64;

        let rows = client
            .query(
                "SELECT *, ts_rank(fts_vector, plainto_tsquery('english', $1)) AS rank
                 FROM muninn.memory_entries
                 WHERE namespace = $2
                   AND deleted_at IS NULL
                   AND fts_vector @@ plainto_tsquery('english', $1)
                 ORDER BY rank DESC
                 LIMIT $3",
                &[&query.query, &ns, &limit],
            )
            .await
            .map_err(db_err)?;

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
                "INSERT INTO muninn.debug_log (id, event, entry_id, data)
                 VALUES ($1, $2, $3, $4)",
                &[
                    &id.to_string(),
                    &event_str,
                    &input.entry_id.map(|id| id.to_string()),
                    &input.data,
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn query_debug_log(&self, query: DebugLogQuery) -> StorageResult<Vec<DebugLogEntry>> {
        let client = self.get_client().await?;
        let limit = query.limit.unwrap_or(20) as i64;

        // Use a simple query without dynamic params to satisfy Send bounds
        let rows = client
            .query(
                "SELECT * FROM muninn.debug_log ORDER BY created_at DESC LIMIT $1",
                &[&limit],
            )
            .await
            .map_err(db_err)?;

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
                       AND last_accessed_at IS NOT NULL
                       AND last_accessed_at < NOW() - ($4 || ' days')::interval
                     ORDER BY last_accessed_at
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
        let client = self.get_client().await?;
        let status_str = serde_json::to_value(session.status)
            .unwrap()
            .as_str()
            .unwrap_or("active")
            .to_string();
        let affected = client
            .execute(
                "INSERT INTO muninn.sessions
                    (id, namespace, project_id, tool, goal, summary, status, started_at, ended_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT (id) DO NOTHING",
                &[
                    &session.id.to_string(),
                    &session.namespace,
                    &session.project_id,
                    &session.tool,
                    &session.goal,
                    &session.summary,
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

    async fn claim_outbox(&self, max: usize) -> StorageResult<Vec<crate::types::OutboxRow>> {
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
                        ORDER BY created_at ASC
                        LIMIT $1
                        FOR UPDATE SKIP LOCKED
                    )
                    RETURNING id, entry_id, op_kind, row_payload, attempts,
                              last_error, claimed_at, confirmed_at, created_at",
                &[&claim_max],
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
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
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
