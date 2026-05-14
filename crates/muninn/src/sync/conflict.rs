//! Conflict resolver — pure function deciding what happens when an
//! incoming sync row meets an existing local row.
//!
//! Phase 5.6.2 ships this for the push side. Phase 5.6.3 reuses the
//! same resolver for the pull side (just inverts which side is
//! "incoming"). Keep it pure — no I/O, no time, no env reads.

use chrono::{DateTime, Utc};

use crate::types::{
    ApplyOutcome, ConflictDirection, ConflictPolicy, ConflictWinner, MemoryEntry, SyncConflict,
};

/// Decision returned by the resolver. The caller (`apply_remote_entry`
/// implementor) translates this into a concrete write + optional
/// conflict-audit row.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// No existing local row — insert as-is.
    Insert,
    /// Incoming wins by LWW or by being newly verified — overwrite local.
    Update { policy: ConflictPolicy, audit: bool },
    /// Existing local wins — leave untouched, possibly write audit row.
    Skip { policy: ConflictPolicy, audit: bool },
}

impl Resolution {
    pub fn outcome(&self) -> ApplyOutcome {
        match self {
            Self::Insert => ApplyOutcome::Inserted,
            Self::Update { audit: true, .. } => ApplyOutcome::ConflictRecorded,
            Self::Update { .. } => ApplyOutcome::UpdatedLww,
            Self::Skip { audit: true, .. } => ApplyOutcome::ConflictRecorded,
            Self::Skip { .. } => ApplyOutcome::SkippedNewerLocal,
        }
    }
}

/// Resolution table:
///
/// | existing                | incoming                     | result                          |
/// |-------------------------|------------------------------|---------------------------------|
/// | None                    | any                          | Insert                          |
/// | soft-deleted            | not deleted                  | Skip (resurrect-blocked, audit) |
/// | not deleted             | soft-deleted                 | Update (soft-delete-wins)       |
/// | verified                | unverified                   | Skip (verified-wins, audit)     |
/// | unverified              | verified                     | Update (verified-wins)          |
/// | both verified or both not, incoming.updated_at > existing.updated_at | Update (lww) |
/// | both verified or both not, incoming.updated_at <= existing.updated_at | Skip (lww) |
pub fn resolve(existing: Option<&MemoryEntry>, incoming: &MemoryEntry) -> Resolution {
    let Some(existing) = existing else {
        return Resolution::Insert;
    };

    let existing_deleted = existing.deleted_at.is_some();
    let incoming_deleted = incoming.deleted_at.is_some();

    // Soft-delete propagation rules — these win before LWW because a
    // tombstone shouldn't be silently undone by a stale update.
    if existing_deleted && !incoming_deleted {
        // Resurrection requires a manual verify; block it and audit.
        return Resolution::Skip {
            policy: ConflictPolicy::ResurrectBlocked,
            audit: true,
        };
    }
    if !existing_deleted && incoming_deleted {
        return Resolution::Update {
            policy: ConflictPolicy::SoftDeleteWins,
            audit: false,
        };
    }

    // Verified flag wins regardless of timestamp. Audit so the user can
    // see when an unverified edit lost to a verified row.
    if existing.verified && !incoming.verified {
        return Resolution::Skip {
            policy: ConflictPolicy::VerifiedWins,
            audit: true,
        };
    }
    if !existing.verified && incoming.verified {
        return Resolution::Update {
            policy: ConflictPolicy::VerifiedWins,
            audit: false,
        };
    }

    // Same verified state and same delete state — fall through to LWW.
    if incoming.updated_at > existing.updated_at {
        Resolution::Update {
            policy: ConflictPolicy::Lww,
            audit: false,
        }
    } else {
        Resolution::Skip {
            policy: ConflictPolicy::Lww,
            audit: false,
        }
    }
}

