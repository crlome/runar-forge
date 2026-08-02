pub mod postgres;
pub mod sqlite;

use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    ApplyOutcome, DebugLogEntry, DebugLogInput, DebugLogQuery, DuplicateCluster, GlobalStats,
    ListFilters, MemoryEdge, MemoryEdgeInput, MemoryEntry, MemoryEntryInput, MemoryStats,
    MergeCounts, ObservationInput, OutboxHealth, OutboxInput, OutboxRow, PendingObservation,
    SaveResult, SearchQuery, Session, SessionInput, SessionUpdate, SyncConflict, SyncState,
    UnsendableRow,
};

pub type StorageResult<T> = Result<T, StorageError>;

/// SHA-256 hex over normalized title+content — the exact-duplicate guard
/// used by `save`. Normalization is deliberately conservative (lowercase,
/// trim, collapse whitespace runs) so only true restatements collapse.
pub fn content_hash(title: &str, content: &str) -> String {
    use sha2::{Digest, Sha256};
    fn normalize(s: &str) -> String {
        s.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
    let mut hasher = Sha256::new();
    hasher.update(normalize(title).as_bytes());
    hasher.update(b"\n");
    hasher.update(normalize(content).as_bytes());
    format!("{:x}", hasher.finalize())
}

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

    /// Same as `get`, but sees soft-deleted rows too.
    ///
    /// Needed to build a sync tombstone: a `delete` outbox payload is a
    /// full entry snapshot carrying `deleted_at`, and `get` filters
    /// exactly the rows a tombstone is about. The remote's LWW resolver
    /// applies the deletion from that snapshot like any other update.
    async fn get_including_deleted(&self, id: Uuid) -> StorageResult<MemoryEntry>;

    /// Fetch the live entry for a (namespace, topic_key) pair, if any.
    async fn get_by_topic_key(
        &self,
        namespace: &str,
        topic_key: &str,
    ) -> StorageResult<Option<MemoryEntry>>;

    async fn update(&self, id: Uuid, updates: serde_json::Value) -> StorageResult<MemoryEntry>;

    /// Record retrieval of the given entries in one atomic statement:
    /// server-side `access_count + 1`, stamp `last_accessed_at`, and promote
    /// colder-than-EPISODIC layers to EPISODIC. Returns rows touched.
    /// Replaces the old per-entry get-then-update (racy) fire-and-forget.
    async fn touch_entries(&self, ids: &[Uuid]) -> StorageResult<i64>;

    /// Record that the given entries were served into a model's context.
    ///
    /// Deliberately separate from `touch_entries`: injection and ranked
    /// search are different channels with different volumes, and folding
    /// them into one counter is what made "95.9% never retrieved" a
    /// three-month-old misreading. These counters feed reporting only —
    /// not decay, citation thresholds, or dedup keeper selection.
    async fn mark_injected(&self, ids: &[Uuid]) -> StorageResult<i64>;

    async fn delete(&self, id: Uuid) -> StorageResult<()>;
    async fn list(&self, filters: ListFilters) -> StorageResult<Vec<MemoryEntry>>;

    // ── Embeddings ──────────────────────────────────────────────
    async fn save_embedding(&self, entry_id: Uuid, embedding: &[f32]) -> StorageResult<()>;

    // ── Search ─────────────────────────────────────────────────
    async fn search(&self, query: SearchQuery) -> StorageResult<Vec<MemoryEntry>>;
    /// Cosine-similarity search. `filters.query` is unused; the struct
    /// carries namespace/limit/entry_type/project_id so both search arms
    /// apply identical predicates.
    async fn semantic_search(
        &self,
        embedding: &[f32],
        filters: SearchQuery,
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

    /// Cross-namespace aggregate: unfiltered totals plus a per-namespace
    /// entry/session breakdown. The single-namespace `get_stats` cannot see
    /// per-project data (namespace == project_id by write invariant).
    async fn get_stats_all(&self) -> StorageResult<GlobalStats>;

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
    /// newer than `window_secs`. Used for the 30-sec enqueue dedup window.
    /// (The hash here is `extract::short_hash_public` — a truncated
    /// non-cryptographic hash, distinct from `content_hash()` used on
    /// `memory_entries`.)
    async fn check_observation_duplicate(
        &self,
        content_hash: &str,
        window_secs: i64,
    ) -> StorageResult<bool>;

    // ── Content-hash maintenance (dedup backfill/cleanup) ──────

    /// Rows still lacking `content_hash`: `(id, title, content)`, batched.
    /// Covers live AND soft-deleted rows so cluster detection is complete.
    async fn list_missing_content_hash(
        &self,
        limit: usize,
    ) -> StorageResult<Vec<(Uuid, String, String)>>;

    /// Set `content_hash` directly, without touching `updated_at`.
    async fn set_content_hash(&self, id: Uuid, hash: &str) -> StorageResult<()>;

    /// Live rows sharing a `(namespace, content_hash)` value, grouped into
    /// clusters of 2+ members. `namespace = None` scans all namespaces.
    async fn find_duplicate_clusters(
        &self,
        namespace: Option<&str>,
    ) -> StorageResult<Vec<DuplicateCluster>>;

    /// Maintenance-only (`runar scrub`): overwrite title/content/tags and
    /// refresh `content_hash` regardless of `deleted_at`, so credential
    /// tombstones can be redacted too. The guarded FTS triggers keep the
    /// index correct for both live and deleted rows.
    async fn redact_entry_row(
        &self,
        id: Uuid,
        title: &str,
        content: &str,
        tags: &[String],
    ) -> StorageResult<()>;

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

    // ── Two-stage GC ───────────────────────────────────────────

    /// GC stage 1: soft-delete never-accessed crawl-origin entries older
    /// than `age_days`. Crawl-origin = `source = 'scout'`; Human/System/
    /// Agent and verified rows are excluded by construction. `dry_run`
    /// returns the candidate ids without mutating.
    async fn soft_delete_stale_crawl(
        &self,
        namespace: &str,
        age_days: i64,
        max: usize,
        dry_run: bool,
    ) -> StorageResult<Vec<Uuid>>;

    /// GC stage 2: hard-purge rows soft-deleted longer than
    /// `older_than_days` ago. `namespace = None` purges all namespaces.
    /// FK CASCADE removes embeddings + edges; the guarded FTS triggers keep
    /// the index consistent. Irreversible — callers gate on explicit
    /// confirmation. `dry_run` returns candidates without mutating.
    async fn purge_soft_deleted(
        &self,
        namespace: Option<&str>,
        older_than_days: i64,
        max: usize,
        dry_run: bool,
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
    ///
    /// Rows at or beyond `max_attempts` are dead-lettered: they stay in
    /// the table but are never claimed again, so one poisoned payload
    /// cannot starve the queue behind it.
    async fn claim_outbox(&self, max: usize, max_attempts: i32) -> StorageResult<Vec<OutboxRow>>;

    /// Mark outbox rows as confirmed (`confirmed_at = now()`).
    async fn confirm_outbox(&self, ids: &[Uuid]) -> StorageResult<()>;

    /// Record a transient failure on a single outbox row. Increments
    /// `attempts`, stores `last_error`, clears `claimed_at` so the row
    /// can be re-claimed.
    async fn fail_outbox(&self, id: Uuid, err: &str) -> StorageResult<()>;

    /// Release claims held longer than `older_than_secs` by returning
    /// `claimed_at` to NULL. A pusher that dies between claim and
    /// confirm would otherwise strand its rows forever, since
    /// `claim_outbox` only ever sees `claimed_at IS NULL`. Returns the
    /// number of rows released. Confirmed rows are never touched.
    async fn reap_stale_claims(&self, older_than_secs: i64) -> StorageResult<u64>;

    /// Release a specific set of claims immediately, without waiting for
    /// the staleness cutoff. Used by `--dry-run`, which claims rows it
    /// never intends to push.
    async fn release_outbox(&self, ids: &[Uuid]) -> StorageResult<()>;

    /// Ids of soft-deleted entries that have at least one outbox row but
    /// no `delete` op — i.e. the remote has been told (or is queued to be
    /// told) the entry exists, and will never be told it went away.
    ///
    /// Repairs history written while `deprecate` silently dropped its
    /// delete ops. Ordered oldest-deletion-first so a capped run makes
    /// deterministic progress.
    async fn deleted_entries_without_tombstone(&self, limit: usize) -> StorageResult<Vec<Uuid>>;

    /// Unconfirmed `delete` rows whose payload is not a full entry, and
    /// so can never be pushed — `push_one` deserializes into
    /// `MemoryEntry`. Returns `(outbox_id, entry_id)` pairs.
    ///
    /// Detected structurally (missing `title`) rather than by reading
    /// `last_error`, so rows that have not been attempted yet are found
    /// too.
    async fn malformed_delete_payloads(&self, limit: usize) -> StorageResult<Vec<(Uuid, Uuid)>>;

    /// Replace an outbox row's payload and clear its failure state so it
    /// re-enters the queue with a clean slate.
    async fn rewrite_outbox_payload(
        &self,
        outbox_id: Uuid,
        payload: &serde_json::Value,
    ) -> StorageResult<()>;

    /// Unconfirmed outbox rows whose payload `content` exceeds
    /// `max_content_chars` — the remote's CHECK rejects these on every
    /// attempt, so they occupy the queue forever without ever draining.
    async fn unsendable_outbox_rows(
        &self,
        max_content_chars: usize,
        limit: usize,
    ) -> StorageResult<Vec<UnsendableRow>>;

    /// Hard-delete outbox rows by id. Returns the number removed.
    ///
    /// Distinct from `gc_outbox`, which only ever removes rows that were
    /// successfully confirmed. This removes rows that never can be, so the
    /// caller must have established that. Memory entries are untouched.
    async fn delete_outbox_rows(&self, ids: &[Uuid]) -> StorageResult<u64>;

    /// Count of pending (unconfirmed) outbox rows.
    async fn outbox_depth(&self) -> StorageResult<usize>;

    /// Breakdown of unconfirmed outbox rows by state. `outbox_depth`
    /// sums these and so cannot distinguish a queue that is merely deep
    /// from one that is wedged.
    async fn outbox_health(&self, max_attempts: i32) -> StorageResult<OutboxHealth>;

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
