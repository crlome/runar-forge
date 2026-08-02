use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Memory Entry Types ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryType {
    Decision,
    Pattern,
    Bug,
    Rule,
    BusinessRule,
    Architecture,
    TechDebt,
    Session,
    Context,
    Preference,
    #[default]
    Note,
    AutoChange,
    UserPrompt,
}

impl EntryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Pattern => "pattern",
            Self::Bug => "bug",
            Self::Rule => "rule",
            Self::BusinessRule => "business-rule",
            Self::Architecture => "architecture",
            Self::TechDebt => "tech-debt",
            Self::Session => "session",
            Self::Context => "context",
            Self::Preference => "preference",
            Self::Note => "note",
            Self::AutoChange => "auto-change",
            Self::UserPrompt => "user-prompt",
        }
    }
}

// ── Memory Source ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemorySource {
    Human,
    Agent,
    Scout,
    System,
}

// ── Memory Layer (1-4 decay system) ────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MemoryLayer(u8);

impl MemoryLayer {
    pub const WORKING: Self = Self(1);
    pub const EPISODIC: Self = Self(2);
    pub const SEMANTIC: Self = Self(3);
    pub const ARCHIVAL: Self = Self(4);

    pub fn value(&self) -> u8 {
        self.0
    }
}

impl From<u8> for MemoryLayer {
    fn from(v: u8) -> Self {
        Self(v.clamp(1, 4))
    }
}

// ── Session Status ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Completed,
    Abandoned,
}

// ── Edge Types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeType {
    Supports,
    Contradicts,
    Supersedes,
    Elaborates,
    Related,
}

// ── Debug Event Types ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugEvent {
    SearchScoring,
    DecayCompute,
    AutoLink,
    LayerGraduation,
    DedupDecision,
    TouchPromotion,
    HookTiming,
    /// A context packet was served (PreToolUse hook or muninn_context):
    /// which entries were injected, how many, how large, how long it took.
    Injection,
}

// ── Core Data Structures ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntry {
    pub id: Uuid,
    pub title: String,
    pub content: String,
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    pub source: MemorySource,
    pub tags: Vec<String>,
    pub namespace: String,
    pub project_id: Option<String>,
    pub topic_key: Option<String>,
    pub layer: MemoryLayer,
    pub importance: f64,
    pub decay_score: f64,
    pub access_count: i32,
    /// Times this entry was served into a model's context by automatic
    /// recall. Separate from `access_count` (ranked search) on purpose:
    /// one counter for two channels is what let "95.9% never retrieved"
    /// stand for three months while 15,819 injections went unrecorded.
    /// Reporting only — deliberately not an input to ranking or decay.
    #[serde(default)]
    pub injected_count: i32,
    #[serde(default)]
    pub last_injected_at: Option<DateTime<Utc>>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    pub embedding: Option<Vec<f32>>,
    /// Owner-endorsed flag (A10). Set via `muninn_verify` and grants a
    /// ranking bonus in fused search. Independent from confidence.
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub verified_at: Option<DateTime<Utc>>,
    /// Phase 5.7 — dev who proposed this entry. Resolved from
    /// `git config user.name` at save time. NULL for pre-attribution rows
    /// and agent-origin rows where no human was identified.
    #[serde(default)]
    pub author: Option<String>,
    /// Phase 5.7 — dev who ran `mark_verified` on this entry. NULL while
    /// `verified` is false; set independently of `author`.
    #[serde(default)]
    pub verified_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_accessed_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
}

pub const DEFAULT_CONFIDENCE: f32 = 0.9;

fn default_confidence() -> f32 {
    DEFAULT_CONFIDENCE
}

