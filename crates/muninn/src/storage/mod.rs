pub mod postgres;
pub mod sqlite;

use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    ApplyOutcome, DebugLogEntry, DebugLogInput, DebugLogQuery, ListFilters, MemoryEdge,
    MemoryEdgeInput, MemoryEntry, MemoryEntryInput, MemoryStats, MergeCounts, ObservationInput,
    OutboxInput, OutboxRow, PendingObservation, SaveResult, SearchQuery, Session, SessionInput,
    SessionUpdate, SyncConflict, SyncState,
};

pub type StorageResult<T> = Result<T, StorageError>;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("entry not found: {0}")]
    NotFound(Uuid),

    #[error("database error: {0}")]
    Database(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("initialization error: {0}")]
    Init(String),
}

#[async_trait]
pub trait MemoryStorage: Send + Sync {
    // ── Lifecycle ──────────────────────────────────────────────
    async fn initialize(&self) -> StorageResult<()>;
    async fn close(&self) -> StorageResult<()>;

    // ── Memory entries ─────────────────────────────────────────
    async fn save(&self, input: MemoryEntryInput, namespace: &str) -> StorageResult<SaveResult>;
    async fn get(&self, id: Uuid) -> StorageResult<MemoryEntry>;
    async fn update(&self, id: Uuid, updates: serde_json::Value) -> StorageResult<MemoryEntry>;
    async fn delete(&self, id: Uuid) -> StorageResult<()>;
    async fn list(&self, filters: ListFilters) -> StorageResult<Vec<MemoryEntry>>;

    // ── Embeddings ──────────────────────────────────────────────
    async fn save_embedding(&self, entry_id: Uuid, embedding: &[f32]) -> StorageResult<()>;

    // ── Search ─────────────────────────────────────────────────
    async fn search(&self, query: SearchQuery) -> StorageResult<Vec<MemoryEntry>>;
    async fn semantic_search(
        &self,
        query: &str,
        embedding: &[f32],
        limit: usize,
        namespace: &str,
    ) -> StorageResult<Vec<MemoryEntry>>;
    async fn fts_search(&self, query: SearchQuery) -> StorageResult<Vec<MemoryEntry>>;

    // ── Sessions ───────────────────────────────────────────────
    async fn create_session(&self, input: SessionInput, namespace: &str) -> StorageResult<Session>;
    async fn get_session(&self, id: Uuid) -> StorageResult<Session>;
    async fn update_session(&self, id: Uuid, update: SessionUpdate) -> StorageResult<Session>;
    async fn list_sessions(&self, namespace: &str, limit: usize) -> StorageResult<Vec<Session>>;

    // ── Edges ──────────────────────────────────────────────────
    async fn save_edge(&self, input: MemoryEdgeInput) -> StorageResult<MemoryEdge>;
    async fn get_edges(
        &self,
        entry_id: Uuid,
        direction: Option<&str>,
    ) -> StorageResult<Vec<MemoryEdge>>;
    async fn delete_edge(&self, id: Uuid) -> StorageResult<()>;

    // ── Debug ──────────────────────────────────────────────────
    async fn write_debug_log(&self, input: DebugLogInput) -> StorageResult<()>;
    async fn query_debug_log(&self, query: DebugLogQuery) -> StorageResult<Vec<DebugLogEntry>>;
    async fn prune_debug_log(&self, older_than_days: i64) -> StorageResult<i64>;

    // ── Stats ──────────────────────────────────────────────────
    async fn get_stats(&self, namespace: &str) -> StorageResult<MemoryStats>;

    // ── Admin ──────────────────────────────────────────────────
    /// Count live entries + sessions with `project_id = source` or
    /// `namespace = source`. Used by the merge tool's dry-run mode.
    async fn count_project_namespace(&self, source: &str) -> StorageResult<MergeCounts>;

    /// Merge all entries + sessions from `source` project namespace into
    /// `target`. Returns the row counts that were migrated.
    async fn merge_project_namespace(
        &self,
        source: &str,
        target: &str,
    ) -> StorageResult<MergeCounts>;

    // ── Pending Observations (auto-capture queue) ──────────────

    /// Append a new observation to the pending queue. Returns the new row id.
    async fn enqueue_observation(
        &self,
        obs: ObservationInput,
        namespace: &str,
    ) -> StorageResult<Uuid>;

    /// Atomically claim up to `max` pending observations in FIFO order,
    /// flipping them to `processing` so concurrent callers get disjoint sets.
    /// Filters by namespace; if `session_id` is Some, restricts to that session.
    async fn claim_observations(
        &self,
        namespace: &str,
        session_id: Option<Uuid>,
        max: usize,
    ) -> StorageResult<Vec<PendingObservation>>;

    /// Mark the given observations as `confirmed` with `confirmed_at = now()`.
    /// Called after the summarizer has successfully folded them into memory entries.
    async fn confirm_observations(&self, ids: &[Uuid]) -> StorageResult<()>;

    /// Flip observations that have been `processing` for longer than
    /// `older_than_secs` back to `pending` so another claimer can retry.
    /// Returns the count reverted.
    async fn recover_stale_observations(&self, older_than_secs: i64) -> StorageResult<i64>;

