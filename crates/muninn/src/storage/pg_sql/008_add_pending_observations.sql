CREATE TABLE IF NOT EXISTS muninn.pending_observations (
  id              TEXT        PRIMARY KEY,
  namespace       TEXT        NOT NULL DEFAULT 'default',
  session_id      TEXT,
  project_id      TEXT,
  tool_name       TEXT        NOT NULL,
  tool_input      JSONB       NOT NULL DEFAULT '{}'::jsonb,
  tool_response   JSONB       NOT NULL DEFAULT '{}'::jsonb,
  content_hash    TEXT        NOT NULL,
  status          TEXT        NOT NULL DEFAULT 'pending'
                              CHECK (status IN ('pending', 'processing', 'confirmed')),
  attempt_count   INTEGER     NOT NULL DEFAULT 0,
  claimed_at      TIMESTAMPTZ,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  confirmed_at    TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_pending_obs_claim
  ON muninn.pending_observations (namespace, status, created_at)
  WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_pending_obs_dedup
  ON muninn.pending_observations (content_hash, created_at);

CREATE INDEX IF NOT EXISTS idx_pending_obs_session
  ON muninn.pending_observations (session_id)
  WHERE session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_pending_obs_stale
  ON muninn.pending_observations (status, claimed_at)
  WHERE status = 'processing';
