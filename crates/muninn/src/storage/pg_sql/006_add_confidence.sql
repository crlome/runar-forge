ALTER TABLE muninn.memory_entries
  ADD COLUMN IF NOT EXISTS confidence REAL NOT NULL DEFAULT 0.9;

CREATE INDEX IF NOT EXISTS idx_memory_confidence
  ON muninn.memory_entries (confidence) WHERE deleted_at IS NULL;