/// Resolve a `confidence` field (string preset or number) to a clamped f32.
/// Returns None when the value is absent. Unknown strings fall back to default.
pub fn resolve_confidence(value: Option<&serde_json::Value>) -> Option<f32> {
    let v = value?;
    if let Some(s) = v.as_str() {
        let resolved = match s {
            "verified" => 1.0,
            "observed" => 0.9,
            "inferred" => 0.7,
            "speculative" => 0.4,
            other => other.parse::<f32>().unwrap_or(DEFAULT_CONFIDENCE),
        };
        return Some(resolved.clamp(0.0, 1.0));
    }
    if let Some(n) = v.as_f64() {
        return Some((n as f32).clamp(0.0, 1.0));
    }
    None
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEntryInput {
    pub title: String,
    pub content: String,
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    #[serde(default)]
    pub source: Option<MemorySource>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub topic_key: Option<String>,
    #[serde(default)]
    pub importance: Option<f64>,
    #[serde(default)]
    pub confidence: Option<f32>,
    /// Phase 5.7 — explicit override of the resolved author. When None,
    /// the librarian falls back to `identity::resolve_author()`.
    #[serde(default)]
    pub author: Option<String>,
    /// Opt out of the content length bound because this entry's content is
    /// machine-readable and must round-trip byte for byte.
    ///
    /// Only the crawl state sets this today: it stores a JSON blob that
    /// `git::deserialize_state` parses back, and that is
    /// `serde_json::from_str(json).ok()` — so a truncated blob does not
    /// error, it returns `None`, which reads as "no previous state" and
    /// silently downgrades every incremental crawl to a full one. On a
    /// 2,082-file project the blob is 127,680 chars, 99.8% of it
    /// `file_hashes`.
    ///
    /// The exemption has a cost, and it is deliberate: an entry over the
    /// limit cannot satisfy the remote's own CHECK, so `propose` declines
    /// to enqueue it for sync rather than queueing a row that can only
    /// fail. See `MAX_CONTENT_CHARS`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exact_content: bool,
}

// ── Search ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchQuery {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub entry_type: Option<EntryType>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilters {
    #[serde(default)]
    pub entry_type: Option<EntryType>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    /// Include soft-deleted rows. Maintenance-only (scrub must be able to
    /// redact tombstones); every normal read leaves this false.
    #[serde(default)]
    pub include_deleted: bool,
}

// ── Sessions ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: Uuid,
    pub namespace: String,
    pub project_id: Option<String>,
    pub tool: Option<String>,
    pub goal: Option<String>,
    pub summary: Option<String>,
    #[serde(default)]
    pub discoveries: Vec<String>,
    #[serde(default)]
    pub files_modified: Vec<String>,
    pub status: SessionStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInput {
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub summary: String,
    /// What the user was trying to accomplish — short one-liner.
    #[serde(default)]
    pub goal: Option<String>,
    /// Explicit instructions or constraints the user gave, in order.
    #[serde(default)]
    pub instructions: Vec<String>,
    /// Concrete tasks the session finished (typically file paths + action).
    #[serde(default)]
    pub accomplished: Vec<String>,
    #[serde(default)]
    pub discoveries: Vec<String>,
    #[serde(default)]
    pub files_modified: Vec<String>,
}

