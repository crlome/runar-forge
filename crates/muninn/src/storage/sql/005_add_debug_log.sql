CREATE TABLE IF NOT EXISTS debug_log (
  id          TEXT PRIMARY KEY,
  event       TEXT NOT NULL,
  entry_id    TEXT,
  data        TEXT NOT NULL DEFAULT '{}',
  duration_ms REAL,
  created_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_debug_log_created ON debug_log (created_at);
CREATE INDEX IF NOT EXISTS idx_debug_log_event ON debug_log (event);