/// Build a `SyncConflict` audit row from a resolved decision. Caller
/// supplies the direction (push or pull) and which side won.
pub fn build_audit(
    direction: ConflictDirection,
    policy: ConflictPolicy,
    winner: ConflictWinner,
    local: Option<&MemoryEntry>,
    remote: Option<&MemoryEntry>,
) -> SyncConflict {
    fn payload(e: Option<&MemoryEntry>) -> Option<serde_json::Value> {
        e.and_then(|x| serde_json::to_value(x).ok())
    }
    fn ts(e: Option<&MemoryEntry>) -> Option<DateTime<Utc>> {
        e.map(|x| x.updated_at)
    }
    SyncConflict {
        id: uuid::Uuid::new_v4(),
        entry_id: local.map(|e| e.id).or_else(|| remote.map(|e| e.id)).unwrap(),
        direction,
        policy,
        winner_side: winner,
        local_updated_at: ts(local),
        remote_updated_at: ts(remote),
        local_payload: payload(local),
        remote_payload: payload(remote),
        created_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EntryType, MemoryLayer, MemorySource};
    use chrono::Duration;

    fn entry(updated: DateTime<Utc>, verified: bool, deleted: bool) -> MemoryEntry {
        MemoryEntry {
            id: uuid::Uuid::new_v4(),
            namespace: "test".into(),
            title: "T".into(),
            content: "C".into(),
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
            verified,
            verified_at: None,
            author: None,
            verified_by: None,
            created_at: updated,
            updated_at: updated,
            deleted_at: if deleted { Some(updated) } else { None },
        }
    }

    #[test]
    fn no_existing_inserts() {
        let inc = entry(Utc::now(), false, false);
        let r = resolve(None, &inc);
        assert!(matches!(r, Resolution::Insert));
    }

    #[test]
    fn soft_delete_blocks_resurrection() {
        let now = Utc::now();
        let existing = entry(now, false, true);
        let incoming = entry(now + Duration::seconds(60), false, false);
        match resolve(Some(&existing), &incoming) {
            Resolution::Skip { policy: ConflictPolicy::ResurrectBlocked, audit: true } => {}
            other => panic!("expected resurrect-blocked Skip, got {other:?}"),
        }
    }

    #[test]
    fn soft_delete_propagates() {
        let now = Utc::now();
        let existing = entry(now, false, false);
        let incoming = entry(now + Duration::seconds(60), false, true);
        match resolve(Some(&existing), &incoming) {
            Resolution::Update { policy: ConflictPolicy::SoftDeleteWins, .. } => {}
            other => panic!("expected SoftDeleteWins Update, got {other:?}"),
        }
    }

    #[test]
    fn verified_existing_blocks_unverified_incoming() {
        let now = Utc::now();
        let existing = entry(now, true, false);
        let incoming = entry(now + Duration::seconds(60), false, false);
        match resolve(Some(&existing), &incoming) {
            Resolution::Skip { policy: ConflictPolicy::VerifiedWins, audit: true } => {}
            other => panic!("expected verified-wins Skip, got {other:?}"),
        }
    }

    #[test]
    fn unverified_existing_yields_to_verified_incoming() {
        let now = Utc::now();
        let existing = entry(now + Duration::seconds(60), false, false);
        let incoming = entry(now, true, false); // older but verified
        match resolve(Some(&existing), &incoming) {
            Resolution::Update { policy: ConflictPolicy::VerifiedWins, .. } => {}
            other => panic!("expected verified-wins Update, got {other:?}"),
        }
    }

    #[test]
    fn lww_newer_incoming_wins() {
        let now = Utc::now();
        let existing = entry(now, false, false);
        let incoming = entry(now + Duration::seconds(10), false, false);
        match resolve(Some(&existing), &incoming) {
            Resolution::Update { policy: ConflictPolicy::Lww, audit: false } => {}
            other => panic!("expected lww Update, got {other:?}"),
        }
    }

    #[test]
    fn lww_older_incoming_skips() {
        let now = Utc::now();
        let existing = entry(now, false, false);
        let incoming = entry(now - Duration::seconds(10), false, false);
        match resolve(Some(&existing), &incoming) {
            Resolution::Skip { policy: ConflictPolicy::Lww, audit: false } => {}
            other => panic!("expected lww Skip, got {other:?}"),
        }
    }

    #[test]
    fn lww_equal_updated_at_skips() {
        let now = Utc::now();
        let existing = entry(now, false, false);
        let incoming = entry(now, false, false);
        match resolve(Some(&existing), &incoming) {
            Resolution::Skip { policy: ConflictPolicy::Lww, .. } => {}
            other => panic!("expected lww Skip, got {other:?}"),
        }
    }
}
