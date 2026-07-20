-- 1) Exact-duplicate guard column (SHA-256 of normalized title+content).
--    Backfill happens in `runar dedup`, not here, so the migration stays
--    instant on large databases.
ALTER TABLE memory_entries ADD COLUMN content_hash TEXT;

CREATE INDEX IF NOT EXISTS idx_memory_content_hash
  ON memory_entries (namespace, content_hash)
  WHERE deleted_at IS NULL AND content_hash IS NOT NULL;

-- 2) FTS trigger fix. Invariant after this migration: memory_fts contains
--    exactly the live (deleted_at IS NULL) rows. memory_fts is an
--    external-content FTS5 table, where a 'delete' on a rowid that was never
--    indexed corrupts the index — so every transition is guarded:
--      live -> live      : de-index old, re-index new   (normal edit)
--      live -> deleted   : de-index old only            (soft delete)
--      deleted -> deleted: no-op                        (tombstone edit)
--      deleted -> live   : re-index new only            (undelete)
--    memory_au fires only when indexed columns (or deleted_at) actually
--    change, so metadata-only updates — touch counters, content_hash
--    backfill, layer graduation — skip the FTS delete+reinsert entirely.
DROP TRIGGER IF EXISTS memory_au;
CREATE TRIGGER memory_au AFTER UPDATE OF title, content, tags, deleted_at ON memory_entries BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, title, content, tags)
  SELECT 'delete', old.rowid, old.title, old.content, old.tags
  WHERE old.deleted_at IS NULL;
  INSERT INTO memory_fts(rowid, title, content, tags)
  SELECT new.rowid, new.title, new.content, new.tags
  WHERE new.deleted_at IS NULL;
END;

DROP TRIGGER IF EXISTS memory_ad;
CREATE TRIGGER memory_ad AFTER DELETE ON memory_entries
WHEN old.deleted_at IS NULL
BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, title, content, tags)
  VALUES('delete', old.rowid, old.title, old.content, old.tags);
END;

-- memory_ai must respect the invariant too: sync conflict-resolution and
-- import both INSERT rows that already carry deleted_at.
DROP TRIGGER IF EXISTS memory_ai;
CREATE TRIGGER memory_ai AFTER INSERT ON memory_entries
WHEN new.deleted_at IS NULL
BEGIN
  INSERT INTO memory_fts(rowid, title, content, tags)
  VALUES (new.rowid, new.title, new.content, new.tags);
END;

-- 3) One-time purge of soft-deleted rows still in the index (the old
--    memory_au re-inserted them on every UPDATE; a soft delete does not
--    change title/content/tags, so the external-content 'delete' contract
--    holds: the values below match what is indexed).
INSERT INTO memory_fts(memory_fts, rowid, title, content, tags)
SELECT 'delete', rowid, title, content, tags
FROM memory_entries WHERE deleted_at IS NOT NULL;
