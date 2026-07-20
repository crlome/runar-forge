//! One-time maintenance operations, surfaced as `runar scrub` and
//! `runar dedup`. Both default to reporting; destructive/rewriting paths
//! require the caller to drop `--dry-run` explicitly.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::librarian::MemoryLibrarian;
use crate::redact;
use crate::storage::content_hash;
use crate::types::ListFilters;

pub struct ScrubReport {
    pub scanned: usize,
    pub rewritten: usize,
    pub hits_by_kind: BTreeMap<String, usize>,
    /// Entries where redacted spans dominate the content — likely
    /// pure-credential rows worth soft-deleting outright.
    pub heavy_hit_ids: Vec<(Uuid, String)>,
}

/// Scan live entries for secret patterns and rewrite the hits in place
/// (title + content). The rewrite path goes through `update_entry`, which
/// re-indexes FTS via the guarded triggers and refreshes `content_hash`.
/// Scrub never deletes; `heavy_hit_ids` are reported for the user to act on.
pub async fn run_scrub(
    lib: &MemoryLibrarian,
    project: Option<&str>,
    dry_run: bool,
    limit: usize,
) -> anyhow::Result<ScrubReport> {
    let namespaces: Vec<String> = match project {
        Some(p) => vec![p.to_string()],
        None => lib.list_namespaces().await?,
    };

    let mut report = ScrubReport {
        scanned: 0,
        rewritten: 0,
        hits_by_kind: BTreeMap::new(),
        heavy_hit_ids: Vec::new(),
    };

    const BATCH: usize = 500;
    'outer: for ns in &namespaces {
        let mut offset = 0usize;
        loop {
            let batch = lib
                .list(ListFilters {
                    namespace: Some(ns.clone()),
                    limit: Some(BATCH),
                    offset: Some(offset),
                    // Tombstones carry pre-fix secrets too (topic_key
                    // supersession + dedup losers); scrub them as well.
                    include_deleted: true,
                    ..Default::default()
                })
                .await?;
            if batch.is_empty() {
                break;
            }
            offset += batch.len();

            for entry in batch {
                if report.scanned >= limit {
                    break 'outer;
                }
                report.scanned += 1;

                let (new_title, title_hits) = redact::redact_secrets(&entry.title);
                let (new_content, content_hits) = redact::redact_secrets(&entry.content);
                let total: usize = title_hits
                    .iter()
                    .chain(content_hits.iter())
                    .map(|h| h.count)
                    .sum();
                if total == 0 {
                    continue;
                }

                for hit in title_hits.iter().chain(content_hits.iter()) {
                    *report.hits_by_kind.entry(hit.kind.to_string()).or_insert(0) += hit.count;
                }

                // Redacted spans dominating the content = the row IS the
                // secret; flag for deletion rather than silently keeping a
                // husk of [REDACTED] markers.
                let redacted_chars = new_content.matches("[REDACTED:").count() * 20;
                if !entry.content.is_empty() && redacted_chars * 2 > entry.content.len() {
                    report.heavy_hit_ids.push((entry.id, entry.title.clone()));
                }

                if !dry_run {
                    let mut tags = entry.tags.clone();
                    // Tags are FTS-indexed and caller-supplied — scrub them too.
                    for tag in tags.iter_mut() {
                        let (clean, hits) = redact::redact_secrets(tag);
                        if !hits.is_empty() {
                            *tag = clean;
                        }
                    }
                    if !tags.iter().any(|t| t == "redacted:secret") {
                        tags.push("redacted:secret".to_string());
                    }
                    // redact_entry_row (not update_entry): reaches soft-deleted
                    // rows and binds tags portably on both backends.
                    lib.redact_entry_row(entry.id, &new_title, &new_content, &tags)
                        .await?;
                    report.rewritten += 1;
                }
            }
        }
    }

    Ok(report)
}

#[allow(clippy::struct_field_names)]
pub struct DedupReport {
    pub backfilled: usize,
    pub clusters: usize,
    pub redundant: usize,
    pub soft_deleted: usize,
}

