-- CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE INDEX IF NOT EXISTS idx_torrents_author_trgm ON torrents USING gin (author gin_trgm_ops);
CREATE INDEX IF NOT EXISTS idx_torrents_repo_name_trgm ON torrents USING gin (repo_name gin_trgm_ops);
