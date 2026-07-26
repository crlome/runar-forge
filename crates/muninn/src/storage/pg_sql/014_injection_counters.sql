-- Separate counters for the injection channel. See the sqlite twin
-- (sql/014_injection_counters.sql) for the full rationale: `access_count`
-- only ever records ranked search, so it cannot tell "served 13,063 times"
-- from "never served".
--
-- Not wired into decay, citation thresholds, or dedup keeper selection.
ALTER TABLE muninn.memory_entries
    ADD COLUMN IF NOT EXISTS injected_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE muninn.memory_entries
    ADD COLUMN IF NOT EXISTS last_injected_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_entries_injected
    ON muninn.memory_entries (namespace, last_injected_at DESC);
