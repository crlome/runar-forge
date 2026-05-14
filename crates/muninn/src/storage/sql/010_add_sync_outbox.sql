-- Phase 5.6.1 — Sync Foundation
--
-- Three tables for outbox+reconcile hybrid local↔remote sync:
--   sync_outbox    — durable per-row write log awaiting push to remote
--   sync_state     — singleton row tracking pull cursor + last init
--   sync_conflicts — audit log of resolver decisions (pull or push)
--
-- All tables stay empty for single-backend users (RUNAR_STORAGE_LOCAL
-- unset). Hybrid mode is opt-in via env in 5.6.2+.

CREATE TABLE IF NOT EXISTS sync_outbox (
  id              TEXT    PRIMARY KEY,
  entry_id        TEXT    NOT NULL,
  op_kind         TEXT    NOT NULL
                          CHECK (op_kind IN ('insert', 'update', 'delete')),
  row_payload     TEXT    NOT NULL DEFAULT '{}',
  attempts        INTEGER NOT NULL DEFAULT 0,
  last_error      TEXT,
  claimed_at      TEXT,
  confirmed_at    TEXT,
  created_at      TEXT    NOT NULL
);

-- FIFO claim of pending rows (confirmed_at IS NULL)
CREATE INDEX IF NOT EXISTS idx_sync_outbox_pending
  ON sync_outbox (created_at)
  WHERE confirmed_at IS NULL;

-- Coalesce duplicates per entry on push
CREATE INDEX IF NOT EXISTS idx_sync_outbox_entry
  ON sync_outbox (entry_id, created_at);

-- GC scan: confirmed rows older than retention threshold
CREATE INDEX IF NOT EXISTS idx_sync_outbox_gc
  ON sync_outbox (confirmed_at)
  WHERE confirmed_at IS NOT NULL;

-- Stale-claim recovery (claimed but not confirmed for too long)
CREATE INDEX IF NOT EXISTS idx_sync_outbox_stale
  ON sync_outbox (claimed_at)
  WHERE claimed_at IS NOT NULL AND confirmed_at IS NULL;

-- Singleton state row. id = 1 enforces single row.
CREATE TABLE IF NOT EXISTS sync_state (
  id                       INTEGER PRIMARY KEY CHECK (id = 1),
  last_pulled_updated_at   TEXT,
  last_pulled_session_at   TEXT,
  last_pulled_edge_at      TEXT,
  last_push_at             TEXT,
  last_pull_at             TEXT,
  local_dim                INTEGER,
  remote_dim               INTEGER,
  local_schema_version     TEXT,
  remote_schema_version    TEXT,
  initialized_at           TEXT
);

-- Audit log of resolver decisions. Append-only, GC'd separately.
CREATE TABLE IF NOT EXISTS sync_conflicts (
  id                  TEXT NOT NULL PRIMARY KEY,
  entry_id            TEXT NOT NULL,
  direction           TEXT NOT NULL
                      CHECK (direction IN ('push', 'pull')),
  policy              TEXT NOT NULL
                      CHECK (policy IN ('lww', 'verified-wins',
                                         'soft-delete-wins',
                                         'resurrect-blocked')),
  winner_side         TEXT NOT NULL
                      CHECK (winner_side IN ('local', 'remote')),
  local_updated_at    TEXT,
  remote_updated_at   TEXT,
  local_payload       TEXT,
  remote_payload      TEXT,
  created_at          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sync_conflicts_recent
  ON sync_conflicts (created_at);

CREATE INDEX IF NOT EXISTS idx_sync_conflicts_entry
  ON sync_conflicts (entry_id, created_at);
