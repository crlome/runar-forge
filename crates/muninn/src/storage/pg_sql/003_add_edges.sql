CREATE TABLE IF NOT EXISTS muninn.memory_edges (
  id          TEXT        PRIMARY KEY,
  from_id     TEXT        NOT NULL REFERENCES muninn.memory_entries(id) ON DELETE CASCADE,
  to_id       TEXT        NOT NULL REFERENCES muninn.memory_entries(id) ON DELETE CASCADE,
  type        TEXT        NOT NULL CHECK (type IN ('supports', 'contradicts', 'supersedes', 'elaborates', 'related')),
  strength    REAL        NOT NULL DEFAULT 1.0 CHECK (strength >= 0.0 AND strength <= 1.0),
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_edges_from ON muninn.memory_edges (from_id);
CREATE INDEX IF NOT EXISTS idx_edges_to ON muninn.memory_edges (to_id);
CREATE INDEX IF NOT EXISTS idx_edges_type ON muninn.memory_edges (type);
