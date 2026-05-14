CREATE TABLE IF NOT EXISTS memory_embeddings (
  entry_id    TEXT PRIMARY KEY REFERENCES memory_entries(id) ON DELETE CASCADE,
  embedding   TEXT NOT NULL,
  created_at  TEXT NOT NULL
);
