use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::embedding::EmbeddingProvider;
use crate::storage::{MemoryStorage, StorageResult};
use crate::types::*;

pub struct MemoryLibrarian {
    storage: Arc<dyn MemoryStorage>,
    embedding: Arc<dyn EmbeddingProvider>,
    default_namespace: String,
    decay_config: DecayConfig,
}

impl MemoryLibrarian {
    pub fn new(
        storage: Arc<dyn MemoryStorage>,
        embedding: Arc<dyn EmbeddingProvider>,
        default_namespace: &str,
        decay_config: Option<DecayConfig>,
    ) -> Self {
        Self {
            storage,
            embedding,
            default_namespace: default_namespace.to_string(),
            decay_config: decay_config.unwrap_or_default(),
        }
    }

    fn ns<'a>(&'a self, namespace: Option<&'a str>) -> &'a str {
        namespace.unwrap_or(&self.default_namespace)
    }

    // ── Write ──────────────────────────────────────────────────

    pub async fn propose(&self, input: MemoryEntryInput) -> StorageResult<SaveResult> {
        let namespace = input
            .project_id
            .clone()
            .unwrap_or_else(|| self.default_namespace.clone());

        // A3 — strip any `<private>…</private>` blocks from title/content
        // before the row hits storage. Best-effort privacy hygiene; not a
        // security boundary.
        let mut input = input;
        let (title_clean, title_redactions) = crate::redact::strip_private(&input.title);
        let (content_clean, content_redactions) = crate::redact::strip_private(&input.content);
        let total_redactions = title_redactions + content_redactions;
        input.title = title_clean;
        input.content = content_clean;
        if total_redactions > 0 && !input.tags.iter().any(|t| t == "redacted") {
            input.tags.push("redacted".to_string());
        }

        // Phase 5.7 — stamp author from `git config user.name` when the
        // caller did not pass an explicit value. Stays None if git isn't
        // configured; the storage column then remains NULL.
        if input.author.is_none() {
            input.author = crate::identity::resolve_author();
        }

        let embed_text = format!("{} {}", input.title, input.content);
        let result = self.storage.save(input, &namespace).await?;

        if matches!(result.action, SaveAction::Created | SaveAction::Updated) {
            let entry_id = result.id;

            // Generate and save embedding synchronously (before returning)
            if self.embedding.is_available() {
                if let Some(emb) = self.embedding.embed(&embed_text).await {
                    let _ = self.storage.save_embedding(entry_id, &emb).await;
                }
            }

            // Phase 5.1.2 — link new → old via `supersedes` edge so the
            // provenance graph retains the history we just soft-deleted.
            if let Some(ref old) = result.superseded {
                let _ = self
                    .storage
                    .save_edge(MemoryEdgeInput {
                        from_id: entry_id,
                        to_id: old.id,
                        edge_type: EdgeType::Supersedes,
                        strength: 1.0,
                    })
                    .await;
            }

            // Phase 5.6.2 — outbox enqueue. Hybrid mode only; otherwise
            // no-op. Re-fetch the freshly saved row so the payload
            // reflects the post-save state (including supersession).
            self.enqueue_outbox_for_entry(entry_id, OutboxOp::Insert).await;

            // Auto-link is fire-and-forget (less critical)
            let storage = self.storage.clone();
            let embedding = self.embedding.clone();
            let ns = namespace.clone();
            tokio::spawn(async move {
                let _ = auto_link_entry(&*storage, &*embedding, entry_id, &ns).await;
            });
        }

        Ok(result)
    }

    pub async fn deprecate(&self, id: Uuid) -> StorageResult<()> {
        self.storage.delete(id).await?;
        // Phase 5.6.2 — soft-delete needs to propagate to remote.
        self.enqueue_outbox_for_entry(id, OutboxOp::Delete).await;
        Ok(())
    }

