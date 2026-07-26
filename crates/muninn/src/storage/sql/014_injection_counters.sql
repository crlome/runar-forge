-- Separate counters for the injection channel.
--
-- `access_count` has exactly one writer (`touch_entries`, reached only from
-- ranked search), so it measures how often someone called `muninn_search` —
-- not whether a memory was ever put in front of a model. The 2026-07-26 audit
-- reported "95.9% never retrieved" for three months on the strength of a
-- counter whose only writer ran three times in the observation week, while
-- 15,819 context injections went unrecorded.
--
-- These columns record the other channel. They are deliberately NOT wired
-- into decay, `citation_threshold`, or dedup keeper selection: the context
-- packet used to be a fixed recency window, so counting it there would have
-- pinned the same rows at the top forever. Ranking stays on `access_count`
-- until real injection distributions exist to reason about.
ALTER TABLE memory_entries ADD COLUMN injected_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE memory_entries ADD COLUMN last_injected_at TEXT;

-- Reads are "which entries has this project actually served, recently" —
-- always namespace-scoped, like every other read path since 012.
CREATE INDEX IF NOT EXISTS idx_entries_injected
    ON memory_entries (namespace, last_injected_at DESC);
