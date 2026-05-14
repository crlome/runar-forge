CREATE TABLE IF NOT EXISTS memory_entries (
  id              TEXT PRIMARY KEY,
  namespace       TEXT NOT NULL DEFAULT 'default',
  title           TEXT NOT NULL,
  content         TEXT NOT NULL,
  type            TEXT NOT NULL,
  tags            TEXT NOT NULL DEFAULT '[]',
  project_id      TEXT,
  session_id      TEXT,
  related_ids     TEXT NOT NULL DEFAULT '[]',
  source          TEXT NOT NULL DEFAULT 'human',
  source_detail   TEXT,
  layer           INTEGER NOT NULL DEFAULT 3 CHECK (layer BETWEEN 1 AND 4),
  access_count    INTEGER NOT NULL DEFAULT 0,
  last_accessed_at TEXT,
  compressed_from TEXT NOT NULL DEFAULT '[]',
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  deleted_at      TEXT
);

CREATE INDEX IF NOT EXISTS idx_memory_namespace
  ON memory_entries (namespace) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_memory_type
  ON memory_entries (type) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_memory_project
  ON memory_entries (project_id) WHERE deleted_at IS NULL AND project_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_memory_layer
  ON memory_entries (layer) WHERE deleted_at IS NULL;

CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts
  USING fts5(title, content, tags, content=memory_entries, content_rowid=rowid);

CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory_entries BEGIN
  INSERT INTO memory_fts(rowid, title, content, tags)
  VALUES (new.rowid, new.title, new.content, new.tags);
END;

CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory_entries BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, title, content, tags)
  VALUES('delete', old.rowid, old.title, old.content, old.tags);
END;

CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory_entries BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, title, content, tags)
  VALUES('delete', old.rowid, old.title, old.content, old.tags);
  INSERT INTO memory_fts(rowid, title, content, tags)
  VALUES (new.rowid, new.title, new.content, new.tags);
END;