    /// Phase 5.6.2 — append an outbox row for the given entry id.
    /// Best-effort: a sync enqueue failure must NOT fail the underlying
    /// write. Skipped entirely when not in hybrid mode (no
    /// `RUNAR_STORAGE_LOCAL` set).
    async fn enqueue_outbox_for_entry(&self, entry_id: Uuid, op_kind: OutboxOp) {
        if std::env::var("RUNAR_STORAGE_LOCAL").is_err() {
            return;
        }
        let entry = match self.storage.get(entry_id).await {
            Ok(e) => e,
            Err(_) => return, // can't outbox what we can't read
        };
        let payload = match serde_json::to_value(&entry) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "outbox payload serialization failed");
                return;
            }
        };
        if let Err(e) = self
            .storage
            .enqueue_outbox(OutboxInput {
                entry_id,
                op_kind,
                row_payload: payload,
            })
            .await
        {
            tracing::warn!(error = %e, "outbox enqueue failed");
        }
    }

    // ── Sessions ───────────────────────────────────────────────

    pub async fn propose_session(&self, input: SessionInput) -> StorageResult<Session> {
        let ns = self.ns(input.project_id.as_deref()).to_string();
        self.storage.create_session(input, &ns).await
    }

    pub async fn end_session(
        &self,
        id: Uuid,
        summary: SessionSummary,
        project_id: Option<&str>,
    ) -> StorageResult<Session> {
        self.end_session_inner(id, summary, project_id, false).await
    }

    /// Save a summary memory entry without marking the session Completed.
    /// Used by the Phase 5.2.2 checkpoint mode of `muninn_session_end`.
    pub async fn checkpoint_session(
        &self,
        id: Uuid,
        summary: SessionSummary,
        project_id: Option<&str>,
    ) -> StorageResult<Session> {
        self.end_session_inner(id, summary, project_id, true).await
    }

    async fn end_session_inner(
        &self,
        id: Uuid,
        summary: SessionSummary,
        project_id: Option<&str>,
        checkpoint: bool,
    ) -> StorageResult<Session> {
        let update = if checkpoint {
            SessionUpdate {
                status: None,
                summary: Some(summary.summary.clone()),
                ended_at: None,
                files_modified: Some(summary.files_modified.clone()),
            }
        } else {
            SessionUpdate {
                status: Some(SessionStatus::Completed),
                summary: Some(summary.summary.clone()),
                ended_at: Some(Utc::now()),
                files_modified: Some(summary.files_modified.clone()),
            }
        };

        let session = self.storage.update_session(id, update).await?;

        // Create session summary entry — A7 rich structured format.
        let content = summary.render_markdown();

        let pid = project_id.unwrap_or("unknown");
        let tag = if checkpoint {
            "session-checkpoint"
        } else {
            "session-end"
        };
        let _ = self
            .storage
            .save(
                MemoryEntryInput {
                    title: format!("Session summary — {pid}"),
                    content,
                    entry_type: EntryType::Session,
                    source: Some(MemorySource::System),
                    tags: vec!["session-summary".into(), tag.into(), pid.into()],
                    project_id: project_id.map(|s| s.to_string()),
                    ..Default::default()
                },
                self.ns(project_id),
            )
            .await;

        Ok(session)
    }

    pub async fn get_active_session(
        &self,
        namespace: Option<&str>,
    ) -> StorageResult<Option<Session>> {
        let sessions = self.storage.list_sessions(self.ns(namespace), 10).await?;
        Ok(sessions
            .into_iter()
            .find(|s| s.status == SessionStatus::Active))
    }

    pub async fn update_session(
        &self,
        id: uuid::Uuid,
        update: crate::types::SessionUpdate,
    ) -> StorageResult<Session> {
        self.storage.update_session(id, update).await
    }

    // ── Search ─────────────────────────────────────────────────

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        namespace: Option<&str>,
        project_id: Option<&str>,
        entry_type: Option<EntryType>,
        tags: Option<Vec<String>>,
    ) -> StorageResult<Vec<MemoryEntry>> {
        let requested_limit = limit.min(50);

        self.fused_search(
            query,
            requested_limit,
            namespace,
            project_id,
            entry_type,
            tags,
        )
        .await
    }

    pub async fn fused_search(
        &self,
        query: &str,
        limit: usize,
        namespace: Option<&str>,
        project_id: Option<&str>,
        entry_type: Option<EntryType>,
        tags: Option<Vec<String>>,
    ) -> StorageResult<Vec<MemoryEntry>> {
        let ns = self.ns(namespace);
        let over_fetch = (limit * 3).min(50);
        let k: f64 = 60.0;

        let search_query = SearchQuery {
            query: query.trim().to_string(),
            limit: Some(over_fetch),
            entry_type,
            project_id: project_id.map(|s| s.to_string()),
            tags,
            namespace: Some(ns.to_string()),
        };

        // Run semantic + FTS in parallel
        let (semantic_result, fts_result) = tokio::join!(
            self.run_semantic_search(query, over_fetch, ns),
            self.storage.fts_search(search_query),
        );

        let mut semantic_ranks: HashMap<Uuid, usize> = HashMap::new();
        let mut fts_ranks: HashMap<Uuid, usize> = HashMap::new();
        let mut entry_map: HashMap<Uuid, MemoryEntry> = HashMap::new();

        if let Ok(ref entries) = semantic_result {
            for (i, entry) in entries.iter().enumerate() {
                semantic_ranks.insert(entry.id, i);
                entry_map.insert(entry.id, entry.clone());
            }
        }

        if let Ok(ref entries) = fts_result {
            for (i, entry) in entries.iter().enumerate() {
                fts_ranks.insert(entry.id, i);
                entry_map.entry(entry.id).or_insert_with(|| entry.clone());
            }
        }

        // Compute RRF scores
        let now = Utc::now();
        let mut scored: Vec<(f64, MemoryEntry)> = Vec::new();

        for (id, entry) in &entry_map {
            let mut rrf_score = 0.0;

            if let Some(&rank) = semantic_ranks.get(id) {
                rrf_score += 1.0 / (k + rank as f64);
            }

            if let Some(&rank) = fts_ranks.get(id) {
                rrf_score += 1.0 / (k + rank as f64);
            }

            // Decay signal
            let decay = self.compute_decay_score(entry, &now);
            rrf_score += decay * (1.0 / k);

            // Recency boost
            let days_since = (now - entry.created_at).num_seconds().max(0) as f64 / 86400.0;
            let recency = 1.0_f64.min((-0.05 * days_since).exp());
            rrf_score += recency * (0.5 / k);

            // Source-confidence multiplier (Phase 5.1 item 5.1.1): down-weight
            // speculative/inferred entries so a verified fact outranks them at
            // similar relevance.
            rrf_score *= entry.confidence.clamp(0.0, 1.0) as f64;

            // A10 — owner-endorsed entries get a 1.25× rank bonus. Multiplier
            // is conservative on purpose: humans should tie-break close calls,
            // not overwhelm semantic match.
            if entry.verified {
                rrf_score *= 1.25;
            }

            // Phase 5.4 — SEMANTIC entries that have proven useful via
            // repeated retrieval earn a modest 1.1× bonus. Existing ARCHIVAL
            // penalty lives in `compute_decay_score` via base_weight=0.5,
            // so we don't double-count here.
            if entry.layer == MemoryLayer::SEMANTIC
                && entry.access_count >= self.decay_config.citation_threshold
            {
                rrf_score *= 1.1;
            }

            scored.push((rrf_score, entry.clone()));
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        // Auto-touch returned entries (fire and forget)
        for (_, entry) in &scored {
            let storage = self.storage.clone();
            let entry_id = entry.id;
            tokio::spawn(async move {
                let _ = touch_entry(&*storage, entry_id).await;
            });
        }

        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }

    async fn run_semantic_search(
        &self,
        query: &str,
        limit: usize,
        namespace: &str,
    ) -> StorageResult<Vec<MemoryEntry>> {
        if !self.embedding.is_available() {
            return Ok(vec![]);
        }
        match self.embedding.embed(query).await {
            Some(embedding) => {
                self.storage
                    .semantic_search(query, &embedding, limit, namespace)
                    .await
            }
            None => Ok(vec![]),
        }
    }

    // ── Read ───────────────────────────────────────────────────

    pub async fn get(&self, id: Uuid) -> StorageResult<MemoryEntry> {
        self.storage.get(id).await
    }

    pub async fn list(&self, filters: ListFilters) -> StorageResult<Vec<MemoryEntry>> {
        let mut f = filters;
        if f.namespace.is_none() {
            f.namespace = Some(self.default_namespace.clone());
        }
        if f.limit.is_none() {
            f.limit = Some(20);
        }
        self.storage.list(f).await
    }

    pub async fn list_sessions(
        &self,
        namespace: Option<&str>,
        limit: usize,
    ) -> StorageResult<Vec<Session>> {
        self.storage.list_sessions(self.ns(namespace), limit).await
    }

    pub async fn get_timeline(
        &self,
        entry_id: Uuid,
        window_hours: i64,
    ) -> StorageResult<Vec<MemoryEntry>> {
        let entry = self.storage.get(entry_id).await?;
        let window = chrono::Duration::hours(window_hours);
        let start = entry.created_at - window;
        let end = entry.created_at + window;

        let all = self
            .storage
            .list(ListFilters {
                namespace: Some(entry.namespace.clone()),
                limit: Some(50),
                ..Default::default()
            })
            .await?;

        let mut filtered: Vec<MemoryEntry> = all
            .into_iter()
            .filter(|e| e.created_at >= start && e.created_at <= end)
            .collect();

        filtered.sort_by_key(|e| e.created_at);
        Ok(filtered)
    }

    // ── Context ────────────────────────────────────────────────

    pub async fn get_context(
        &self,
        namespace: Option<&str>,
        project_id: Option<&str>,
        session_count: usize,
    ) -> StorageResult<ContextPacket> {
        let ns = self.ns(namespace);

        let sessions = self.storage.list_sessions(ns, session_count * 2).await?;
        let recent_sessions: Vec<&Session> = sessions
            .iter()
            .filter(|s| s.status == SessionStatus::Completed)
            .take(session_count)
            .collect();

        let recent_entries = self
            .storage
            .list(ListFilters {
                namespace: Some(ns.to_string()),
                project_id: project_id.map(|s| s.to_string()),
                limit: Some(40),
                ..Default::default()
            })
            .await?;

        let stats = self.storage.get_stats(ns).await?;

        let formatted = format_context_packet(&recent_sessions, &recent_entries, &stats);

        Ok(ContextPacket {
            formatted,
            stats,
            recent_sessions: sessions
                .into_iter()
                .filter(|s| s.status == SessionStatus::Completed)
                .take(session_count)
                .collect(),
            recent_entries,
        })
    }

    // ── Decay ──────────────────────────────────────────────────

    pub fn compute_decay_score(&self, entry: &MemoryEntry, now: &chrono::DateTime<Utc>) -> f64 {
        let cfg = &self.decay_config;
        let last_access = entry.last_accessed_at.unwrap_or(entry.created_at);
        let days_since = (*now - last_access).num_seconds().max(0) as f64 / 86400.0;

        let base_weight = if entry.layer == MemoryLayer::ARCHIVAL {
            cfg.archival_base_weight
        } else {
            1.0
        };

        let access_boost =
            (entry.access_count as f64 * cfg.access_boost_rate).min(cfg.max_access_boost);

        let raw = base_weight * (-cfg.lambda * days_since).exp() + access_boost;
        raw.clamp(0.0, 1.0)
    }

    // ── Layer graduation ───────────────────────────────────────

    pub async fn graduate_layers(
        &self,
        namespace: Option<&str>,
    ) -> StorageResult<Vec<LayerTransition>> {
        self.graduate_layers_inner(namespace, false).await
    }

    /// Phase 5.4 — `dry_run=true` returns the planned transitions without
    /// mutating the store, so `runar gc --dry-run` can preview changes.
    pub async fn graduate_layers_inner(
        &self,
        namespace: Option<&str>,
        dry_run: bool,
    ) -> StorageResult<Vec<LayerTransition>> {
        let ns = self.ns(namespace);
        let entries = self
            .storage
            .list(ListFilters {
                namespace: Some(ns.to_string()),
                limit: Some(500),
                ..Default::default()
            })
            .await?;

        let now = Utc::now();
        let cfg = &self.decay_config;
        let thresholds = &cfg.graduation_thresholds;
        let mut transitions = Vec::new();

        for entry in &entries {
            let last_access = entry.last_accessed_at.unwrap_or(entry.created_at);
            let days_since = (now - last_access).num_days();

            let target_layer = decide_target_layer(entry, days_since, thresholds, cfg);

            if let Some(new_layer) = target_layer {
                if !dry_run {
                    let update = serde_json::json!({ "layer": new_layer.value() });
                    let _ = self.storage.update(entry.id, update).await;
                }

                transitions.push(LayerTransition {
                    id: entry.id,
                    title: entry.title.clone(),
                    previous_layer: entry.layer,
                    new_layer,
                    days_since_access: days_since,
                });
            }
        }

        Ok(transitions)
    }

    // ── Edges ──────────────────────────────────────────────────

    pub async fn save_edge(&self, input: MemoryEdgeInput) -> StorageResult<MemoryEdge> {
        self.storage.save_edge(input).await
    }

    pub async fn get_edges(
        &self,
        entry_id: Uuid,
        direction: Option<&str>,
    ) -> StorageResult<Vec<MemoryEdge>> {
        self.storage.get_edges(entry_id, direction).await
    }

    pub async fn delete_edge(&self, id: Uuid) -> StorageResult<()> {
        self.storage.delete_edge(id).await
    }

    // ── Stats ──────────────────────────────────────────────────

    pub async fn get_stats(&self, namespace: Option<&str>) -> StorageResult<MemoryStats> {
        self.storage.get_stats(self.ns(namespace)).await
    }

    // ── Admin ──────────────────────────────────────────────────

    /// Count entries + sessions that would be touched by a merge.
    /// Use this for the dry-run code path.
    pub async fn preview_merge(&self, source: &str) -> StorageResult<MergeCounts> {
        self.storage.count_project_namespace(source).await
    }

    /// Migrate all entries + sessions from `source` → `target`.
    pub async fn merge_projects(
        &self,
        source: &str,
        target: &str,
    ) -> StorageResult<MergeCounts> {
        self.storage.merge_project_namespace(source, target).await
    }

    // ── Pending Observation queue (auto-capture) ──────────────

    pub async fn enqueue_observation(
        &self,
        obs: ObservationInput,
        namespace: Option<&str>,
    ) -> StorageResult<Uuid> {
        self.storage
            .enqueue_observation(obs, self.ns(namespace))
            .await
    }

    pub async fn claim_observations(
        &self,
        namespace: Option<&str>,
        session_id: Option<Uuid>,
        max: usize,
    ) -> StorageResult<Vec<PendingObservation>> {
        self.storage
            .claim_observations(self.ns(namespace), session_id, max)
            .await
    }

    pub async fn confirm_observations(&self, ids: &[Uuid]) -> StorageResult<()> {
        self.storage.confirm_observations(ids).await
    }

    pub async fn recover_stale_observations(
        &self,
        older_than_secs: i64,
    ) -> StorageResult<i64> {
        self.storage.recover_stale_observations(older_than_secs).await
    }

    pub async fn check_observation_duplicate(
        &self,
        content_hash: &str,
        window_secs: i64,
    ) -> StorageResult<bool> {
        self.storage
            .check_observation_duplicate(content_hash, window_secs)
            .await
    }

    /// A10 — owner-endorse an entry. Sets `verified=true` + `verified_at=now`,
    /// returning the refreshed row. The ranking bonus kicks in on the next
    /// search because fused_search multiplies by a verified factor.
    pub async fn mark_verified(&self, id: Uuid) -> StorageResult<MemoryEntry> {
        let verified_by = crate::identity::resolve_author();
        let entry = self
            .storage
            .mark_verified(id, verified_by.as_deref())
            .await?;
        // Phase 5.6.2 — verified flip is a row mutation; push it.
        self.enqueue_outbox_for_entry(id, OutboxOp::Update).await;
        Ok(entry)
    }

    /// A6 — import a pre-serialized entry with its original id. Returns
    /// true if it was inserted, false if a conflicting id already existed.
    pub async fn import_entry(&self, entry: MemoryEntry) -> StorageResult<bool> {
        self.storage.import_entry(entry).await
    }

    pub async fn list_all_edges(&self, limit: usize) -> StorageResult<Vec<MemoryEdge>> {
        self.storage.list_all_edges(limit).await
    }

    pub async fn import_edge(&self, edge: MemoryEdge) -> StorageResult<bool> {
        self.storage.import_edge(edge).await
    }

    pub async fn import_session(&self, session: Session) -> StorageResult<bool> {
        self.storage.import_session(session).await
    }

    /// Phase 5.4 — soft-delete archival + low-confidence + zero-access
    /// entries older than `eviction_age_days`. Never touches `verified`.
    /// Returns the ids actually deleted.
    pub async fn evict_stale(&self, namespace: Option<&str>) -> StorageResult<Vec<Uuid>> {
        let cfg = &self.decay_config;
        self.storage
            .evict_stale(
                self.ns(namespace),
                cfg.eviction_age_days,
                cfg.low_confidence_threshold,
                cfg.eviction_max_per_run,
            )
            .await
    }

    // ── Passive capture (`## Key Learnings:` mining) ──────

    /// Parse a markdown blob, find the `## Key Learnings:` (or equivalent)
    /// section, and save each bulleted / numbered item as its own memory
    /// entry. Dedup piggybacks on the existing save-path via a stable
    /// `topic_key` derived from the normalized content — re-saving the same
    /// learning upserts the existing entry instead of creating a duplicate.
    ///
    /// Returns one `SaveResult` per parsed item in the order they appeared.
    pub async fn capture_passive(
        &self,
        text: &str,
        project_id: Option<&str>,
        entry_type: Option<EntryType>,
        extra_tags: &[String],
    ) -> StorageResult<Vec<SaveResult>> {
        let items = crate::capture::parse_key_learnings(text);
        if items.is_empty() {
            return Ok(Vec::new());
        }

        let entry_type = entry_type.unwrap_or(EntryType::Note);
        let mut results = Vec::with_capacity(items.len());

        for item in items {
            let mut tags: Vec<String> =
                vec!["auto-capture".into(), "key-learning".into()];
            if let Some(pid) = project_id {
                tags.push(pid.to_string());
            }
            for t in extra_tags {
                if !tags.contains(t) {
                    tags.push(t.clone());
                }
            }

            let input = MemoryEntryInput {
                title: item.title.clone(),
                content: item.content.clone(),
                entry_type,
                source: Some(MemorySource::Agent),
                tags,
                project_id: project_id.map(|s| s.to_string()),
                // Stable key so repeated saves upsert rather than duplicate.
                topic_key: Some(format!("learning:{}", item.dedup_hash)),
                ..Default::default()
            };

            match self.propose(input).await {
                Ok(res) => results.push(res),
                Err(e) => return Err(e),
            }
        }

        Ok(results)
    }
}

