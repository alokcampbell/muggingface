-- Add migration script here
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = CURRENT_TIMESTAMP;
    RETURN NEW;
END;
$$ language 'plpgsql';

CREATE TABLE IF NOT EXISTS torrents (
    sha TEXT PRIMARY KEY,
    author TEXT NOT NULL,
    repo_name TEXT NOT NULL,
    magnet_link TEXT NOT NULL,
    torrent_file BYTEA NOT NULL,
    page_hits INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TRIGGER update_torrents_updated_at
BEFORE UPDATE ON torrents
FOR EACH ROW
EXECUTE FUNCTION update_updated_at_column();
