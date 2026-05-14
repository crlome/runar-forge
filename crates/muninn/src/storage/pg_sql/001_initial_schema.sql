CREATE SCHEMA IF NOT EXISTS muninn;
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE IF NOT EXISTS muninn.memory_entries (
  id            TEXT        PRIMARY KEY,
  namespace     TEXT        NOT NULL DEFAULT 'default',
  title         TEXT        NOT NULL,
  content       TEXT        NOT NULL CHECK (char_length(content) <= 10000),
  type          TEXT        NOT NULL,
  tags          TEXT[]      NOT NULL DEFAULT '{}',
  project_id    TEXT,
  session_id    TEXT,
  related_ids   TEXT[]      NOT NULL DEFAULT '{}',
  embedding     vector,
  source        TEXT        NOT NULL DEFAULT 'human',
  source_detail TEXT,
  layer         INTEGER     NOT NULL DEFAULT 3 CHECK (layer BETWEEN 1 AND 4),
  access_count  INTEGER     NOT NULL DEFAULT 0,
  last_accessed_at TIMESTAMPTZ,
  compressed_from TEXT[]    NOT NULL DEFAULT '{}',
  fts_vector    TSVECTOR,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  deleted_at    TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_memory_namespace
  ON muninn.memory_entries (namespace) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_memory_type
  ON muninn.memory_entries (type) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_memory_tags
  ON muninn.memory_entries USING GIN (tags) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_memory_project
  ON muninn.memory_entries (project_id) WHERE deleted_at IS NULL AND project_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_memory_layer
  ON muninn.memory_entries (layer) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_memory_fts
  ON muninn.memory_entries USING GIN (fts_vector) WHERE deleted_at IS NULL;

CREATE OR REPLACE FUNCTION muninn.update_updated_at()
RETURNS TRIGGER AS $$
BEGIN
  NEW.updated_at = NOW();
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER memory_entries_updated_at
  BEFORE UPDATE ON muninn.memory_entries
  FOR EACH ROW EXECUTE FUNCTION muninn.update_updated_at();

CREATE OR REPLACE FUNCTION muninn.update_fts_vector()
RETURNS TRIGGER AS $$
BEGIN
  NEW.fts_vector := to_tsvector('english',
    coalesce(NEW.title, '') || ' ' ||
    coalesce(NEW.content, '') || ' ' ||
    coalesce(array_to_string(NEW.tags, ' '), '')
  );
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER memory_entries_fts_vector
  BEFORE INSERT OR UPDATE ON muninn.memory_entries
  FOR EACH ROW EXECUTE FUNCTION muninn.update_fts_vector();