// ── Context formatting ─────────────────────────────────────────────

pub struct ContextPacket {
    pub formatted: String,
    pub stats: MemoryStats,
    pub recent_sessions: Vec<Session>,
    pub recent_entries: Vec<MemoryEntry>,
}

#[derive(Debug)]
pub struct LayerTransition {
    pub id: Uuid,
    pub title: String,
    pub previous_layer: MemoryLayer,
    pub new_layer: MemoryLayer,
    pub days_since_access: i64,
}

fn format_context_packet(
    sessions: &[&Session],
    entries: &[MemoryEntry],
    stats: &MemoryStats,
) -> String {
    let mut lines = vec![
        "## Memory Context (restored after compaction)".to_string(),
        String::new(),
    ];

    if !sessions.is_empty() {
        lines.push("### Recent Sessions".into());
        for s in sessions {
            let date = s.started_at.format("%Y-%m-%d").to_string();
            let goal = s
                .goal
                .as_deref()
                .map(|g| format!(": {g}"))
                .unwrap_or_default();
            lines.push(format!("- Session {date}{goal}"));
            if let Some(ref summary) = s.summary {
                lines.push(format!("  {summary}"));
            }
        }
        lines.push(String::new());
    }

    if !entries.is_empty() {
        lines.push("### Key Knowledge".into());
        for e in entries.iter().take(32) {
            let max_chars = match e.entry_type {
                EntryType::Bug | EntryType::Decision => 400,
                EntryType::Pattern => 300,
                EntryType::Architecture => 250,
                EntryType::Session => e.content.len(),
                _ => 200,
            };
            let summary = if e.content.len() > max_chars {
                format!("{}...", &e.content[..max_chars])
            } else {
                e.content.clone()
            };
            lines.push(format!(
                "- **{}** [{}]: {}",
                e.title,
                e.entry_type.as_str(),
                summary
            ));
        }
        lines.push(String::new());
    }

    lines.push("### Stats".into());
    lines.push(format!(
        "- {} memory entries | {} sessions",
        stats.total_entries, stats.total_sessions
    ));
    if !stats.namespaces.is_empty() {
        lines.push(format!("- Namespaces: {}", stats.namespaces.join(", ")));
    }

    lines.join("\n")
}

