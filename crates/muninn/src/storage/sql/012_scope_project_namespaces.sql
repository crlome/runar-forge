-- One-time backfill enforcing the write-path invariant "namespace ==
-- project_id" on legacy rows. Early versions stored crawl/save entries under
-- namespace 'default' with project_id set; every read path now resolves the
-- namespace from the project id, so those rows must move or they become
-- unreachable through project-scoped reads.
UPDATE memory_entries SET namespace = project_id
 WHERE project_id IS NOT NULL AND project_id <> '' AND namespace <> project_id;

UPDATE sessions SET namespace = project_id
 WHERE project_id IS NOT NULL AND project_id <> '' AND namespace <> project_id;

-- The move can land two live rows with the same (namespace, topic_key) —
-- one written under each regime. Supersession only ever replaces the
-- newest, so the older twin would be stale forever: soft-delete all but
-- the newest per key now (the 013 purge later de-indexes these tombstones).
UPDATE memory_entries
 SET deleted_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
 WHERE deleted_at IS NULL
   AND topic_key IS NOT NULL AND topic_key <> ''
   AND id NOT IN (
     SELECT keep_id FROM (
       SELECT namespace AS ns, topic_key AS tk,
              (SELECT id FROM memory_entries m2
                WHERE m2.namespace = m1.namespace AND m2.topic_key = m1.topic_key
                  AND m2.deleted_at IS NULL
                ORDER BY m2.created_at DESC LIMIT 1) AS keep_id
       FROM memory_entries m1
       WHERE m1.deleted_at IS NULL AND m1.topic_key IS NOT NULL AND m1.topic_key <> ''
       GROUP BY m1.namespace, m1.topic_key
     )
   );