impl SessionSummary {
    /// Render the structured fields plus the free-form `summary` into a
    /// markdown body suitable for storing as the session's memory entry.
    /// Omits sections that are empty so the resulting entry stays terse.
    pub fn render_markdown(&self) -> String {
        let mut lines = vec![format!("## Session Summary\n\n{}", self.summary.trim())];

        if let Some(goal) = self.goal.as_ref().filter(|g| !g.trim().is_empty()) {
            lines.push(format!("\n**Goal:** {}", goal.trim()));
        }

        let push_list = |lines: &mut Vec<String>, label: &str, items: &[String]| {
            let items: Vec<&String> = items.iter().filter(|s| !s.trim().is_empty()).collect();
            if items.is_empty() {
                return;
            }
            lines.push(format!("\n**{label}:**"));
            for item in items {
                lines.push(format!("- {}", item.trim()));
            }
        };

        push_list(&mut lines, "Instructions", &self.instructions);
        push_list(&mut lines, "Accomplished", &self.accomplished);
        push_list(&mut lines, "Discoveries", &self.discoveries);
        push_list(&mut lines, "Files modified", &self.files_modified);

        lines.join("\n")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdate {
    #[serde(default)]
    pub status: Option<SessionStatus>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub files_modified: Option<Vec<String>>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub discoveries: Option<Vec<String>>,
}

// ── Edges / Relationships ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEdge {
    pub id: Uuid,
    pub from_id: Uuid,
    pub to_id: Uuid,
    pub edge_type: EdgeType,
    pub strength: f64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEdgeInput {
    pub from_id: Uuid,
    pub to_id: Uuid,
    pub edge_type: EdgeType,
    #[serde(default = "default_edge_strength")]
    pub strength: f64,
}

fn default_edge_strength() -> f64 {
    0.8
}

// ── Debug Log ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugLogEntry {
    pub id: Uuid,
    pub event: DebugEvent,
    pub entry_id: Option<Uuid>,
    pub data: serde_json::Value,
    #[serde(default)]
    pub duration_ms: Option<f64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugLogInput {
    pub event: DebugEvent,
    #[serde(default)]
    pub entry_id: Option<Uuid>,
    pub data: serde_json::Value,
    #[serde(default)]
    pub duration_ms: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugLogQuery {
    #[serde(default)]
    pub event: Option<DebugEvent>,
    #[serde(default)]
    pub entry_id: Option<Uuid>,
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub limit: Option<usize>,
}

// ── Stats ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStats {
    pub total_entries: i64,
    pub total_sessions: i64,
    pub entries_by_type: Vec<(String, i64)>,
    pub entries_by_layer: Vec<(u8, i64)>,
    pub namespaces: Vec<String>,
}

/// Per-namespace slice of the cross-namespace aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceStats {
    pub namespace: String,
    pub entries: i64,
    pub sessions: i64,
}

/// Whole-database stats: totals across every namespace, plus the
/// per-namespace breakdown. `muninn_stats` without arguments returns this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalStats {
    pub total_entries: i64,
    pub total_sessions: i64,
    pub entries_by_type: Vec<(String, i64)>,
    pub entries_by_layer: Vec<(u8, i64)>,
    pub by_namespace: Vec<NamespaceStats>,
}

// ── Save Result ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub id: Uuid,
    pub action: SaveAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded: Option<SupersededEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupersededEntry {
    pub id: Uuid,
    pub title: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SaveAction {
    Created,
    Updated,
    Rejected,
    /// An identical live entry (same namespace + content hash) already
    /// exists; nothing was inserted. `SaveResult.id` is the existing row.
    Duplicate,
}

// ── Merge Counts ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeCounts {
    pub entries: i64,
    pub sessions: i64,
}

/// One member of a duplicate-content cluster (see `find_duplicate_clusters`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DupMember {
    pub id: Uuid,
    pub namespace: String,
    pub title: String,
    pub access_count: i64,
    pub verified: bool,
    pub created_at: DateTime<Utc>,
}

/// Live rows sharing `(namespace, content_hash)`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateCluster {
    pub content_hash: String,
    pub entries: Vec<DupMember>,
}

// ── Decay Configuration ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DecayConfig {
    pub lambda: f64,
    pub archival_base_weight: f64,
    pub access_boost_rate: f64,
    pub max_access_boost: f64,
    pub graduation_thresholds: GraduationThresholds,
    /// Phase 5.4 — access count that triggers Hebbian tier promotion
    /// (bump one layer when reached, as long as entry is not yet SEMANTIC).
    pub citation_threshold: i32,
    /// Phase 5.4 — confidence below this floor marks an entry as low-quality;
    /// combined with age > 2× graduation threshold, it fast-tracks to ARCHIVAL.
    pub low_confidence_threshold: f32,
    /// Phase 5.4 — age in ARCHIVAL past which unverified zero-access entries
    /// are soft-deleted (`evict_stale`).
    pub eviction_age_days: i64,
    /// Phase 5.4 — maximum rows soft-deleted per `evict_stale` call. Cap
    /// exists to prevent surprise mass deletion during the first run on
    /// long-lived DBs.
    pub eviction_max_per_run: usize,
}