// ── Fire-and-forget helpers ────────────────────────────────────────

/// Phase 5.4 — pick the new layer for an entry, or None if no transition
/// should happen. Rule priority (first match wins):
///
/// 1. **Verified fast-promote:** `verified && layer == WORKING` → SEMANTIC.
///    Owner-endorsed entries skip the age ladder entirely.
/// 2. **Hebbian citation bump:** `access_count >= citation_threshold && layer < SEMANTIC`
///    → one layer up. Frequently-retrieved entries graduate faster than age alone.
/// 3. **Low-confidence demote:** `confidence < low_confidence_threshold &&
///    days_since >= 2× current graduation threshold && layer < ARCHIVAL` →
///    jump straight to ARCHIVAL. Weak observations age out fast.
/// 4. **Standard age ladder:** WORKING→EPISODIC at working_to_episodic_days,
///    EPISODIC→SEMANTIC at episodic_to_semantic_days, SEMANTIC→ARCHIVAL at
///    semantic_to_archival_days.
pub(crate) fn decide_target_layer(
    entry: &MemoryEntry,
    days_since: i64,
    thresholds: &GraduationThresholds,
    cfg: &DecayConfig,
) -> Option<MemoryLayer> {
    // Rule 1 — verified fast-promote
    if entry.verified && entry.layer == MemoryLayer::WORKING {
        return Some(MemoryLayer::SEMANTIC);
    }

    // Rule 2 — Hebbian citation bump
    if entry.access_count >= cfg.citation_threshold && entry.layer < MemoryLayer::SEMANTIC {
        let bumped = MemoryLayer::from(entry.layer.value() + 1);
        return Some(bumped);
    }

    // Rule 3 — low-confidence aggressive demote
    if entry.confidence < cfg.low_confidence_threshold && entry.layer < MemoryLayer::ARCHIVAL {
        let current_threshold = match entry.layer {
            l if l == MemoryLayer::WORKING => thresholds.working_to_episodic_days,
            l if l == MemoryLayer::EPISODIC => thresholds.episodic_to_semantic_days,
            l if l == MemoryLayer::SEMANTIC => thresholds.semantic_to_archival_days,
            _ => i64::MAX,
        };
        if days_since >= current_threshold.saturating_mul(2) {
            return Some(MemoryLayer::ARCHIVAL);
        }
    }

    // Rule 4 — standard age ladder
    if entry.layer == MemoryLayer::WORKING && days_since >= thresholds.working_to_episodic_days {
        return Some(MemoryLayer::EPISODIC);
    }
    if entry.layer == MemoryLayer::EPISODIC && days_since >= thresholds.episodic_to_semantic_days {
        return Some(MemoryLayer::SEMANTIC);
    }
    if entry.layer == MemoryLayer::SEMANTIC && days_since >= thresholds.semantic_to_archival_days {
        return Some(MemoryLayer::ARCHIVAL);
    }

    None
}

