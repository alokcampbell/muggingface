CREATE TABLE IF NOT EXISTS torrents (
    sha TEXT PRIMARY KEY,
    author TEXT NOT NULL,
    repo_name TEXT NOT NULL,
    magnet_link TEXT NOT NULL,
    torrent_file BYTEA NOT NULL,
    page_hits INTEGER NOT NULL DEFAULT 0
); 