/// Pass 1: backfill `content_hash` for rows created before migration 013.
/// Pass 2: cluster live rows on (namespace, content_hash); keep the most
/// valuable member (highest access_count, then verified, then newest) and
/// soft-delete the rest — recoverable until `runar gc --hard` purges them.
pub async fn run_dedup(
    lib: &MemoryLibrarian,
    project: Option<&str>,
    dry_run: bool,
    backfill_only: bool,
) -> anyhow::Result<DedupReport> {
    let mut report = DedupReport {
        backfilled: 0,
        clusters: 0,
        redundant: 0,
        soft_deleted: 0,
    };

    // Pass 1 — backfill. Runs even under --dry-run: hashes are metadata,
    // not a destructive change, and cluster detection needs them.
    loop {
        let batch = lib.list_missing_content_hash(500).await?;
        if batch.is_empty() {
            break;
        }
        for (id, title, content) in batch {
            let hash = content_hash(&title, &content);
            lib.set_content_hash(id, &hash).await?;
            report.backfilled += 1;
        }
    }

    if backfill_only {
        return Ok(report);
    }

    // Pass 2 — cluster + soft-delete losers.
    let clusters = lib.find_duplicate_clusters(project).await?;
    report.clusters = clusters.len();
    for cluster in &clusters {
        // Keeper: highest access_count, tie-break verified, then newest.
        let keeper = cluster
            .entries
            .iter()
            .max_by(|a, b| {
                a.access_count
                    .cmp(&b.access_count)
                    .then(a.verified.cmp(&b.verified))
                    .then(a.created_at.cmp(&b.created_at))
            })
            .map(|m| m.id);
        for member in &cluster.entries {
            if Some(member.id) == keeper {
                continue;
            }
            report.redundant += 1;
            if !dry_run {
                lib.deprecate(member.id).await?;
                report.soft_deleted += 1;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::embedding::DisabledEmbeddingProvider;
    use crate::storage::sqlite::SqliteAdapter;
    use crate::storage::MemoryStorage;
    use crate::types::{EntryType, MemoryEntryInput};

    async fn lib() -> MemoryLibrarian {
        let storage = Arc::new(SqliteAdapter::in_memory("test").unwrap());
        storage.initialize().await.unwrap();
        MemoryLibrarian::new(storage, Arc::new(DisabledEmbeddingProvider), "test", None)
    }

    #[tokio::test]
    async fn scrub_dry_run_reports_without_rewriting() {
        let lib = lib().await;
        // Bypass propose (which redacts) via update_entry after a clean save,
        // simulating pre-redaction legacy rows.
        let r = lib
            .propose(MemoryEntryInput {
                title: "legacy entry".into(),
                content: "placeholder".into(),
                entry_type: EntryType::Note,
                ..Default::default()
            })
            .await
            .unwrap();
        lib.update_entry(
            r.id,
            serde_json::json!({"content": "export TENANT_DB_PASS=Sup3rS3cret99 && deploy"}),
        )
        .await
        .unwrap();

        let report = run_scrub(&lib, None, true, 1000).await.unwrap();
        assert_eq!(report.hits_by_kind.get("keyed-secret"), Some(&1));
        assert_eq!(report.rewritten, 0);
        let entry = lib.get(r.id).await.unwrap();
        assert!(entry.content.contains("Sup3rS3cret99"), "dry-run must not mutate");

        let report = run_scrub(&lib, None, false, 1000).await.unwrap();
        assert_eq!(report.rewritten, 1);
        let entry = lib.get(r.id).await.unwrap();
        assert!(!entry.content.contains("Sup3rS3cret99"));
        assert!(entry.content.contains("[REDACTED:keyed-secret]"));
        assert!(entry.tags.iter().any(|t| t == "redacted:secret"));

        // The secret no longer matches in FTS.
        let hits = lib
            .search("Sup3rS3cret99", 10, None, None, None, None)
            .await
            .unwrap();
        assert!(hits.is_empty(), "redacted secret must leave the index");
    }

    #[tokio::test]
    async fn dedup_backfills_and_collapses_clusters() {
        let lib = lib().await;

        // Legacy duplicates: bypass save's dedup by clearing content_hash
        // and re-pointing them at identical content via raw-ish updates.
        let mut ids = Vec::new();
        for i in 0..3 {
            let r = lib
                .propose(MemoryEntryInput {
                    title: format!("distinct-{i}"),
                    content: format!("distinct content {i}"),
                    entry_type: EntryType::Note,
                    ..Default::default()
                })
                .await
                .unwrap();
            ids.push(r.id);
        }
        // Make the first two identical — update_entry recomputes their
        // hashes, so they now form an exact-duplicate cluster (mirrors a
        // legacy corpus where identical rows predate write-time dedup).
        for id in &ids[..2] {
            lib.update_entry(
                *id,
                serde_json::json!({"title": "same title", "content": "same content"}),
            )
            .await
            .unwrap();
        }

        // Give one of the dupes an access so keeper choice is observable.
        lib.update_entry(ids[0], serde_json::json!({"access_count": 5}))
            .await
            .unwrap();

        let report = run_dedup(&lib, None, true, false).await.unwrap();
        assert_eq!(report.clusters, 1);
        assert_eq!(report.redundant, 1);
        assert_eq!(report.soft_deleted, 0, "dry run deletes nothing");

        let report = run_dedup(&lib, None, false, false).await.unwrap();
        assert_eq!(report.soft_deleted, 1);
        assert!(lib.get(ids[0]).await.is_ok(), "most-accessed keeper survives");
        assert!(lib.get(ids[1]).await.is_err(), "loser soft-deleted");
        assert!(lib.get(ids[2]).await.is_ok(), "unique row untouched");
    }
}