async fn touch_entry(storage: &dyn MemoryStorage, id: Uuid) -> StorageResult<()> {
    let entry = storage.get(id).await?;
    let new_count = entry.access_count + 1;
    let new_layer = if entry.layer.value() > MemoryLayer::EPISODIC.value() {
        MemoryLayer::EPISODIC.value()
    } else {
        entry.layer.value()
    };

    let update = serde_json::json!({
        "access_count": new_count,
        "last_accessed_at": Utc::now().to_rfc3339(),
        "layer": new_layer,
    });
    let _ = storage.update(id, update).await;
    Ok(())
}

async fn auto_link_entry(
    storage: &dyn MemoryStorage,
    embedding: &dyn EmbeddingProvider,
    entry_id: Uuid,
    namespace: &str,
) -> StorageResult<()> {
    if !embedding.is_available() {
        return Ok(());
    }

    let entry = storage.get(entry_id).await?;
    let search_text = format!("{} {}", entry.title, entry.content);
    let search_text = &search_text[..search_text.len().min(500)];

    let query_emb = match embedding.embed(search_text).await {
        Some(e) => e,
        None => return Ok(()),
    };

    let candidates = storage
        .semantic_search(search_text, &query_emb, 10, namespace)
        .await?;

    let existing_edges = storage.get_edges(entry_id, None).await.unwrap_or_default();
    let linked_ids: HashSet<Uuid> = existing_edges
        .iter()
        .flat_map(|e| [e.from_id, e.to_id])
        .collect();

    let to_link: Vec<&MemoryEntry> = candidates
        .iter()
        .filter(|c| c.id != entry_id && !linked_ids.contains(&c.id))
        .take(3)
        .collect();

    for candidate in to_link {
        let _ = storage
            .save_edge(MemoryEdgeInput {
                from_id: entry_id,
                to_id: candidate.id,
                edge_type: EdgeType::Related,
                strength: 0.8,
            })
            .await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::DisabledEmbeddingProvider;
    use crate::storage::sqlite::SqliteAdapter;

    async fn test_librarian() -> MemoryLibrarian {
        let storage = Arc::new(SqliteAdapter::in_memory("test").unwrap());
        storage.initialize().await.unwrap();
        let embedding = Arc::new(DisabledEmbeddingProvider);
        MemoryLibrarian::new(storage, embedding, "test", None)
    }

    #[tokio::test]
    async fn test_propose_and_get() {
        let lib = test_librarian().await;

        let result = lib
            .propose(MemoryEntryInput {
                title: "Auth decision".into(),
                content: "We chose JWT with refresh tokens".into(),
                entry_type: EntryType::Decision,
                tags: vec!["auth".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(result.action, SaveAction::Created);

        let entry = lib.get(result.id).await.unwrap();
        assert_eq!(entry.title, "Auth decision");
    }

    /// Phase 5.7 — explicit `author` on the input survives propose →
    /// storage → get unchanged. Also covers `mark_verified` recording
    /// `verified_by` from the resolver without disturbing `author`.
    #[tokio::test]
    async fn explicit_author_survives_propose_and_verify() {
        let lib = test_librarian().await;

        let result = lib
            .propose(MemoryEntryInput {
                title: "Stamped".into(),
                content: "with explicit author".into(),
                entry_type: EntryType::Note,
                author: Some("Alice".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        let entry = lib.get(result.id).await.unwrap();
        assert_eq!(entry.author.as_deref(), Some("Alice"));
        assert!(entry.verified_by.is_none());

        let verified = lib.mark_verified(result.id).await.unwrap();
        assert!(verified.verified);
        // `verified_by` resolves from `git config user.name` in this
        // environment; just assert it doesn't clobber author.
        assert_eq!(verified.author.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn test_session_lifecycle() {
        let lib = test_librarian().await;

        let session = lib
            .propose_session(SessionInput {
                goal: Some("Fix auth bug".into()),
                project_id: None,
                tool: Some("claude-code".into()),
            })
            .await
            .unwrap();

        assert_eq!(session.status, SessionStatus::Active);

        let active = lib.get_active_session(None).await.unwrap();
        assert!(active.is_some());
        assert_eq!(active.unwrap().id, session.id);

        let ended = lib
            .end_session(
                session.id,
                SessionSummary {
                    summary: "Fixed the JWT refresh bug".into(),
                    discoveries: vec!["Token expiry was too short".into()],
                    files_modified: vec!["auth.ts".into()],
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();

        assert_eq!(ended.status, SessionStatus::Completed);
    }

    #[tokio::test]
    async fn test_decay_score() {
        let lib = test_librarian().await;
        let now = Utc::now();

        let fresh_entry = MemoryEntry {
            id: Uuid::new_v4(),
            title: "Fresh".into(),
            content: "Just created".into(),
            entry_type: EntryType::Note,
            source: MemorySource::Human,
            tags: vec![],
            namespace: "test".into(),
            project_id: None,
            topic_key: None,
            layer: MemoryLayer::WORKING,
            importance: 0.5,
            decay_score: 1.0,
            access_count: 0,
            confidence: crate::types::DEFAULT_CONFIDENCE,
            embedding: None,
            verified: false,
            verified_at: None,
            author: None,
            verified_by: None,
            created_at: now,
            updated_at: now,
            last_accessed_at: Some(now),
            deleted_at: None,
        };

        let score = lib.compute_decay_score(&fresh_entry, &now);
        assert!(
            score > 0.95,
            "fresh entry should have score near 1.0, got {score}"
        );

        let old_entry = MemoryEntry {
            last_accessed_at: Some(now - chrono::Duration::days(30)),
            access_count: 0,
            layer: MemoryLayer::ARCHIVAL,
            ..fresh_entry.clone()
        };

        let old_score = lib.compute_decay_score(&old_entry, &now);
        assert!(
            old_score < 0.5,
            "30-day-old archival entry should have low score, got {old_score}"
        );

        let accessed_entry = MemoryEntry {
            last_accessed_at: Some(now - chrono::Duration::days(30)),
            access_count: 10,
            layer: MemoryLayer::ARCHIVAL,
            ..fresh_entry
        };

        let accessed_score = lib.compute_decay_score(&accessed_entry, &now);
        assert!(
            accessed_score > old_score,
            "frequently accessed entry should score higher: {accessed_score} vs {old_score}"
        );
    }

    #[tokio::test]
    async fn test_context_packet() {
        let lib = test_librarian().await;

        lib.propose(MemoryEntryInput {
            title: "Test entry for context".into(),
            content: "This should appear in the context packet".into(),
            entry_type: EntryType::Note,
            ..Default::default()
        })
        .await
        .unwrap();

        let ctx = lib.get_context(None, None, 3).await.unwrap();
        assert!(ctx.formatted.contains("Memory Context"));
        assert!(ctx.stats.total_entries >= 1);
    }

    #[tokio::test]
    async fn test_checkpoint_session_keeps_active() {
        let lib = test_librarian().await;

        let session = lib
            .propose_session(SessionInput {
                goal: Some("Long work".into()),
                project_id: Some("demo".into()),
                tool: Some("claude-code".into()),
            })
            .await
            .unwrap();

        let checkpointed = lib
            .checkpoint_session(
                session.id,
                SessionSummary {
                    summary: "halfway done".into(),
                    discoveries: vec!["finding A".into()],
                    files_modified: vec!["src/a.rs".into()],
                    ..Default::default()
                },
                Some("demo"),
            )
            .await
            .unwrap();

        assert_eq!(checkpointed.status, SessionStatus::Active);
        assert!(checkpointed.ended_at.is_none());
        assert_eq!(checkpointed.summary.as_deref(), Some("halfway done"));

        // A checkpoint saves a session-summary memory tagged session-checkpoint
        let results = lib
            .search("session summary", 10, Some("demo"), Some("demo"), None, None)
            .await
            .unwrap();
        assert!(
            results
                .iter()
                .any(|e| e.tags.contains(&"session-checkpoint".to_string())),
            "expected a session-checkpoint tagged memory entry"
        );
    }

    #[tokio::test]
    async fn test_merge_projects_moves_entries_and_sessions() {
        let lib = test_librarian().await;

        for i in 0..2 {
            lib.propose(MemoryEntryInput {
                title: format!("entry {i}"),
                content: "legacy".into(),
                entry_type: EntryType::Note,
                project_id: Some("runar_forge".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        lib.propose_session(SessionInput {
            goal: Some("legacy".into()),
            project_id: Some("runar_forge".into()),
            tool: Some("claude-code".into()),
        })
        .await
        .unwrap();

        let preview = lib.preview_merge("runar_forge").await.unwrap();
        assert_eq!(preview.entries, 2);
        assert_eq!(preview.sessions, 1);

        let counts = lib
            .merge_projects("runar_forge", "runar-forge")
            .await
            .unwrap();
        assert_eq!(counts.entries, 2);
        assert_eq!(counts.sessions, 1);

        let after_src = lib.preview_merge("runar_forge").await.unwrap();
        assert_eq!(after_src.entries, 0);
        assert_eq!(after_src.sessions, 0);

        let after_tgt = lib.preview_merge("runar-forge").await.unwrap();
        assert_eq!(after_tgt.entries, 2);
        assert_eq!(after_tgt.sessions, 1);
    }

    #[tokio::test]
    async fn test_propose_upsert_creates_supersedes_edge() {
        let lib = test_librarian().await;

        let first = lib
            .propose(MemoryEntryInput {
                title: "Auth approach v1".into(),
                content: "Initial plan".into(),
                entry_type: EntryType::Decision,
                topic_key: Some("decision:auth-approach".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(first.action, SaveAction::Created);
        assert!(first.superseded.is_none());

        let second = lib
            .propose(MemoryEntryInput {
                title: "Auth approach v2".into(),
                content: "Refined plan".into(),
                entry_type: EntryType::Decision,
                topic_key: Some("decision:auth-approach".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(second.action, SaveAction::Updated);
        let sup = second.superseded.expect("superseded metadata missing");
        assert_eq!(sup.id, first.id);

        let edges = lib.get_edges(second.id, Some("from")).await.unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Supersedes && e.to_id == first.id),
            "expected Supersedes edge new → old; got {:?}",
            edges
        );
    }

    #[tokio::test]
    async fn test_confidence_affects_search_ranking() {
        let lib = test_librarian().await;

        // Speculative entry saved first so it also wins the recency signal —
        // ranking should still put the verified entry ahead via the
        // confidence multiplier.
        lib.propose(MemoryEntryInput {
            title: "Cache invalidation edge case".into(),
            content: "Speculative guess about cache invalidation issues".into(),
            entry_type: EntryType::Note,
            confidence: Some(0.4),
            ..Default::default()
        })
        .await
        .unwrap();

        lib.propose(MemoryEntryInput {
            title: "Cache invalidation decision".into(),
            content: "Verified cache invalidation strategy used in production".into(),
            entry_type: EntryType::Decision,
            confidence: Some(1.0),
            ..Default::default()
        })
        .await
        .unwrap();

        let results = lib
            .search("cache invalidation", 10, None, None, None, None)
            .await
            .unwrap();

        assert!(results.len() >= 2, "expected both entries returned");
        assert_eq!(
            results[0].title, "Cache invalidation decision",
            "verified entry must outrank speculative peer"
        );
    }

    #[tokio::test]
    async fn test_fts_search_via_librarian() {
        let lib = test_librarian().await;

        lib.propose(MemoryEntryInput {
            title: "Database migration strategy".into(),
            content: "We use Drizzle ORM with PostgreSQL for all migrations".into(),
            entry_type: EntryType::Decision,
            tags: vec!["database".into()],
            ..Default::default()
        })
        .await
        .unwrap();

        let results = lib
            .search("Drizzle PostgreSQL", 10, None, None, None, None)
            .await
            .unwrap();

        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Database migration strategy");
    }

    #[tokio::test]
    async fn test_capture_passive_fans_out() {
        let lib = test_librarian().await;

        let text = "\
Here's a summary of what we did.\n\
\n\
## Key Learnings:\n\
- The auth middleware was expiring tokens one second early.\n\
- We standardized on UUID v4 for all primary keys.\n\
- Background jobs need explicit RUNAR_STORAGE=postgresql.\n\
\n\
## Next Steps\n\
- something unrelated\n";

        let results = lib
            .capture_passive(text, Some("proj-a2"), Some(EntryType::Note), &[])
            .await
            .unwrap();

        assert_eq!(results.len(), 3, "expected 3 learnings, got {}", results.len());
        for r in &results {
            assert_eq!(r.action, SaveAction::Created);
        }

        // Second call with same text upserts via stable topic_key.
        let second = lib
            .capture_passive(text, Some("proj-a2"), Some(EntryType::Note), &[])
            .await
            .unwrap();
        assert_eq!(second.len(), 3);
        assert!(
            second.iter().all(|r| matches!(r.action, SaveAction::Updated)),
            "repeated capture should upsert, not duplicate"
        );
    }

    #[tokio::test]
    async fn test_propose_redacts_private_content() {
        let lib = test_librarian().await;

        let result = lib
            .propose(MemoryEntryInput {
                title: "API token rotation".into(),
                content: "Rotate via <private>sk-live-abc123</private> endpoint weekly."
                    .into(),
                entry_type: EntryType::Rule,
                ..Default::default()
            })
            .await
            .unwrap();

        let entry = lib.get(result.id).await.unwrap();
        assert!(
            !entry.content.contains("sk-live-abc123"),
            "secret should not leak into stored content: {}",
            entry.content
        );
        assert!(entry.content.contains("[redacted]"));
        assert!(
            entry.tags.iter().any(|t| t == "redacted"),
            "expected `redacted` tag on entries that had private blocks, got {:?}",
            entry.tags
        );
    }

    #[tokio::test]
    async fn test_propose_leaves_non_private_content_untouched() {
        let lib = test_librarian().await;

        let result = lib
            .propose(MemoryEntryInput {
                title: "Plain title".into(),
                content: "Plain content with <code>markup</code>".into(),
                entry_type: EntryType::Note,
                ..Default::default()
            })
            .await
            .unwrap();

        let entry = lib.get(result.id).await.unwrap();
        assert_eq!(entry.content, "Plain content with <code>markup</code>");
        assert!(!entry.tags.iter().any(|t| t == "redacted"));
    }

    #[tokio::test]
    async fn test_verify_sets_flag_and_boosts_rank() {
        let lib = test_librarian().await;

        // Two entries of identical recency + confidence — verify should tip the
        // scale.
        let unverified = lib
            .propose(MemoryEntryInput {
                title: "cache invalidation approach".into(),
                content: "invalidate cache with TTL + explicit bust on write".into(),
                entry_type: EntryType::Decision,
                confidence: Some(0.9),
                ..Default::default()
            })
            .await
            .unwrap();

        let to_verify = lib
            .propose(MemoryEntryInput {
                title: "cache invalidation pattern".into(),
                content: "invalidate cache via explicit bust on write".into(),
                entry_type: EntryType::Decision,
                confidence: Some(0.9),
                ..Default::default()
            })
            .await
            .unwrap();

        let verified_entry = lib.mark_verified(to_verify.id).await.unwrap();
        assert!(verified_entry.verified);
        assert!(verified_entry.verified_at.is_some());

        let refetched = lib.get(to_verify.id).await.unwrap();
        assert!(refetched.verified, "verified flag must persist");

        let results = lib
            .search("cache invalidation", 10, None, None, None, None)
            .await
            .unwrap();

        let verified_pos = results.iter().position(|e| e.id == to_verify.id);
        let unverified_pos = results.iter().position(|e| e.id == unverified.id);
        assert!(verified_pos.is_some(), "verified entry should be returned");
        assert!(
            verified_pos <= unverified_pos,
            "verified entry must rank at least as high as unverified peer; got verified@{:?}, unverified@{:?}",
            verified_pos,
            unverified_pos
        );
    }

    #[tokio::test]
    async fn test_verify_missing_id_returns_not_found() {
        let lib = test_librarian().await;
        let missing = Uuid::new_v4();
        let err = lib.mark_verified(missing).await.unwrap_err();
        assert!(matches!(err, crate::storage::StorageError::NotFound(id) if id == missing));
    }

    // ── Phase 5.4 tier tests ──────────────────────────────────

    fn tier_entry(
        layer: MemoryLayer,
        access_count: i32,
        verified: bool,
        confidence: f32,
        days_old: i64,
    ) -> MemoryEntry {
        let now = Utc::now();
        MemoryEntry {
            id: Uuid::new_v4(),
            title: "tier-fixture".into(),
            content: "x".into(),
            entry_type: EntryType::Note,
            source: MemorySource::Agent,
            tags: vec![],
            namespace: "test".into(),
            project_id: None,
            topic_key: None,
            layer,
            importance: 0.5,
            decay_score: 1.0,
            access_count,
            confidence,
            embedding: None,
            verified,
            verified_at: if verified { Some(now) } else { None },
            author: None,
            verified_by: None,
            created_at: now - chrono::Duration::days(days_old),
            updated_at: now,
            last_accessed_at: Some(now - chrono::Duration::days(days_old)),
            deleted_at: None,
        }
    }

    #[test]
    fn tier_rule_verified_fast_promote_to_semantic() {
        let cfg = DecayConfig::default();
        let e = tier_entry(MemoryLayer::WORKING, 0, true, 0.9, 0);
        let got = decide_target_layer(&e, 0, &cfg.graduation_thresholds, &cfg);
        assert_eq!(got, Some(MemoryLayer::SEMANTIC));
    }

    #[test]
    fn tier_rule_citation_bumps_one_layer() {
        let cfg = DecayConfig::default();
        // access_count = citation_threshold → bump one layer
        let e = tier_entry(MemoryLayer::WORKING, 5, false, 0.9, 0);
        let got = decide_target_layer(&e, 0, &cfg.graduation_thresholds, &cfg);
        assert_eq!(got, Some(MemoryLayer::EPISODIC));

        // Also fires from EPISODIC
        let e2 = tier_entry(MemoryLayer::EPISODIC, 5, false, 0.9, 0);
        let got2 = decide_target_layer(&e2, 0, &cfg.graduation_thresholds, &cfg);
        assert_eq!(got2, Some(MemoryLayer::SEMANTIC));
    }

    #[test]
    fn tier_rule_low_confidence_demotes_to_archival() {
        let cfg = DecayConfig::default();
        // confidence=0.3, age = 2× working_to_episodic threshold (2×7=14)
        let e = tier_entry(MemoryLayer::WORKING, 0, false, 0.3, 14);
        let got = decide_target_layer(&e, 14, &cfg.graduation_thresholds, &cfg);
        assert_eq!(got, Some(MemoryLayer::ARCHIVAL));
    }

    #[test]
    fn tier_rule_low_confidence_below_2x_threshold_does_not_jump() {
        let cfg = DecayConfig::default();
        // Low confidence but only just past normal threshold → takes
        // standard ladder, not the aggressive demote.
        let e = tier_entry(MemoryLayer::WORKING, 0, false, 0.3, 7);
        let got = decide_target_layer(&e, 7, &cfg.graduation_thresholds, &cfg);
        assert_eq!(got, Some(MemoryLayer::EPISODIC));
    }

    #[test]
    fn tier_rule_standard_age_ladder_still_fires() {
        let cfg = DecayConfig::default();
        // Healthy entry, no citations, no verify, just old.
        let e = tier_entry(MemoryLayer::SEMANTIC, 0, false, 0.9, 30);
        let got = decide_target_layer(&e, 30, &cfg.graduation_thresholds, &cfg);
        assert_eq!(got, Some(MemoryLayer::ARCHIVAL));
    }

    #[test]
    fn tier_rule_no_transition_when_not_ready() {
        let cfg = DecayConfig::default();
        let e = tier_entry(MemoryLayer::WORKING, 2, false, 0.9, 3);
        let got = decide_target_layer(&e, 3, &cfg.graduation_thresholds, &cfg);
        assert_eq!(got, None);
    }

    #[test]
    fn tier_rule_verified_not_promoted_past_semantic() {
        let cfg = DecayConfig::default();
        // verified only fast-promotes from WORKING; SEMANTIC-verified stays put
        // unless the age ladder fires.
        let e = tier_entry(MemoryLayer::SEMANTIC, 0, true, 0.9, 0);
        let got = decide_target_layer(&e, 0, &cfg.graduation_thresholds, &cfg);
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn graduate_layers_transitions_old_entries() {
        let lib = test_librarian().await;

        let result = lib
            .propose(MemoryEntryInput {
                title: "Aging entry".into(),
                content: "Should graduate to EPISODIC".into(),
                entry_type: EntryType::Note,
                ..Default::default()
            })
            .await
            .unwrap();

        // Backdate last_accessed_at so standard ladder fires.
        let backdated = serde_json::json!({
            "last_accessed_at": (Utc::now() - chrono::Duration::days(10)).to_rfc3339(),
        });
        lib.storage.update(result.id, backdated).await.unwrap();

        let transitions = lib.graduate_layers(None).await.unwrap();
        assert!(
            transitions.iter().any(|t| t.id == result.id
                && t.new_layer == MemoryLayer::EPISODIC),
            "expected WORKING→EPISODIC transition, got {:?}",
            transitions
        );

        let refetched = lib.get(result.id).await.unwrap();
        assert_eq!(refetched.layer, MemoryLayer::EPISODIC);
    }

    #[tokio::test]
    async fn evict_stale_respects_verified_and_cap() {
        let lib = test_librarian().await;

        // Insert a stale archival entry — unverified, low confidence, old access
        let victim = lib
            .propose(MemoryEntryInput {
                title: "Stale victim".into(),
                content: "x".into(),
                entry_type: EntryType::Note,
                confidence: Some(0.1),
                ..Default::default()
            })
            .await
            .unwrap();
        let stale_update = serde_json::json!({
            "layer": MemoryLayer::ARCHIVAL.value(),
            "last_accessed_at": (Utc::now() - chrono::Duration::days(200)).to_rfc3339(),
        });
        lib.storage.update(victim.id, stale_update).await.unwrap();

        // Verified entry with identical staleness profile — must survive.
        let protected = lib
            .propose(MemoryEntryInput {
                title: "Verified survivor".into(),
                content: "x".into(),
                entry_type: EntryType::Note,
                confidence: Some(0.1),
                ..Default::default()
            })
            .await
            .unwrap();
        lib.mark_verified(protected.id).await.unwrap();
        let stale_update2 = serde_json::json!({
            "layer": MemoryLayer::ARCHIVAL.value(),
            "last_accessed_at": (Utc::now() - chrono::Duration::days(200)).to_rfc3339(),
        });
        lib.storage
            .update(protected.id, stale_update2)
            .await
            .unwrap();

        let evicted = lib.evict_stale(None).await.unwrap();
        assert!(evicted.contains(&victim.id));
        assert!(!evicted.contains(&protected.id), "verified must never evict");

        // Victim was soft-deleted → storage.get filters `deleted_at IS NULL`
        // so it returns NotFound. Protected entry still fetchable.
        assert!(matches!(
            lib.storage.get(victim.id).await,
            Err(crate::storage::StorageError::NotFound(_))
        ));
        let pro = lib.storage.get(protected.id).await.unwrap();
        assert!(pro.deleted_at.is_none());
    }

    #[tokio::test]
    async fn test_import_edge_and_session_roundtrip() {
        let src = test_librarian().await;

        // Two entries so the edge has valid endpoints after import.
        let a = src
            .propose(MemoryEntryInput {
                title: "Decision A".into(),
                content: "We chose X".into(),
                entry_type: EntryType::Decision,
                ..Default::default()
            })
            .await
            .unwrap();
        let b = src
            .propose(MemoryEntryInput {
                title: "Decision B (supersedes A)".into(),
                content: "We actually chose Y".into(),
                entry_type: EntryType::Decision,
                ..Default::default()
            })
            .await
            .unwrap();

        let edge = src
            .save_edge(MemoryEdgeInput {
                from_id: b.id,
                to_id: a.id,
                edge_type: EdgeType::Supersedes,
                strength: 1.0,
            })
            .await
            .unwrap();

        let session = src
            .propose_session(SessionInput {
                goal: Some("exportable session".into()),
                project_id: Some("exp-test".into()),
                tool: Some("claude-code".into()),
            })
            .await
            .unwrap();

        let dest = test_librarian().await;

        // Entries must exist before edges import; edges reference their ids.
        assert!(dest.import_entry(src.get(a.id).await.unwrap()).await.unwrap());
        assert!(dest.import_entry(src.get(b.id).await.unwrap()).await.unwrap());
        assert!(dest.import_edge(edge.clone()).await.unwrap());
        assert!(!dest.import_edge(edge).await.unwrap(), "duplicate edge skip");

        assert!(dest.import_session(session.clone()).await.unwrap());
        assert!(
            !dest.import_session(session.clone()).await.unwrap(),
            "duplicate session skip"
        );

        // Verify the edge round-tripped with correct endpoints + type.
        let edges_from_b = dest.get_edges(b.id, Some("from")).await.unwrap();
        assert!(edges_from_b
            .iter()
            .any(|e| e.edge_type == EdgeType::Supersedes && e.to_id == a.id));
    }

    #[tokio::test]
    async fn test_import_preserves_id_and_dedups() {
        let src = test_librarian().await;

        let save_result = src
            .propose(MemoryEntryInput {
                title: "Exportable entry".into(),
                content: "This row should survive export + import unchanged".into(),
                entry_type: EntryType::Pattern,
                tags: vec!["export-test".into()],
                confidence: Some(0.8),
                ..Default::default()
            })
            .await
            .unwrap();
        let original = src.get(save_result.id).await.unwrap();

        // Fresh destination librarian (simulates export → import into a new DB)
        let dest = test_librarian().await;

        let inserted = dest.import_entry(original.clone()).await.unwrap();
        assert!(inserted, "first import should insert");

        let roundtrip = dest.get(original.id).await.unwrap();
        assert_eq!(roundtrip.id, original.id, "id must survive roundtrip");
        assert_eq!(roundtrip.title, original.title);
        assert_eq!(roundtrip.content, original.content);
        assert_eq!(roundtrip.tags, original.tags);
        assert_eq!(roundtrip.entry_type, original.entry_type);

        // Second import of the same row is a no-op.
        let reinserted = dest.import_entry(original).await.unwrap();
        assert!(!reinserted, "duplicate id must skip, not overwrite");
    }

    #[tokio::test]
    async fn test_capture_passive_no_section_returns_empty() {
        let lib = test_librarian().await;
        let results = lib
            .capture_passive("no header here, just prose", None, None, &[])
            .await
            .unwrap();
        assert!(results.is_empty());
    }
}
