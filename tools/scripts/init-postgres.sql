-- RunarForge PostgreSQL initialization
-- Runs once when the PostgreSQL container is first created

-- Enable pgvector extension
CREATE EXTENSION IF NOT EXISTS vector;

-- Enable full-text search
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Create schemas (packages own their schema)
CREATE SCHEMA IF NOT EXISTS muninn;
CREATE SCHEMA IF NOT EXISTS huginn;
CREATE SCHEMA IF NOT EXISTS curator;
CREATE SCHEMA IF NOT EXISTS audit;

-- Verify pgvector is working
DO $$
BEGIN
  PERFORM '[1,2,3]'::vector;
  RAISE NOTICE 'pgvector extension verified';
END $$;
