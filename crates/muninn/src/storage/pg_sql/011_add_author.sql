-- Phase 5.7 — Per-dev attribution
--
-- `author` records the dev who proposed the entry; `verified_by` records
-- the dev who ran `mark_verified`. Both nullable: pre-existing rows stay
-- valid, agent-origin rows leave author NULL, unverified rows leave
-- verified_by NULL. Identity comes from `git config user.name` (name
-- only, never email — avoids storing PII in shared remote PG).

ALTER TABLE muninn.memory_entries
  ADD COLUMN IF NOT EXISTS author      TEXT,
  ADD COLUMN IF NOT EXISTS verified_by TEXT;

CREATE INDEX IF NOT EXISTS idx_memory_author
  ON muninn.memory_entries (author)
  WHERE deleted_at IS NULL AND author IS NOT NULL;
