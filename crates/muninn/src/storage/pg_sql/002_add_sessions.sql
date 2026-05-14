CREATE TABLE IF NOT EXISTS muninn.sessions (
  id            TEXT        PRIMARY KEY,
  namespace     TEXT        NOT NULL DEFAULT 'default',
  project_id    TEXT,
  tool          TEXT,
  goal          TEXT,
  summary       TEXT,
  discoveries   TEXT[]      NOT NULL DEFAULT '{}',
  files_modified TEXT[]     NOT NULL DEFAULT '{}',
  status        TEXT        NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active', 'completed', 'abandoned')),
  started_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  ended_at      TIMESTAMPTZ,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_sessions_namespace ON muninn.sessions (namespace);
CREATE INDEX IF NOT EXISTS idx_sessions_status ON muninn.sessions (status);
CREATE INDEX IF NOT EXISTS idx_sessions_project
  ON muninn.sessions (project_id) WHERE project_id IS NOT NULL;

CREATE TRIGGER sessions_updated_at
  BEFORE UPDATE ON muninn.sessions
  FOR EACH ROW EXECUTE FUNCTION muninn.update_updated_at();
