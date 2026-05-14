CREATE TABLE IF NOT EXISTS sessions (
  id              TEXT PRIMARY KEY,
  namespace       TEXT NOT NULL DEFAULT 'default',
  project_id      TEXT,
  tool            TEXT,
  goal            TEXT,
  summary         TEXT,
  discoveries     TEXT NOT NULL DEFAULT '[]',
  files_modified  TEXT NOT NULL DEFAULT '[]',
  status          TEXT NOT NULL DEFAULT 'active'
                  CHECK (status IN ('active', 'completed', 'abandoned')),
  started_at      TEXT NOT NULL,
  ended_at        TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sessions_namespace ON sessions (namespace);
CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions (status);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions (project_id) WHERE project_id IS NOT NULL;