#[derive(Debug, Clone)]
pub struct GraduationThresholds {
    pub working_to_episodic_days: i64,
    pub episodic_to_semantic_days: i64,
    pub semantic_to_archival_days: i64,
}

impl DecayConfig {
    /// Build a config from defaults, overridden by any `RUNAR_TIER_*` env vars
    /// that are present. Unknown or unparseable values fall back to the
    /// default so a typo never breaks startup.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        fn env_i64(key: &str) -> Option<i64> {
            std::env::var(key).ok().and_then(|v| v.parse().ok())
        }
        fn env_i32(key: &str) -> Option<i32> {
            std::env::var(key).ok().and_then(|v| v.parse().ok())
        }
        fn env_usize(key: &str) -> Option<usize> {
            std::env::var(key).ok().and_then(|v| v.parse().ok())
        }
        fn env_f32(key: &str) -> Option<f32> {
            std::env::var(key).ok().and_then(|v| v.parse().ok())
        }

        if let Some(v) = env_i64("RUNAR_TIER_WORKING_DAYS") {
            cfg.graduation_thresholds.working_to_episodic_days = v;
        }
        if let Some(v) = env_i64("RUNAR_TIER_EPISODIC_DAYS") {
            cfg.graduation_thresholds.episodic_to_semantic_days = v;
        }
        if let Some(v) = env_i64("RUNAR_TIER_SEMANTIC_DAYS") {
            cfg.graduation_thresholds.semantic_to_archival_days = v;
        }
        if let Some(v) = env_i32("RUNAR_TIER_CITATION_THRESHOLD") {
            cfg.citation_threshold = v.max(1);
        }
        if let Some(v) = env_f32("RUNAR_TIER_LOW_CONFIDENCE") {
            cfg.low_confidence_threshold = v.clamp(0.0, 1.0);
        }
        if let Some(v) = env_i64("RUNAR_TIER_EVICTION_AGE_DAYS") {
            cfg.eviction_age_days = v.max(1);
        }
        if let Some(v) = env_usize("RUNAR_TIER_EVICTION_MAX_PER_RUN") {
            cfg.eviction_max_per_run = v;
        }

        cfg
    }
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            lambda: 0.05,
            archival_base_weight: 0.5,
            access_boost_rate: 0.05,
            max_access_boost: 0.5,
            graduation_thresholds: GraduationThresholds {
                working_to_episodic_days: 7,
                episodic_to_semantic_days: 14,
                semantic_to_archival_days: 30,
            },
            citation_threshold: 5,
            low_confidence_threshold: 0.5,
            eviction_age_days: 90,
            eviction_max_per_run: 100,
        }
    }
}

// ── Pending Observations (Phase 6 auto-capture queue) ─────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PendingStatus {
    Pending,
    Processing,
    Confirmed,
}

impl PendingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Confirmed => "confirmed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationInput {
    pub session_id: Option<Uuid>,
    pub project_id: Option<String>,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_response: serde_json::Value,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingObservation {
    pub id: Uuid,
    pub namespace: String,
    pub session_id: Option<Uuid>,
    pub project_id: Option<String>,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_response: serde_json::Value,
    pub content_hash: String,
    pub status: PendingStatus,
    pub attempt_count: i32,
    pub claimed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub confirmed_at: Option<DateTime<Utc>>,
}

// ── Phase 5.6 — Hybrid sync types ──────────────────────────────────

/// Kind of mutation captured in `sync_outbox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboxOp {
    Insert,
    Update,
    Delete,
}

