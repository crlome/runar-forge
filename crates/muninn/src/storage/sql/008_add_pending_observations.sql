CREATE TABLE IF NOT EXISTS pending_observations (
  id              TEXT PRIMARY KEY,
  namespace       TEXT NOT NULL DEFAULT 'default',
  session_id      TEXT,
  project_id      TEXT,
  tool_name       TEXT NOT NULL,
  tool_input      TEXT NOT NULL DEFAULT '{}',
  tool_response   TEXT NOT NULL DEFAULT '{}',
  content_hash    TEXT NOT NULL,
  status          TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'processing', 'confirmed')),
  attempt_count   INTEGER NOT NULL DEFAULT 0,
  claimed_at      TEXT,
  created_at      TEXT NOT NULL,
  confirmed_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_pending_obs_claim
  ON pending_observations (namespace, status, created_at)
  WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_pending_obs_dedup
  ON pending_observations (content_hash, created_at);

CREATE INDEX IF NOT EXISTS idx_pending_obs_session
  ON pending_observations (session_id)
  WHERE session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_pending_obs_stale
  ON pending_observations (status, claimed_at)
  WHERE status = 'processing';
