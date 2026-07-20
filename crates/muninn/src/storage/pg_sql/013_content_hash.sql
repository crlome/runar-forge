-- Exact-duplicate guard column (SHA-256 of normalized title+content).
-- Postgres needs no FTS fix: fts_vector is a column on the row and every
-- query already filters deleted_at IS NULL; a hard DELETE removes the
-- vector with the row.
ALTER TABLE muninn.memory_entries ADD COLUMN IF NOT EXISTS content_hash TEXT;

CREATE INDEX IF NOT EXISTS idx_memory_content_hash
  ON muninn.memory_entries (namespace, content_hash)
  WHERE deleted_at IS NULL AND content_hash IS NOT NULL;
