use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::embedding::EmbeddingProvider;
use crate::storage::{MemoryStorage, StorageResult};
use crate::types::*;

/// Ceiling on an entry's stored content, in characters.
///
/// Matches the remote's own `CHECK (char_length(content) <= 10000)`
/// (`pg_sql/001_initial_schema.sql`). SQLite carries no equivalent CHECK, and
/// that divergence is not cosmetic: an oversized entry saved locally and was
/// then rejected by the remote on every push attempt, forever. Override with
/// `RUNAR_MAX_CONTENT_CHARS` — but raising it past the remote's CHECK just
/// moves the failure back to push time.
pub const MAX_CONTENT_CHARS: usize = 10_000;

pub fn content_limit() -> usize {
    std::env::var("RUNAR_MAX_CONTENT_CHARS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(MAX_CONTENT_CHARS)
}

/// Bound an entry's content, returning the bounded text and the number of
/// characters dropped.
///
/// The marker states the count, because a bare ellipsis leaves a reader unable
/// to tell a lightly-clipped entry from one that lost 97% of its body. Every
/// measurement is in chars: this repo has permanently broken a hook by
/// comparing a byte length against a char budget.
pub fn bound_content(content: &str, limit: usize) -> (String, usize) {
    let total = content.chars().count();
    if total <= limit {
        return (content.to_string(), 0);
    }
    let dropped = total - limit;
    let marker = format!("\n\n… [truncated {dropped} chars]");
    // Spend part of the budget on the marker, so the result still fits the
    // limit rather than overshooting it by the marker's length.
    let body_budget = limit.saturating_sub(marker.chars().count());
    (
        format!("{}{marker}", crate::text::char_prefix(content, body_budget)),
        dropped,
    )
}

pub struct MemoryLibrarian {
    storage: Arc<dyn MemoryStorage>,
    embedding: Arc<dyn EmbeddingProvider>,
    default_namespace: String,
    decay_config: DecayConfig,
    /// Captured at construction (RUNAR_DEBUG) rather than read per call, so
    /// tests can flip it per instance without process-global env mutation.
    debug_enabled: bool,
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
            debug_enabled: crate::debug::enabled(),
        }
    }

    /// Override the RUNAR_DEBUG gate for this instance (tests, tooling).
    pub fn with_debug(mut self, on: bool) -> Self {
        self.debug_enabled = on;
        self
    }

    /// Passthroughs for the muninn_debug tool.
    pub async fn query_debug_log(&self, q: DebugLogQuery) -> StorageResult<Vec<DebugLogEntry>> {
        self.storage.query_debug_log(q).await
    }

    pub async fn prune_debug_log(&self, older_than_days: i64) -> StorageResult<i64> {
        self.storage.prune_debug_log(older_than_days).await
    }

    /// Best-effort HookTiming event. Callers gate on RUNAR_DEBUG.
    pub async fn write_hook_timing(&self, hook: &str, budget_exceeded: bool, duration_ms: f64) {
        crate::debug::log(
            &self.storage,
            DebugLogInput {
                event: DebugEvent::HookTiming,
                entry_id: None,
                data: serde_json::json!({
                    "hook": hook,
                    "budgetExceeded": budget_exceeded,
                }),
                duration_ms: Some(duration_ms),
            },
        )
        .await;
    }

    fn ns<'a>(&'a self, namespace: Option<&'a str>) -> &'a str {
        namespace.unwrap_or(&self.default_namespace)
    }

    /// Resolve the namespace for a read the same way `propose` resolves it
    /// for a write: an explicit namespace wins, else the project id IS the
    /// namespace, else the default. Keeping read and write resolution
    /// symmetric is what makes project-scoped retrieval actually see
    /// project-scoped entries.
    fn scope<'a>(&'a self, namespace: Option<&'a str>, project_id: Option<&'a str>) -> &'a str {
        namespace.or(project_id).unwrap_or(&self.default_namespace)
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

        // Secret-pattern redaction (tokens, passwords, keys) on the same
        // chokepoint so every entry write path — MCP save, prompt capture,
        // extraction, huginn crawl — is covered. Tags are redacted too:
        // they are caller-supplied and FTS-indexed.
        let (title_scrubbed, title_hits) = crate::redact::redact_secrets(&input.title);
        let (content_scrubbed, content_hits) = crate::redact::redact_secrets(&input.content);
        let mut secret_hits: usize = title_hits
            .iter()
            .chain(content_hits.iter())
            .map(|h| h.count)
            .sum();
        input.title = title_scrubbed;
        input.content = content_scrubbed;
        for tag in input.tags.iter_mut() {
            let (clean, hits) = crate::redact::redact_secrets(tag);
            let n: usize = hits.iter().map(|h| h.count).sum();
            if n > 0 {
                *tag = clean;
                secret_hits += n;
            }
        }
        // The topic key is caller-supplied on the MCP save path and is stored
        // and returned verbatim, so a secret pasted into it outlived the
        // scrubbing of every other field. Only the *derived* key was clean,
        // because that one is built from an already-redacted title.
        if let Some(ref mut key) = input.topic_key {
            secret_hits += crate::redact::scrub(key);
        }
        if secret_hits > 0 && !input.tags.iter().any(|t| t == "redacted:secret") {
            input.tags.push("redacted:secret".to_string());
        }

        // Bound the content on the same chokepoint, and AFTER redaction so
        // the limit applies to what actually gets stored — truncating first
        // could cut a secret in half and hide it from the matchers.
        //
        // The remote enforces `char_length(content) <= 10000`; SQLite has no
        // such CHECK, so an oversized entry used to save locally and then be
        // rejected by the remote forever. 107 rows sat unsyncable for months
        // behind a `db error` that named nothing.
        let over_limit = if input.exact_content {
            input.content.chars().count() > content_limit()
        } else {
            let (bounded, dropped) = bound_content(&input.content, content_limit());
            if dropped > 0 {
                input.content = bounded;
                if !input.tags.iter().any(|t| t == "truncated") {
                    input.tags.push("truncated".to_string());
                }
            }
            false
        };

        // Phase 5.7 — stamp author from `git config user.name` when the
        // caller did not pass an explicit value. Stays None if git isn't
        // configured; the storage column then remains NULL.
        if input.author.is_none() {
            input.author = crate::identity::resolve_author();
        }

        let embed_text = format!("{} {}", input.title, input.content);
        let input_chars = input.content.chars().count();
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
            //
            // An `exact_content` entry over the limit is deliberately not
            // queued: the remote's CHECK would reject it on every attempt
            // until the dead-letter cap parked it, so queueing it only
            // manufactures noise. Say so once, loudly, instead.
            if over_limit {
                tracing::warn!(
                    entry = %entry_id,
                    chars = input_chars,
                    limit = content_limit(),
                    "entry exceeds the remote content limit and will not sync \
                     (exact_content is set, so it is stored whole locally)"
                );
            } else {
                self.enqueue_outbox_for_entry(entry_id, OutboxOp::Insert)
                    .await;
            }

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
        // Phase 5.6.2 — soft-delete needs to propagate to remote.
        //
        // Snapshot AFTER deleting, through the reader that can see
        // tombstones. Two constraints meet here:
        //
        //   - `storage.get` filters `deleted_at IS NULL`, so the
        //     original delete-then-`get` returned NotFound and the
        //     enqueue bailed silently. Zero delete ops had ever reached
        //     the outbox, and every deletion in the tree (dedup,
        //     retirement, crawler cleanup) funnels through here, so a
        //     remote drain would resurrect all of it.
        //   - `push_one` deserializes the payload as a full
        //     `MemoryEntry`, so a stub is unpushable — and a pre-delete
        //     snapshot would carry `deleted_at: None`, telling the
        //     remote the entry is alive. The payload has to be the
        //     deleted row itself.
        self.storage.delete(id).await?;
        let snapshot = self.outbox_snapshot(id).await;
        self.enqueue_outbox_snapshot(id, OutboxOp::Delete, snapshot)
            .await;
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
        let payload = self.outbox_snapshot(entry_id).await;
        self.enqueue_outbox_snapshot(entry_id, op_kind, payload)
            .await;
    }

    /// Serialize the current row for an outbox payload. `None` when the
    /// row is unreadable or unserializable, or when hybrid mode is off.
    ///
    /// Reads through `get_including_deleted` because a delete's payload
    /// is the soft-deleted row itself.
    async fn outbox_snapshot(&self, entry_id: Uuid) -> Option<serde_json::Value> {
        if std::env::var("RUNAR_STORAGE_LOCAL").is_err() {
            return None;
        }
        let entry = self.storage.get_including_deleted(entry_id).await.ok()?;
        match serde_json::to_value(&entry) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(error = %e, "outbox payload serialization failed");
                None
            }
        }
    }

    /// Append an outbox row from an already-captured payload.
    async fn enqueue_outbox_snapshot(
        &self,
        entry_id: Uuid,
        op_kind: OutboxOp,
        payload: Option<serde_json::Value>,
    ) {
        if std::env::var("RUNAR_STORAGE_LOCAL").is_err() {
            return;
        }
        // No id-stub fallback: `push_one` deserializes the payload as a
        // full `MemoryEntry`, so a stub is unpushable and would just
        // fail its way to the dead-letter queue. Dropping the enqueue
        // leaves the entry visible to `sync repair`, which can retry it
        // once the row is readable again.
        let Some(payload) = payload else {
            tracing::warn!(entry = %entry_id, op = op_kind.as_str(), "outbox snapshot unavailable");
            return;
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
        // Straight to storage, so it scrubs here. The goal is model- or
        // caller-supplied free text — "set up deploys with sk-ant-…" is an
        // ordinary thing to write — and it is persisted on the session row and
        // replayed into the next session's context packet.
        let mut input = input;
        if let Some(ref mut goal) = input.goal {
            crate::redact::scrub(goal);
        }
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
        // This path persists via storage.save directly (no propose), so it
        // must redact here: summaries/goals/discoveries are model-supplied
        // text and end up both in the sessions table and in an FTS-indexed
        // session entry.
        let mut summary = summary;
        let redact_string = |s: &mut String| {
            let (clean, hits) = crate::redact::redact_secrets(s);
            if !hits.is_empty() {
                *s = clean;
            }
        };
        redact_string(&mut summary.summary);
        if let Some(ref mut g) = summary.goal {
            redact_string(g);
        }
        for item in summary
            .instructions
            .iter_mut()
            .chain(summary.accomplished.iter_mut())
            .chain(summary.discoveries.iter_mut())
        {
            redact_string(item);
        }

        // Goal and discoveries used to be dropped here (SessionUpdate had no
        // fields for them) — the reason 3 months of sessions had discoveries
        // '[]' and boilerplate goals.
        let update = if checkpoint {
            SessionUpdate {
                status: None,
                summary: Some(summary.summary.clone()),
                ended_at: None,
                files_modified: Some(summary.files_modified.clone()),
                goal: summary.goal.clone(),
                discoveries: Some(summary.discoveries.clone()),
            }
        } else {
            SessionUpdate {
                status: Some(SessionStatus::Completed),
                summary: Some(summary.summary.clone()),
                ended_at: Some(Utc::now()),
                files_modified: Some(summary.files_modified.clone()),
                goal: summary.goal.clone(),
                discoveries: Some(summary.discoveries.clone()),
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
        // `pid` reaches both the title and a tag, and a project id is just a
        // caller-supplied string. The summary body is already scrubbed above;
        // these two were not, because this path never goes through `propose`.
        let mut title = format!("Session summary — {pid}");
        let mut pid_tag = pid.to_string();
        crate::redact::scrub(&mut title);
        crate::redact::scrub(&mut pid_tag);
        let _ = self
            .storage
            .save(
                MemoryEntryInput {
                    title,
                    content,
                    entry_type: EntryType::Session,
                    source: Some(MemorySource::System),
                    tags: vec!["session-summary".into(), tag.into(), pid_tag],
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

    pub async fn get_session(&self, id: uuid::Uuid) -> StorageResult<Session> {
        self.storage.get_session(id).await
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
        self.fused_search_inner(query, limit, namespace, project_id, entry_type, tags, true)
            .await
    }

    /// Ranked recall for automatic context injection.
    ///
    /// Same ranking as `fused_search`, but records the result as an
    /// *injection* rather than a *retrieval*: `injected_count` instead of
    /// `access_count`. The distinction matters because this fires on every
    /// user prompt — folding it into `access_count` would promote the same
    /// handful of entries past `citation_threshold` within hours and lock
    /// them into the decay/Hebbian path, exactly the failure mode that made
    /// the old recency-window packet self-reinforcing.
    pub async fn recall_for_prompt(
        &self,
        prompt: &str,
        limit: usize,
        project_id: Option<&str>,
    ) -> StorageResult<Vec<MemoryEntry>> {
        let started = std::time::Instant::now();
        // Over-fetch, then drop the types that are captured *input* rather
        // than knowledge, plus the ones that describe work not yet done.
        // Without this the arm recalls the very prompt it was triggered by —
        // user prompts are the largest and most textually similar cohort in
        // the corpus, and a 0.5× rank multiplier is not enough to keep them
        // out of the top 8. See `EntryType::excluded_from_injection`.
        let entries: Vec<MemoryEntry> = self
            .fused_search_inner(prompt, limit * 3, None, project_id, None, None, false)
            .await?
            .into_iter()
            .filter(|e| !e.entry_type.excluded_from_injection())
            .take(limit)
            .collect();

        let ids: Vec<Uuid> = entries.iter().map(|e| e.id).collect();
        if ids.is_empty() {
            return Ok(entries);
        }
        let _ = self.storage.mark_injected(&ids).await;

        if self.debug_enabled {
            crate::debug::log(
                &self.storage,
                DebugLogInput {
                    event: DebugEvent::Injection,
                    entry_id: None,
                    data: serde_json::json!({
                        "hookEvent": "UserPromptSubmit",
                        "projectId": project_id,
                        "namespace": self.scope(None, project_id),
                        "entryIds": ids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
                        "entryCount": ids.len(),
                    }),
                    duration_ms: Some(started.elapsed().as_secs_f64() * 1000.0),
                },
            )
            .await;
        }

        Ok(entries)
    }

    #[allow(clippy::too_many_arguments)]
    async fn fused_search_inner(
        &self,
        query: &str,
        limit: usize,
        namespace: Option<&str>,
        project_id: Option<&str>,
        entry_type: Option<EntryType>,
        tags: Option<Vec<String>>,
        touch: bool,
    ) -> StorageResult<Vec<MemoryEntry>> {
        const USER_PROMPT_RANK_MULTIPLIER: f64 = 0.5;
        let search_started = std::time::Instant::now();
        let ns = self.scope(namespace, project_id);
        let over_fetch = (limit * 3).min(50);
        let k: f64 = 60.0;

        let search_query = SearchQuery {
            query: query.trim().to_string(),
            limit: Some(over_fetch),
            entry_type,
            project_id: project_id.map(|s| s.to_string()),
            tags: tags.clone(),
            namespace: Some(ns.to_string()),
        };

        // Run semantic + FTS in parallel, both arms identically scoped.
        let (semantic_result, fts_result) = tokio::join!(
            self.run_semantic_search(query, search_query.clone()),
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

            // User prompts are raw inputs, not curated knowledge — keep them
            // retrievable but never let them outrank a real decision/pattern
            // at similar relevance. 0.5 halves the fused score: symmetric
            // with the worst-case confidence penalty, and strong enough that
            // the 1.25× verified bonus cannot recover it.
            if entry.entry_type == EntryType::UserPrompt {
                rrf_score *= USER_PROMPT_RANK_MULTIPLIER;
            }

            scored.push((rrf_score, entry.clone()));
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Tag filter applied here rather than in SQL: tags are stored as a
        // JSON text column with backend-divergent representations, so a
        // librarian post-filter keeps semantics identical across backends.
        // Runs before truncation so `limit` stays honest.
        if let Some(ref required) = tags {
            if !required.is_empty() {
                scored.retain(|(_, e)| required.iter().all(|t| e.tags.contains(t)));
            }
        }

        scored.truncate(limit);

        // Record retrieval before returning: one atomic UPDATE for all
        // returned ids. Awaited on purpose — the old fire-and-forget spawn
        // died with short-lived hook processes and left partial writes.
        // Skipped for automatic recall, which counts as an injection.
        let touched_ids: Vec<Uuid> = if touch {
            let ids: Vec<Uuid> = scored.iter().map(|(_, e)| e.id).collect();
            let _ = self.storage.touch_entries(&ids).await;
            ids
        } else {
            Vec::new()
        };

        if self.debug_enabled {
            let duration_ms = search_started.elapsed().as_secs_f64() * 1000.0;
            crate::debug::log(
                &self.storage,
                DebugLogInput {
                    event: DebugEvent::SearchScoring,
                    entry_id: None,
                    data: serde_json::json!({
                        // Redacted: debug_log is not namespace-scoped, and
                        // automatic recall now puts whole user prompts through
                        // this path. Clean while n=3 is not a guarantee.
                        "query": crate::redact::redact_secrets(query).0,
                        "namespace": ns,
                        "projectId": project_id,
                        "semanticCount": semantic_ranks.len(),
                        "ftsCount": fts_ranks.len(),
                        "top": scored
                            .iter()
                            .map(|(s, e)| serde_json::json!({
                                "id": e.id.to_string(),
                                "score": s,
                            }))
                            .collect::<Vec<_>>(),
                    }),
                    duration_ms: Some(duration_ms),
                },
            )
            .await;
            if !touched_ids.is_empty() {
                crate::debug::log(
                    &self.storage,
                    DebugLogInput {
                        event: DebugEvent::TouchPromotion,
                        entry_id: None,
                        data: serde_json::json!({
                            "entryIds": touched_ids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
                            "count": touched_ids.len(),
                        }),
                        duration_ms: None,
                    },
                )
                .await;
            }
        }

        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }

    async fn run_semantic_search(
        &self,
        query: &str,
        filters: SearchQuery,
    ) -> StorageResult<Vec<MemoryEntry>> {
        if !self.embedding.is_available() {
            return Ok(vec![]);
        }
        match self.embedding.embed(query).await {
            Some(embedding) => self.storage.semantic_search(&embedding, filters).await,
            None => Ok(vec![]),
        }
    }

    // ── Read ───────────────────────────────────────────────────

    pub async fn get(&self, id: Uuid) -> StorageResult<MemoryEntry> {
        self.storage.get(id).await
    }

    /// Metadata lookup — deliberately does not touch access counters or
    /// decay, so a topic_key probe never looks like a retrieval.
    pub async fn get_by_topic_key(
        &self,
        project_id: Option<&str>,
        topic_key: &str,
    ) -> StorageResult<Option<MemoryEntry>> {
        let ns = self.scope(None, project_id);
        self.storage.get_by_topic_key(ns, topic_key).await
    }

    /// Every live entry under a `topic_key` prefix, in key order.
    ///
    /// The read path for documents chunked across several entries — plans
    /// and icebox items. Like `get_by_topic_key` this is a metadata lookup
    /// and deliberately touches no access counters: assembling a document
    /// is not a retrieval of each of its parts.
    pub async fn list_by_topic_prefix(
        &self,
        project_id: Option<&str>,
        prefix: &str,
    ) -> StorageResult<Vec<MemoryEntry>> {
        let ns = self.scope(None, project_id);
        self.storage.list_by_topic_prefix(ns, prefix).await
    }

    pub async fn list(&self, filters: ListFilters) -> StorageResult<Vec<MemoryEntry>> {
        let mut f = filters;
        if f.namespace.is_none() {
            // Same read/write symmetry as `scope()`: project entries live in
            // namespace == project_id.
            f.namespace = f
                .project_id
                .clone()
                .or_else(|| Some(self.default_namespace.clone()));
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
        let injection_started = std::time::Instant::now();
        let ns = self.scope(namespace, project_id);

        let sessions = self.storage.list_sessions(ns, session_count * 2).await?;
        let recent_sessions: Vec<&Session> = sessions
            .iter()
            .filter(|s| s.status == SessionStatus::Completed)
            .take(session_count)
            .collect();

        // Over-fetch, drop captured input, then trim to what gets rendered.
        //
        // Two things this guards. First the 40-vs-`take(32)` mismatch, which
        // meant 20% of every logged injection slot named an entry the packet
        // never contained. Second, and larger: this packet is ordered by
        // recency, so without a type filter it fills with whatever was
        // written last — measured at 47% captured user prompts and 41%
        // auto-extracted diffs, leaving zero decisions, patterns or
        // architecture in a 12.8 KB payload sent to every session. Prompts
        // are the user's own words read back at them, and session-summary
        // entries duplicate the "Recent Sessions" block rendered above.
        let recent_entries: Vec<MemoryEntry> = self
            .storage
            .list(ListFilters {
                namespace: Some(ns.to_string()),
                project_id: project_id.map(|s| s.to_string()),
                limit: Some(CONTEXT_ENTRY_LIMIT * 4),
                ..Default::default()
            })
            .await?
            .into_iter()
            .filter(|e| !e.entry_type.excluded_from_injection())
            .take(CONTEXT_ENTRY_LIMIT)
            .collect();

        let stats = self.storage.get_stats(ns).await?;

        let formatted = format_context_packet(&recent_sessions, &recent_entries, &stats);

        if self.debug_enabled {
            crate::debug::log(
                &self.storage,
                DebugLogInput {
                    event: DebugEvent::Injection,
                    entry_id: None,
                    data: serde_json::json!({
                        "hookEvent": "SessionStart",
                        "projectId": project_id,
                        "namespace": ns,
                        "entryIds": recent_entries
                            .iter()
                            .map(|e| e.id.to_string())
                            .collect::<Vec<_>>(),
                        "entryCount": recent_entries.len(),
                        "sessionCount": recent_sessions.len(),
                        "formattedChars": formatted.chars().count(),
                    }),
                    duration_ms: Some(injection_started.elapsed().as_secs_f64() * 1000.0),
                },
            )
            .await;
        }

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

    /// Whole-database aggregate across every namespace. The scoped
    /// `get_stats` cannot see per-project data (namespace == project_id).
    pub async fn get_stats_global(&self) -> StorageResult<GlobalStats> {
        self.storage.get_stats_all().await
    }

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
    pub async fn merge_projects(&self, source: &str, target: &str) -> StorageResult<MergeCounts> {
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

    pub async fn recover_stale_observations(&self, older_than_secs: i64) -> StorageResult<i64> {
        self.storage
            .recover_stale_observations(older_than_secs)
            .await
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

    /// Replace an entry's tags in place, keeping its id, and push the row.
    ///
    /// The write path for state that lives in tags — a plan's status, a
    /// phase's progress. Re-saving through `propose` cannot do this: the
    /// exact-duplicate guard hashes title and content only, so a tags-only
    /// change short-circuits as `Duplicate` and the new tags are silently
    /// discarded. Advancing a phase would then appear to succeed and change
    /// nothing, which is the failure mode this whole feature exists to
    /// avoid.
    ///
    /// Keeping the id also keeps cross-references stable: an icebox item
    /// that names the plan it was promoted into must not acquire a new id
    /// every time its status moves.
    pub async fn retag(&self, id: Uuid, tags: Vec<String>) -> StorageResult<MemoryEntry> {
        let entry = self
            .storage
            .update(id, serde_json::json!({ "tags": tags }))
            .await?;
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

    // ── Two-stage GC + maintenance passthroughs ───────────────

    /// GC stage 1: soft-delete never-accessed crawl bulk older than
    /// `age_days` in the project's namespace.
    pub async fn gc_stage1(
        &self,
        project_id: Option<&str>,
        age_days: i64,
        max: usize,
        dry_run: bool,
    ) -> StorageResult<Vec<Uuid>> {
        let ns = self.ns(project_id);
        self.storage
            .soft_delete_stale_crawl(ns, age_days, max, dry_run)
            .await
    }

    /// GC stage 2: hard-purge tombstones older than `older_than_days`.
    pub async fn gc_purge(
        &self,
        namespace: Option<&str>,
        older_than_days: i64,
        max: usize,
        dry_run: bool,
    ) -> StorageResult<Vec<Uuid>> {
        self.storage
            .purge_soft_deleted(namespace, older_than_days, max, dry_run)
            .await
    }

    pub async fn update_entry(
        &self,
        id: Uuid,
        updates: serde_json::Value,
    ) -> StorageResult<MemoryEntry> {
        self.storage.update(id, updates).await
    }

    pub async fn list_missing_content_hash(
        &self,
        limit: usize,
    ) -> StorageResult<Vec<(Uuid, String, String)>> {
        self.storage.list_missing_content_hash(limit).await
    }

    pub async fn set_content_hash(&self, id: Uuid, hash: &str) -> StorageResult<()> {
        self.storage.set_content_hash(id, hash).await
    }

    pub async fn redact_entry_row(
        &self,
        id: Uuid,
        title: &str,
        content: &str,
        tags: &[String],
    ) -> StorageResult<()> {
        self.storage
            .redact_entry_row(id, title, content, tags)
            .await
    }

    pub async fn find_duplicate_clusters(
        &self,
        namespace: Option<&str>,
    ) -> StorageResult<Vec<DuplicateCluster>> {
        self.storage.find_duplicate_clusters(namespace).await
    }

    pub async fn list_namespaces(&self) -> StorageResult<Vec<String>> {
        Ok(self
            .storage
            .get_stats_all()
            .await?
            .by_namespace
            .into_iter()
            .map(|n| n.namespace)
            .collect())
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
            let mut tags: Vec<String> = vec!["auto-capture".into(), "key-learning".into()];
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

/// Entries carried by the SessionStart context packet. One constant so the
/// number fetched, the number rendered, and the number logged cannot drift.
pub const CONTEXT_ENTRY_LIMIT: usize = 32;

/// Render memories recalled for a specific user prompt.
///
/// Deliberately terser than the SessionStart packet: this fires once per
/// prompt, so it must earn its tokens on every single one.
pub fn format_recall_packet(entries: &[MemoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "## Muninn — relevant memories for this request".to_string(),
        String::new(),
    ];
    for e in entries {
        lines.push(format!(
            "- **{}** [{}]: {}",
            e.title,
            e.entry_type.as_str(),
            crate::text::truncate_ellipsis(&e.content, 300)
        ));
    }
    lines.push(String::new());
    lines.join("\n")
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
        for e in entries.iter().take(CONTEXT_ENTRY_LIMIT) {
            let max_chars = match e.entry_type {
                EntryType::Bug | EntryType::Decision => 400,
                EntryType::Pattern => 300,
                EntryType::Architecture => 250,
                EntryType::Session => e.content.chars().count(),
                _ => 200,
            };
            let summary = crate::text::truncate_ellipsis(&e.content, max_chars);
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
        .semantic_search(
            &query_emb,
            SearchQuery {
                limit: Some(10),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
        )
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

    /// Same, but keeping the storage handle so a test can read the
    /// outbox the librarian wrote to.
    async fn test_librarian_with_storage() -> (MemoryLibrarian, Arc<SqliteAdapter>) {
        let storage = Arc::new(SqliteAdapter::in_memory("test").unwrap());
        storage.initialize().await.unwrap();
        let embedding = Arc::new(DisabledEmbeddingProvider);
        let lib = MemoryLibrarian::new(storage.clone(), embedding, "test", None);
        (lib, storage)
    }

    /// Deletes never reached the outbox: `deprecate` deleted first and
    /// then asked `storage.get` for a payload, but `get` filters
    /// `deleted_at IS NULL`, so it returned NotFound and the enqueue
    /// bailed silently. Live dogfood DB: 3,946 outbox rows, every one an
    /// `insert`, zero `delete` — a remote drain would have resurrected
    /// every deduped and GC'd entry.
    #[tokio::test]
    async fn deprecate_enqueues_a_delete_op() {
        let _env = crate::test_support::with_env("RUNAR_STORAGE_LOCAL", "sqlite");
        let (lib, storage) = test_librarian_with_storage().await;

        let saved = lib
            .propose(MemoryEntryInput {
                title: "doomed".into(),
                content: "this entry gets retired".into(),
                entry_type: EntryType::Note,
                ..Default::default()
            })
            .await
            .unwrap();

        lib.deprecate(saved.id).await.unwrap();

        let rows = storage.claim_outbox(50, 10).await.unwrap();
        let ops: Vec<_> = rows
            .iter()
            .filter(|r| r.entry_id == saved.id)
            .map(|r| r.op_kind)
            .collect();
        assert!(
            ops.contains(&OutboxOp::Delete),
            "a soft-delete must enqueue a delete op, got {ops:?}"
        );

        let del = rows
            .iter()
            .find(|r| r.entry_id == saved.id && r.op_kind == OutboxOp::Delete)
            .unwrap();

        // The payload must survive exactly what `push_one` does to it.
        // An id-only stub passes every enqueue-side assertion and then
        // fails on the wire with "missing field `title`" — which is how
        // 200 tombstones dead-lettered against the live remote.
        let round_tripped: crate::types::MemoryEntry =
            serde_json::from_value(del.row_payload.clone())
                .expect("delete payload must deserialize as a full MemoryEntry");
        assert_eq!(round_tripped.id, saved.id);
        assert!(
            round_tripped.deleted_at.is_some(),
            "the tombstone must carry deleted_at, or the remote resolver \
             treats it as a live entry and the deletion never propagates"
        );
    }

    /// Outbox writes stay off unless hybrid mode is configured; a
    /// single-backend user should never accumulate a queue.
    #[tokio::test]
    async fn deprecate_writes_no_outbox_row_outside_hybrid_mode() {
        let _env = crate::test_support::with_env("RUNAR_STORAGE_LOCAL", "");
        std::env::remove_var("RUNAR_STORAGE_LOCAL");
        let (lib, storage) = test_librarian_with_storage().await;

        let saved = lib
            .propose(MemoryEntryInput {
                title: "local only".into(),
                content: "no sync configured".into(),
                entry_type: EntryType::Note,
                ..Default::default()
            })
            .await
            .unwrap();
        lib.deprecate(saved.id).await.unwrap();

        assert_eq!(storage.outbox_depth().await.unwrap(), 0);
    }

    #[test]
    fn bound_content_reports_what_it_dropped() {
        let (out, dropped) = bound_content("short", 100);
        assert_eq!(out, "short");
        assert_eq!(dropped, 0, "under the limit is untouched");

        let (out, dropped) = bound_content(&"x".repeat(500), 100);
        assert_eq!(dropped, 400);
        assert!(
            out.contains("[truncated 400 chars]"),
            "a bare ellipsis cannot distinguish a light clip from a gutting"
        );
        assert!(
            out.chars().count() <= 100,
            "the marker must fit INSIDE the budget, not overshoot it — got {}",
            out.chars().count()
        );
    }

    /// The repo has permanently broken a hook by measuring bytes against a
    /// char budget. Content is arbitrary user text.
    #[test]
    fn bound_content_counts_chars_not_bytes() {
        let cjk = "配置".repeat(50); // 100 chars, 300 bytes
        let (out, dropped) = bound_content(&cjk, 100);
        assert_eq!(dropped, 0, "100 chars is within a 100-char budget");
        assert_eq!(out, cjk);

        // And cutting mid-sequence must not panic.
        for n in 1..40 {
            let _ = bound_content("🐦‍⬛ raven with a ZWJ sequence", n);
        }
    }

    #[tokio::test]
    async fn oversized_prose_is_bounded_and_tagged() {
        let lib = test_librarian().await;
        let saved = lib
            .propose(MemoryEntryInput {
                title: "huge".into(),
                content: "y".repeat(50_000),
                entry_type: EntryType::Note,
                ..Default::default()
            })
            .await
            .unwrap();

        let entry = lib.get(saved.id).await.unwrap();
        assert!(
            entry.content.chars().count() <= content_limit(),
            "stored content must satisfy the remote's CHECK, got {}",
            entry.content.chars().count()
        );
        assert!(
            entry.tags.iter().any(|t| t == "truncated"),
            "must be marked"
        );
    }

    /// The hazard this design exists to avoid. Crawl state is a JSON blob that
    /// `git::deserialize_state` parses back with `serde_json::from_str(..).ok()`
    /// — truncating it does not error, it returns None, which reads as "no
    /// previous state" and silently downgrades every crawl to a full one.
    #[tokio::test]
    async fn exact_content_survives_whole_so_crawl_state_still_parses() {
        let lib = test_librarian().await;
        // A realistic shape: one big JSON object, far over the limit.
        let hashes: String = (0..4000)
            .map(|i| format!("\"src/f{i}.rs\":\"{i:016x}\""))
            .collect::<Vec<_>>()
            .join(",");
        let blob = format!("{{\"project_id\":\"p\",\"file_hashes\":{{{hashes}}}}}");
        assert!(blob.chars().count() > content_limit() * 3);

        let saved = lib
            .propose(MemoryEntryInput {
                title: "Crawl state: p".into(),
                content: blob.clone(),
                entry_type: EntryType::Context,
                exact_content: true,
                ..Default::default()
            })
            .await
            .unwrap();

        let entry = lib.get(saved.id).await.unwrap();
        assert_eq!(entry.content, blob, "must round-trip byte for byte");
        assert!(
            serde_json::from_str::<serde_json::Value>(&entry.content).is_ok(),
            "must still be parseable JSON — this is the whole point"
        );
        assert!(
            !entry.tags.iter().any(|t| t == "truncated"),
            "exempt entries are not clipped, so must not claim to be"
        );
    }

    /// An exempt entry over the limit cannot satisfy the remote's CHECK, so
    /// queueing it would only feed the dead-letter queue.
    #[tokio::test]
    async fn oversized_exact_content_is_not_queued_for_sync() {
        let _env = crate::test_support::with_env("RUNAR_STORAGE_LOCAL", "sqlite");
        let (lib, storage) = test_librarian_with_storage().await;

        lib.propose(MemoryEntryInput {
            title: "Crawl state: p".into(),
            content: "z".repeat(50_000),
            entry_type: EntryType::Context,
            exact_content: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(
            storage.outbox_depth().await.unwrap(),
            0,
            "an unsyncable entry must not be queued"
        );

        // A bounded entry of the same origin still syncs normally.
        lib.propose(MemoryEntryInput {
            title: "normal".into(),
            content: "z".repeat(50_000),
            entry_type: EntryType::Context,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(storage.outbox_depth().await.unwrap(), 1);
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
            injected_count: 0,
            last_injected_at: None,
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
            .search(
                "session summary",
                10,
                Some("demo"),
                Some("demo"),
                None,
                None,
            )
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

        assert_eq!(
            results.len(),
            3,
            "expected 3 learnings, got {}",
            results.len()
        );
        for r in &results {
            assert_eq!(r.action, SaveAction::Created);
        }

        // Second call with identical text short-circuits on content hash:
        // no new rows, no topic_key delete-and-reinsert churn, existing ids
        // are returned.
        let second = lib
            .capture_passive(text, Some("proj-a2"), Some(EntryType::Note), &[])
            .await
            .unwrap();
        assert_eq!(second.len(), 3);
        assert!(
            second
                .iter()
                .all(|r| matches!(r.action, SaveAction::Duplicate)),
            "repeated identical capture should be reported as duplicate"
        );
        let first_ids: std::collections::HashSet<Uuid> = results.iter().map(|r| r.id).collect();
        assert!(
            second.iter().all(|r| first_ids.contains(&r.id)),
            "duplicates must point at the original rows"
        );
    }

    #[tokio::test]
    async fn test_propose_redacts_private_content() {
        let lib = test_librarian().await;

        let result = lib
            .propose(MemoryEntryInput {
                title: "API token rotation".into(),
                content: "Rotate via <private>sk-live-abc123</private> endpoint weekly.".into(),
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
    async fn end_session_persists_goal_and_discoveries() {
        let lib = test_librarian().await;
        let session = lib
            .propose_session(SessionInput {
                goal: Some("Auto-started session".into()),
                project_id: Some("proj_s".into()),
                tool: Some("test".into()),
            })
            .await
            .unwrap();

        lib.end_session(
            session.id,
            SessionSummary {
                summary: "shipped the retry fix".into(),
                goal: Some("fix the flaky retry logic".into()),
                discoveries: vec![
                    "timeout was 1s too short".into(),
                    "jitter missing on retries".into(),
                ],
                files_modified: vec!["src/retry.rs".into()],
                ..Default::default()
            },
            Some("proj_s"),
        )
        .await
        .unwrap();

        let stored = lib.get_session(session.id).await.unwrap();
        assert_eq!(stored.goal.as_deref(), Some("fix the flaky retry logic"));
        assert_eq!(stored.discoveries.len(), 2);
        assert_eq!(stored.files_modified, vec!["src/retry.rs".to_string()]);
        assert_eq!(stored.status, SessionStatus::Completed);
    }

    #[tokio::test]
    async fn debug_events_written_only_when_enabled() {
        // Disabled (default): no rows.
        let lib = test_librarian().await;
        lib.propose(MemoryEntryInput {
            title: "observable entry".into(),
            content: "observable content".into(),
            entry_type: EntryType::Note,
            ..Default::default()
        })
        .await
        .unwrap();
        lib.search("observable", 5, None, None, None, None)
            .await
            .unwrap();
        let rows = lib.query_debug_log(DebugLogQuery::default()).await.unwrap();
        assert!(rows.is_empty(), "no telemetry without RUNAR_DEBUG");

        // Enabled: search + context produce events with durations.
        let lib = test_librarian().await.with_debug(true);
        lib.propose(MemoryEntryInput {
            title: "observable entry".into(),
            content: "observable content".into(),
            entry_type: EntryType::Note,
            project_id: Some("proj_dbg".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        lib.search("observable", 5, None, Some("proj_dbg"), None, None)
            .await
            .unwrap();
        lib.get_context(None, Some("proj_dbg"), 3).await.unwrap();

        let scoring = lib
            .query_debug_log(DebugLogQuery {
                event: Some(DebugEvent::SearchScoring),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(scoring.len(), 1);
        assert!(scoring[0].duration_ms.is_some());
        assert_eq!(scoring[0].data["namespace"], "proj_dbg");

        let touches = lib
            .query_debug_log(DebugLogQuery {
                event: Some(DebugEvent::TouchPromotion),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(touches.len(), 1);
        assert_eq!(touches[0].data["count"], 1);

        let injections = lib
            .query_debug_log(DebugLogQuery {
                event: Some(DebugEvent::Injection),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].data["entryCount"], 1);
        assert!(injections[0].duration_ms.is_some());
    }

    #[tokio::test]
    async fn test_search_scopes_to_project_namespace() {
        let lib = test_librarian().await;

        lib.propose(MemoryEntryInput {
            title: "gateway timeout policy for proj_a".into(),
            content: "retries use exponential backoff with jitter".into(),
            entry_type: EntryType::Decision,
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        // Project-scoped search finds the entry written to namespace proj_a.
        let hits = lib
            .search(
                "gateway timeout backoff",
                10,
                None,
                Some("proj_a"),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "proj_a search should find its own entry");

        // A different project sees nothing.
        let hits = lib
            .search(
                "gateway timeout backoff",
                10,
                None,
                Some("proj_b"),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(hits.is_empty(), "proj_b must not see proj_a entries");

        // An unscoped (default-namespace) search must not leak project rows.
        let hits = lib
            .search("gateway timeout backoff", 10, None, None, None, None)
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "default-namespace search must not leak proj_a rows"
        );
    }

    #[tokio::test]
    async fn test_list_uses_project_id_as_namespace() {
        let lib = test_librarian().await;

        lib.propose(MemoryEntryInput {
            title: "proj_a architecture".into(),
            content: "modular monolith".into(),
            entry_type: EntryType::Architecture,
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let rows = lib
            .list(ListFilters {
                project_id: Some("proj_a".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "list with project_id should resolve namespace=proj_a"
        );
    }

    #[tokio::test]
    async fn recall_skips_captured_prompts_and_returns_knowledge() {
        let lib = test_librarian().await;

        lib.propose(MemoryEntryInput {
            title: "Pattern: authentication-flow".into(),
            content: "JWT issued at login, refreshed by the gateway".into(),
            entry_type: EntryType::Pattern,
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        // The same text the user just typed, captured moments earlier. This
        // is the highest-similarity row in the corpus and must never come
        // back as a "memory" — the arm would be echoing the prompt at itself.
        lib.propose(MemoryEntryInput {
            title: "how does authentication work".into(),
            content: "how does authentication work".into(),
            entry_type: EntryType::UserPrompt,
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let hits = lib
            .recall_for_prompt("how does authentication work", 8, Some("proj_a"))
            .await
            .unwrap();

        assert!(!hits.is_empty(), "the pattern entry should be recalled");
        assert!(
            hits.iter().all(|e| e.entry_type != EntryType::UserPrompt),
            "captured prompts are input, not knowledge: {:?}",
            hits.iter().map(|e| e.entry_type).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn recall_counts_as_injection_not_retrieval() {
        let lib = test_librarian().await;

        lib.propose(MemoryEntryInput {
            title: "Bug: connection pool exhaustion".into(),
            content: "pool size 5 was too small under load".into(),
            entry_type: EntryType::Bug,
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let hits = lib
            .recall_for_prompt("connection pool exhaustion", 8, Some("proj_a"))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);

        let entry = lib.get(hits[0].id).await.unwrap();
        assert_eq!(entry.injected_count, 1, "injection must be recorded");
        assert!(entry.last_injected_at.is_some());
        assert_eq!(
            entry.access_count, 0,
            "automatic recall must not inflate access_count — it fires on \
             every prompt and would pin the same rows past the citation \
             threshold within hours"
        );
        assert!(entry.last_accessed_at.is_none());
    }

    #[tokio::test]
    async fn test_get_context_serves_project_entries_and_sessions() {
        let lib = test_librarian().await;

        lib.propose(MemoryEntryInput {
            title: "proj_a context entry".into(),
            content: "important background".into(),
            entry_type: EntryType::Context,
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let session = lib
            .propose_session(SessionInput {
                goal: Some("build the thing".into()),
                project_id: Some("proj_a".into()),
                tool: Some("test".into()),
            })
            .await
            .unwrap();
        lib.end_session(
            session.id,
            SessionSummary {
                summary: "did the thing".into(),
                ..Default::default()
            },
            Some("proj_a"),
        )
        .await
        .unwrap();

        let packet = lib.get_context(None, Some("proj_a"), 3).await.unwrap();
        // The session-end entry is deliberately absent: it duplicates the
        // "Recent Sessions" block, which is rendered from the sessions table.
        assert_eq!(
            packet.recent_entries.len(),
            1,
            "the context entry surfaces; the session-end entry does not"
        );
        assert_eq!(
            packet.recent_entries[0].entry_type,
            EntryType::Context,
            "curated knowledge, not captured input"
        );
        assert_eq!(
            packet.recent_sessions.len(),
            1,
            "completed session should surface"
        );
        assert!(!packet.formatted.is_empty());
    }

    #[tokio::test]
    async fn context_packet_excludes_captured_input() {
        let lib = test_librarian().await;

        // Recency alone fills this packet with whatever was written last.
        // Measured on a real corpus that was 47% captured prompts and 41%
        // auto-extracted diffs, leaving no decisions or architecture at all.
        for i in 0..40 {
            lib.propose(MemoryEntryInput {
                title: format!("typed prompt {i}"),
                content: format!("what does the {i}th thing do"),
                entry_type: EntryType::UserPrompt,
                project_id: Some("proj_a".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        lib.propose(MemoryEntryInput {
            title: "Decision: use RRF for fusion".into(),
            content: "reciprocal rank fusion beat naive score addition".into(),
            entry_type: EntryType::Decision,
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let packet = lib.get_context(None, Some("proj_a"), 3).await.unwrap();

        assert!(
            packet
                .recent_entries
                .iter()
                .all(|e| e.entry_type != EntryType::UserPrompt),
            "prompts are the user's own words read back at them"
        );
        assert!(
            packet
                .recent_entries
                .iter()
                .any(|e| e.entry_type == EntryType::Decision),
            "the decision must survive 40 newer prompts"
        );
    }

    #[tokio::test]
    async fn context_packet_excludes_plans_and_icebox_items() {
        let lib = test_librarian().await;

        // A plan describes work that has not happened. Injected into a
        // session packet it reads as a statement about the code, which is
        // the same failure as a stale code graph: confident and wrong.
        for i in 0..10 {
            lib.propose(MemoryEntryInput {
                title: format!("Plan section {i}"),
                content: format!("phase {i}: rewrite the auth middleware"),
                entry_type: EntryType::Plan,
                topic_key: Some(format!("plan:auth:{i:02}-phase")),
                project_id: Some("proj_a".into()),
                ..Default::default()
            })
            .await
            .unwrap();
            lib.propose(MemoryEntryInput {
                title: format!("Icebox item {i}"),
                content: format!("someday: refactor thing {i}"),
                entry_type: EntryType::Icebox,
                topic_key: Some(format!("icebox:thing-{i}")),
                project_id: Some("proj_a".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        lib.propose(MemoryEntryInput {
            title: "Decision: bound content on the write path".into(),
            content: "propose truncates after redaction so a secret cannot be halved".into(),
            entry_type: EntryType::Decision,
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let packet = lib.get_context(None, Some("proj_a"), 3).await.unwrap();

        assert!(
            packet
                .recent_entries
                .iter()
                .all(|e| e.entry_type != EntryType::Plan && e.entry_type != EntryType::Icebox),
            "intended future work must never reach a session packet"
        );
        assert!(
            packet
                .recent_entries
                .iter()
                .any(|e| e.entry_type == EntryType::Decision),
            "the decision must survive 20 newer plan/icebox entries"
        );
    }

    #[tokio::test]
    async fn recall_excludes_plans_and_icebox_items() {
        let lib = test_librarian().await;

        // Same words as the prompt below, so relevance ranking alone would
        // put these first — exclusion has to be by type, not by score.
        lib.propose(MemoryEntryInput {
            title: "Plan: sync outbox dead lettering".into(),
            content: "phase one adds a reaper for stale outbox claims".into(),
            entry_type: EntryType::Plan,
            topic_key: Some("plan:outbox:00-reaper".into()),
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        lib.propose(MemoryEntryInput {
            title: "Icebox: sync outbox dead lettering".into(),
            content: "outbox claims can go stale and wedge the queue".into(),
            entry_type: EntryType::Icebox,
            topic_key: Some("icebox:outbox-dead-lettering".into()),
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        lib.propose(MemoryEntryInput {
            title: "Bug: stale outbox claims were never released".into(),
            content: "claim_outbox stamped claimed_at on rows coalescing then dropped".into(),
            entry_type: EntryType::Bug,
            project_id: Some("proj_a".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let recalled = lib
            .recall_for_prompt("stale outbox claims dead lettering", 8, Some("proj_a"))
            .await
            .unwrap();

        assert!(
            recalled
                .iter()
                .all(|e| e.entry_type != EntryType::Plan && e.entry_type != EntryType::Icebox),
            "recall must not serve plans or icebox items, however relevant"
        );
        assert!(
            recalled.iter().any(|e| e.entry_type == EntryType::Bug),
            "the recorded bug is what recall exists to surface"
        );
    }

    #[tokio::test]
    async fn test_propose_redacts_secret_patterns() {
        let lib = test_librarian().await;

        let result = lib
            .propose(MemoryEntryInput {
                title: "Deploy config".into(),
                content: "set TENANT_DB_PASS=Sup3rS3cret99 then push with \
                          ghp_abcdefghijklmnopqrstuvwxyz0123456789"
                    .into(),
                entry_type: EntryType::Note,
                ..Default::default()
            })
            .await
            .unwrap();

        let entry = lib.get(result.id).await.unwrap();
        assert!(
            !entry.content.contains("Sup3rS3cret99"),
            "{}",
            entry.content
        );
        assert!(!entry.content.contains("ghp_abcdef"), "{}", entry.content);
        assert!(entry.content.contains("[REDACTED:keyed-secret]"));
        assert!(entry.content.contains("[REDACTED:github-token]"));
        assert!(
            entry.tags.iter().any(|t| t == "redacted:secret"),
            "expected `redacted:secret` tag, got {:?}",
            entry.tags
        );
    }

    #[tokio::test]
    async fn test_user_prompt_downranked_in_search() {
        let lib = test_librarian().await;

        // Near-identical content, two types (distinct enough to survive the
        // content-hash dedup) — the curated Note must outrank the raw
        // UserPrompt at comparable FTS relevance.
        lib.propose(MemoryEntryInput {
            title: "checkout flow retries payment gateway".into(),
            content: "checkout flow retries payment gateway on timeout, prompt variant".into(),
            entry_type: EntryType::UserPrompt,
            ..Default::default()
        })
        .await
        .unwrap();
        lib.propose(MemoryEntryInput {
            title: "checkout flow retries payment gateway".into(),
            content: "checkout flow retries payment gateway on timeout, curated variant".into(),
            entry_type: EntryType::Note,
            ..Default::default()
        })
        .await
        .unwrap();

        let results = lib
            .search("checkout payment gateway", 10, None, None, None, None)
            .await
            .unwrap();
        assert!(
            results.len() >= 2,
            "expected both entries, got {}",
            results.len()
        );
        assert_eq!(
            results[0].entry_type,
            EntryType::Note,
            "curated Note should outrank UserPrompt"
        );
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
            injected_count: 0,
            last_injected_at: None,
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
            transitions
                .iter()
                .any(|t| t.id == result.id && t.new_layer == MemoryLayer::EPISODIC),
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
        assert!(
            !evicted.contains(&protected.id),
            "verified must never evict"
        );

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
        assert!(dest
            .import_entry(src.get(a.id).await.unwrap())
            .await
            .unwrap());
        assert!(dest
            .import_entry(src.get(b.id).await.unwrap())
            .await
            .unwrap());
        assert!(dest.import_edge(edge.clone()).await.unwrap());
        assert!(
            !dest.import_edge(edge).await.unwrap(),
            "duplicate edge skip"
        );

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

    /// A representative bare credential, assembled at runtime: written as a
    /// literal it is a well-formed key to any secret scanner, and GitHub push
    /// protection rejects the push. The regex sees the joined string either
    /// way.
    fn leak() -> String {
        format!("{}{}", "sk-ant-", "api03-x7Kq2mNp8vRt4wLs9dFg1hJk3nBc5yZa")
    }

    /// A caller-supplied topic key reached storage verbatim: `propose` scrubbed
    /// the title, content and tags but never the key, and the key is stored,
    /// FTS-adjacent and echoed back in the save response.
    #[tokio::test]
    async fn a_secret_in_a_caller_supplied_topic_key_is_scrubbed() {
        let leak = leak();
        let lib = test_librarian().await;
        lib.propose(MemoryEntryInput {
            title: "deploy notes".into(),
            content: "body".into(),
            topic_key: Some(format!("auth:{leak}")),
            ..Default::default()
        })
        .await
        .unwrap();

        let all = lib
            .list(ListFilters {
                limit: Some(50),
                ..Default::default()
            })
            .await
            .unwrap();
        let key = all[0].topic_key.clone().expect("topic key was stored");
        assert!(!key.contains(&leak), "secret survived in topic key: {key}");
        assert!(key.contains("[REDACTED:anthropic-key]"), "{key}");
    }

    /// The session goal went straight to `storage.create_session` unscrubbed,
    /// and it is replayed into the next session's context packet.
    #[tokio::test]
    async fn a_secret_in_a_session_goal_is_scrubbed() {
        let leak = leak();
        let lib = test_librarian().await;
        lib.propose_session(SessionInput {
            goal: Some(format!("wire up deploys with {leak}")),
            project_id: Some("p".into()),
            tool: None,
        })
        .await
        .unwrap();

        let session = lib
            .get_active_session(Some("p"))
            .await
            .unwrap()
            .expect("session exists");
        let goal = session.goal.expect("goal stored");
        assert!(!goal.contains(&leak), "secret survived in goal: {goal}");
        assert!(goal.contains("[REDACTED:anthropic-key]"), "{goal}");
    }

    /// The session-summary entry bypasses `propose` entirely, so its title and
    /// tags — both built from the caller-supplied project id — were never
    /// scrubbed even though the summary body was.
    #[tokio::test]
    async fn a_secret_in_the_session_summary_title_is_scrubbed() {
        let leak = leak();
        let lib = test_librarian().await;
        let session = lib
            .propose_session(SessionInput {
                goal: None,
                project_id: Some(format!("proj-{leak}")),
                tool: None,
            })
            .await
            .unwrap();
        lib.end_session(
            session.id,
            SessionSummary {
                summary: "done".into(),
                ..Default::default()
            },
            Some(&format!("proj-{leak}")),
        )
        .await
        .unwrap();

        // Project entries live in namespace == project_id, so the read has to
        // name it or it looks at the default namespace and finds nothing.
        let entries = lib
            .list(ListFilters {
                project_id: Some(format!("proj-{leak}")),
                limit: Some(50),
                ..Default::default()
            })
            .await
            .unwrap();
        let summary = entries
            .iter()
            .find(|e| e.entry_type == EntryType::Session)
            .expect("session summary entry");
        assert!(
            !summary.title.contains(&leak),
            "secret survived in title: {}",
            summary.title
        );
        assert!(
            !summary.tags.iter().any(|t| t.contains(&leak)),
            "secret survived in tags: {:?}",
            summary.tags
        );
    }
}