    /// Return true if any `pending_observations` row with this hash exists
    /// newer than `window_secs`. Used for 30-sec SHA256 dedup on enqueue.
    async fn check_observation_duplicate(
        &self,
        content_hash: &str,
        window_secs: i64,
    ) -> StorageResult<bool>;

    // ── Human-in-loop verification (A10) ──────────────────────

    /// Flip `verified=true` + set `verified_at=now()` on the given entry.
    /// `verified_by` records the dev who endorsed it (resolved by the
    /// caller — typically `identity::resolve_author()`); pass `None` to
    /// leave the column NULL. Returns the refreshed entry, or `NotFound`
    /// if the id doesn't exist.
    async fn mark_verified(
        &self,
        id: Uuid,
        verified_by: Option<&str>,
    ) -> StorageResult<MemoryEntry>;

    // ── Export/Import (A6) ────────────────────────────────────

    /// Import an entry with its original id. Returns true if the row was
    /// inserted, false if an entry with this id already existed (skipped).
    /// Preserves every field so edges + references survive a roundtrip.
    async fn import_entry(&self, entry: MemoryEntry) -> StorageResult<bool>;

    /// Soft-delete up to `max` ARCHIVAL entries in `namespace` that have
    /// `access_count = 0`, `confidence < conf_cap`, and are older than
    /// `age_days`. Verified entries are **never** evicted. Returns ids
    /// that were deleted so callers can log / audit.
    async fn evict_stale(
        &self,
        namespace: &str,
        age_days: i64,
        conf_cap: f32,
        max: usize,
    ) -> StorageResult<Vec<Uuid>>;

    /// List every edge across the DB up to `limit`. Used by `runar export`
    /// to dump the relationship graph alongside entries. Large DBs should
    /// paginate via export in chunks.
    async fn list_all_edges(&self, limit: usize) -> StorageResult<Vec<MemoryEdge>>;

    /// Import an edge with its original id. Skips on id conflict.
    async fn import_edge(&self, edge: MemoryEdge) -> StorageResult<bool>;

    /// Import a session with its original id. Skips on id conflict.
    async fn import_session(&self, session: Session) -> StorageResult<bool>;

    // ── Phase 5.6 — Hybrid sync (outbox + state + conflicts) ──────

    /// Append a row to `sync_outbox` for later push to remote. Idempotent
    /// per-call: each call inserts a new row. Coalescing by `entry_id`
    /// happens at push time.
    async fn enqueue_outbox(&self, op: OutboxInput) -> StorageResult<Uuid>;

    /// Atomically claim up to `max` pending outbox rows in FIFO order
    /// (`confirmed_at IS NULL`, `claimed_at IS NULL`), flipping
    /// `claimed_at = now()` so concurrent pushers get disjoint sets.
    async fn claim_outbox(&self, max: usize) -> StorageResult<Vec<OutboxRow>>;

    /// Mark outbox rows as confirmed (`confirmed_at = now()`).
    async fn confirm_outbox(&self, ids: &[Uuid]) -> StorageResult<()>;

    /// Record a transient failure on a single outbox row. Increments
    /// `attempts`, stores `last_error`, clears `claimed_at` so the row
    /// can be re-claimed.
    async fn fail_outbox(&self, id: Uuid, err: &str) -> StorageResult<()>;

    /// Count of pending (unconfirmed) outbox rows.
    async fn outbox_depth(&self) -> StorageResult<usize>;

    /// Delete confirmed outbox rows older than `older_than_secs`.
    /// Returns the number deleted. Pending rows are never touched.
    async fn gc_outbox(&self, older_than_secs: i64) -> StorageResult<i64>;

    /// Read the singleton sync state. Returns default (all None) if no
    /// row exists yet.
    async fn read_sync_state(&self) -> StorageResult<SyncState>;

    /// Upsert the singleton sync state row.
    async fn write_sync_state(&self, state: &SyncState) -> StorageResult<()>;

    /// Append an audit row to `sync_conflicts`. Fire-and-forget — caller
    /// should log but not fail on a record failure.
    async fn record_conflict(&self, conflict: &SyncConflict) -> StorageResult<()>;

    /// List recent conflicts ordered by `created_at DESC`. Used by
    /// `runar sync status`.
    async fn list_conflicts(&self, limit: usize) -> StorageResult<Vec<SyncConflict>>;

    /// Apply a remote-origin row to the local backend, running through the
    /// LWW + verified resolver.
    async fn apply_remote_entry(&self, entry: MemoryEntry) -> StorageResult<ApplyOutcome>;

    /// Phase 5.6.3 — list rows whose `updated_at` is strictly greater than
    /// `after` (or all rows when `after` is `None`), AND whose
    /// `updated_at` is at least `clock_skew_secs` older than `now()` (so
    /// inflight writes on the source don't get half-pulled). Includes
    /// soft-deleted rows so deletes propagate. Ordered by `updated_at`
    /// ASC, then `id` ASC for stable iteration. `project_filter` scopes
    /// to a single namespace when set.
    async fn list_changed_since(
        &self,
        after: Option<chrono::DateTime<chrono::Utc>>,
        clock_skew_secs: i64,
        limit: usize,
        project_filter: Option<&str>,
    ) -> StorageResult<Vec<MemoryEntry>>;
}
