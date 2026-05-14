ALTER TABLE memory_entries ADD COLUMN confidence REAL NOT NULL DEFAULT 0.9;

CREATE INDEX IF NOT EXISTS idx_memory_confidence
  ON memory_entries (confidence) WHERE deleted_at IS NULL;
