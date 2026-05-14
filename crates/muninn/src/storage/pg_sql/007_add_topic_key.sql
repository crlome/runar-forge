ALTER TABLE muninn.memory_entries
  ADD COLUMN IF NOT EXISTS topic_key TEXT;

CREATE INDEX IF NOT EXISTS idx_memory_topic_key
  ON muninn.memory_entries (namespace, topic_key)
  WHERE deleted_at IS NULL AND topic_key IS NOT NULL;
