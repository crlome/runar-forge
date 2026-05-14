-- Phase 5.7 — Per-dev attribution (SQLite mirror of pg_sql/011)
--
-- Same shape as PostgreSQL: nullable `author` + `verified_by`. SQLite
-- supports partial indexes so the predicate matches the PG version.

ALTER TABLE memory_entries ADD COLUMN author      TEXT;
ALTER TABLE memory_entries ADD COLUMN verified_by TEXT;

CREATE INDEX IF NOT EXISTS idx_memory_author
  ON memory_entries (author)
  WHERE deleted_at IS NULL AND author IS NOT NULL;
