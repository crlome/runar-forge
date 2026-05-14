CREATE TABLE IF NOT EXISTS muninn.debug_log (
  id          TEXT        PRIMARY KEY,
  event       TEXT        NOT NULL,
  entry_id    TEXT,
  data        JSONB       NOT NULL DEFAULT '{}',
  duration_ms REAL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_debug_log_created ON muninn.debug_log (created_at);
CREATE INDEX IF NOT EXISTS idx_debug_log_event ON muninn.debug_log (event);
