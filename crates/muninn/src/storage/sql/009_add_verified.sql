ALTER TABLE memory_entries ADD COLUMN verified INTEGER NOT NULL DEFAULT 0;
ALTER TABLE memory_entries ADD COLUMN verified_at TEXT;

CREATE INDEX IF NOT EXISTS idx_memory_verified
  ON memory_entries (namespace, verified)
  WHERE deleted_at IS NULL AND verified = 1;