impl OutboxOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "insert" => Some(Self::Insert),
            "update" => Some(Self::Update),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// Input for `enqueue_outbox`. The payload is a full row snapshot;
/// later edits to the same `entry_id` enqueue separate rows but the
/// reconciler coalesces by latest `created_at` per `entry_id` at
/// push time (5.6.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxInput {
    pub entry_id: Uuid,
    pub op_kind: OutboxOp,
    pub row_payload: serde_json::Value,
}

/// Persisted outbox row read back by the reconciler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxRow {
    pub id: Uuid,
    pub entry_id: Uuid,
    pub op_kind: OutboxOp,
    pub row_payload: serde_json::Value,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Breakdown of unconfirmed `sync_outbox` rows by state.
///
/// `outbox_depth` returns only the sum, which reads as a healthy backlog
/// whether the rows are waiting to be claimed or wedged and unclaimable.
/// The distinction is the whole diagnosis, so report the parts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxHealth {
    /// Claimable now: unconfirmed, unclaimed, under the attempt cap.
    pub pending: u64,
    /// Claimed by some pusher and not yet confirmed or failed. Non-zero
    /// outside an active push means a pusher died mid-run; these are
    /// what `reap_stale_claims` releases.
    pub in_flight: u64,
    /// At or beyond the attempt cap. Never claimed again; needs a human.
    pub dead_lettered: u64,
    /// Age of the oldest unconfirmed row, whatever its state.
    pub oldest_unconfirmed: Option<DateTime<Utc>>,
    /// Highest `attempts` across unconfirmed rows.
    pub max_attempts_seen: i32,
}

impl OutboxHealth {
    /// Total unconfirmed rows — matches `outbox_depth`.
    pub fn total(&self) -> u64 {
        self.pending + self.in_flight + self.dead_lettered
    }

    /// True when rows exist but none can be claimed, i.e. pushing will
    /// report "nothing to push" while the backlog never drains.
    pub fn is_wedged(&self) -> bool {
        self.pending == 0 && (self.in_flight > 0 || self.dead_lettered > 0)
    }
}

/// An outbox row the remote can never accept, because its payload's
/// `content` exceeds the remote's own `CHECK (char_length(content) <= N)`.
///
/// Tested against the **payload**, not the entry's current content: the
/// payload is what gets pushed, and it is a snapshot. An entry rewritten or
/// deleted since the row was queued still carries its original oversized
/// body in the queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsendableRow {
    pub outbox_id: Uuid,
    pub entry_id: Uuid,
    pub op_kind: OutboxOp,
    /// Characters in the payload's `content`, for reporting.
    pub content_chars: usize,
}

/// Singleton state row tracking the pull cursor + handshake metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub last_pulled_updated_at: Option<DateTime<Utc>>,
    pub last_pulled_session_at: Option<DateTime<Utc>>,
    pub last_pulled_edge_at: Option<DateTime<Utc>>,
    pub last_push_at: Option<DateTime<Utc>>,
    pub last_pull_at: Option<DateTime<Utc>>,
    pub local_dim: Option<i32>,
    pub remote_dim: Option<i32>,
    pub local_schema_version: Option<String>,
    pub remote_schema_version: Option<String>,
    pub initialized_at: Option<DateTime<Utc>>,
}

