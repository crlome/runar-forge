-- One-time backfill enforcing the write-path invariant "namespace ==
-- project_id" on legacy rows (see the sqlite twin for rationale).
UPDATE muninn.memory_entries SET namespace = project_id
 WHERE project_id IS NOT NULL AND project_id <> '' AND namespace <> project_id;

UPDATE muninn.sessions SET namespace = project_id
 WHERE project_id IS NOT NULL AND project_id <> '' AND namespace <> project_id;

-- Collapse (namespace, topic_key) twins created by the move: keep the
-- newest live row per key, soft-delete the rest (see sqlite twin).
UPDATE muninn.memory_entries
 SET deleted_at = NOW()
 WHERE deleted_at IS NULL
   AND topic_key IS NOT NULL AND topic_key <> ''
   AND id NOT IN (
     SELECT DISTINCT ON (namespace, topic_key) id
     FROM muninn.memory_entries
     WHERE deleted_at IS NULL AND topic_key IS NOT NULL AND topic_key <> ''
     ORDER BY namespace, topic_key, created_at DESC
   );
