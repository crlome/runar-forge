-- Phase 5.6.1 — Sync Foundation (PostgreSQL)
--
-- Mirror of sql/010_add_sync_outbox.sql with PG-native types.

CREATE TABLE IF NOT EXISTS muninn.sync_outbox (
  id              TEXT        PRIMARY KEY,
  entry_id        TEXT        NOT NULL,
  op_kind         TEXT        NOT NULL
                              CHECK (op_kind IN ('insert', 'update', 'delete')),
  row_payload     JSONB       NOT NULL DEFAULT '{}'::jsonb,
  attempts        INTEGER     NOT NULL DEFAULT 0,
  last_error      TEXT,
  claimed_at      TIMESTAMPTZ,
  confirmed_at    TIMESTAMPTZ,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_sync_outbox_pending
  ON muninn.sync_outbox (created_at)
  WHERE confirmed_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_sync_outbox_entry
  ON muninn.sync_outbox (entry_id, created_at);

CREATE INDEX IF NOT EXISTS idx_sync_outbox_gc
  ON muninn.sync_outbox (confirmed_at)
  WHERE confirmed_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_sync_outbox_stale
  ON muninn.sync_outbox (claimed_at)
  WHERE claimed_at IS NOT NULL AND confirmed_at IS NULL;

CREATE TABLE IF NOT EXISTS muninn.sync_state (
  id                       INTEGER     PRIMARY KEY CHECK (id = 1),
  last_pulled_updated_at   TIMESTAMPTZ,
  last_pulled_session_at   TIMESTAMPTZ,
  last_pulled_edge_at      TIMESTAMPTZ,
  last_push_at             TIMESTAMPTZ,
  last_pull_at             TIMESTAMPTZ,
  local_dim                INTEGER,
  remote_dim               INTEGER,
  local_schema_version     TEXT,
  remote_schema_version    TEXT,
  initialized_at           TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS muninn.sync_conflicts (
  id                  TEXT        NOT NULL PRIMARY KEY,
  entry_id            TEXT        NOT NULL,
  direction           TEXT        NOT NULL
                                  CHECK (direction IN ('push', 'pull')),
  policy              TEXT        NOT NULL
                                  CHECK (policy IN ('lww', 'verified-wins',
                                                     'soft-delete-wins',
                                                     'resurrect-blocked')),
  winner_side         TEXT        NOT NULL
                                  CHECK (winner_side IN ('local', 'remote')),
  local_updated_at    TIMESTAMPTZ,
  remote_updated_at   TIMESTAMPTZ,
  local_payload       JSONB,
  remote_payload      JSONB,
  created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_sync_conflicts_recent
  ON muninn.sync_conflicts (created_at);

CREATE INDEX IF NOT EXISTS idx_sync_conflicts_entry
  ON muninn.sync_conflicts (entry_id, created_at);
