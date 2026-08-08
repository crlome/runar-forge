-- The schema has to exist before the ledger that lives in it. This file is
-- migration 0: it runs unconditionally, before the ledger can be read, so
-- it is the only place a virgin database can be bootstrapped from. Creating
-- the schema in 001 instead is too late — 001 is skipped unless the ledger
-- says otherwise, and the ledger cannot be created without the schema.
--
-- It worked anyway for a long time because docker-compose mounts
-- tools/scripts/init-postgres.sql, which creates the schema when the
-- container is first built, and the live remote was provisioned the same
-- way. Pointing runar at a genuinely fresh Postgres failed with
-- `[3F000] schema "muninn" does not exist`.
CREATE SCHEMA IF NOT EXISTS muninn;

CREATE TABLE IF NOT EXISTS muninn.schema_migrations (
  version    TEXT PRIMARY KEY,
  applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