/// Resolver decision audited in `sync_conflicts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    /// Picked by `updated_at` comparison.
    Lww,
    /// Verified-side won regardless of timestamp.
    VerifiedWins,
    /// Soft-delete propagated over a non-deleted update.
    SoftDeleteWins,
    /// Incoming non-deleted row blocked by an existing soft-delete
    /// (resurrection requires manual verify).
    ResurrectBlocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictDirection {
    Push,
    Pull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictWinner {
    Local,
    Remote,
}

/// Audit row written by the resolver. Fire-and-forget — failure to
/// record never fails the sync itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConflict {
    pub id: Uuid,
    pub entry_id: Uuid,
    pub direction: ConflictDirection,
    pub policy: ConflictPolicy,
    pub winner_side: ConflictWinner,
    pub local_updated_at: Option<DateTime<Utc>>,
    pub remote_updated_at: Option<DateTime<Utc>>,
    pub local_payload: Option<serde_json::Value>,
    pub remote_payload: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// Outcome of `apply_remote_entry` — what happened on the local side
/// after the resolver inspected an inbound remote row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplyOutcome {
    /// Row didn't exist locally; inserted as-is.
    Inserted,
    /// Local row updated because remote was newer per LWW rules.
    UpdatedLww,
    /// Local row left unchanged because it was newer / verified.
    SkippedNewerLocal,
    /// Resolver flagged this and called `record_conflict` —
    /// caller may inspect the audit table.
    ConflictRecorded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_summary_render_full() {
        let s = SessionSummary {
            summary: "Wired up auth retry.".into(),
            goal: Some("Fix flaky login".into()),
            instructions: vec!["no breaking changes".into(), "keep latency < 200ms".into()],
            accomplished: vec!["added 3-retry backoff".into()],
            discoveries: vec!["middleware was caching tokens past expiry".into()],
            files_modified: vec!["src/auth.rs".into()],
        };
        let md = s.render_markdown();
        assert!(md.contains("## Session Summary"));
        assert!(md.contains("Wired up auth retry."));
        assert!(md.contains("**Goal:** Fix flaky login"));
        assert!(md.contains("**Instructions:**"));
        assert!(md.contains("- no breaking changes"));
        assert!(md.contains("**Accomplished:**"));
        assert!(md.contains("- added 3-retry backoff"));
        assert!(md.contains("**Discoveries:**"));
        assert!(md.contains("**Files modified:**"));
        assert!(md.contains("- src/auth.rs"));
    }

    #[test]
    fn session_summary_render_skips_empty_sections() {
        let s = SessionSummary {
            summary: "Minimal session.".into(),
            goal: None,
            instructions: vec![],
            accomplished: vec![],
            discoveries: vec![],
            files_modified: vec![],
        };
        let md = s.render_markdown();
        assert!(md.contains("Minimal session."));
        assert!(!md.contains("**Goal:**"));
        assert!(!md.contains("**Instructions:**"));
        assert!(!md.contains("**Accomplished:**"));
    }

    #[test]
    fn resolve_confidence_presets() {
        let cases = [
            ("verified", 1.0_f32),
            ("observed", 0.9),
            ("inferred", 0.7),
            ("speculative", 0.4),
        ];
        for (preset, expected) in cases {
            let v = serde_json::Value::String(preset.into());
            assert_eq!(resolve_confidence(Some(&v)), Some(expected));
        }
    }

    #[test]
    fn resolve_confidence_numeric_clamped() {
        let hi = serde_json::Value::from(1.9);
        assert_eq!(resolve_confidence(Some(&hi)), Some(1.0));

        let lo = serde_json::Value::from(-0.4);
        assert_eq!(resolve_confidence(Some(&lo)), Some(0.0));

        let mid = serde_json::Value::from(0.65);
        assert_eq!(resolve_confidence(Some(&mid)).unwrap(), 0.65_f32);
    }

    #[test]
    fn resolve_confidence_string_number_accepted() {
        let v = serde_json::Value::String("0.65".into());
        assert_eq!(resolve_confidence(Some(&v)).unwrap(), 0.65_f32);
    }

    #[test]
    fn resolve_confidence_absent() {
        assert_eq!(resolve_confidence(None), None);
    }

    #[test]
    fn resolve_confidence_unknown_string_falls_back() {
        let v = serde_json::Value::String("garbage".into());
        assert_eq!(resolve_confidence(Some(&v)).unwrap(), DEFAULT_CONFIDENCE);
    }
}
