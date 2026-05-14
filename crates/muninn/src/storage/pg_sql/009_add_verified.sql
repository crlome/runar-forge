ALTER TABLE muninn.memory_entries
  ADD COLUMN IF NOT EXISTS verified BOOLEAN NOT NULL DEFAULT FALSE,
  ADD COLUMN IF NOT EXISTS verified_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_memory_verified
  ON muninn.memory_entries (namespace, verified)
  WHERE deleted_at IS NULL AND verified = TRUE;
