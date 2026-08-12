use anyhow::Result;
use sqlx::PgPool;
use tracing::{error, info};

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct TorrentRow {
    pub author: String,
    pub repo_name: String,
}

#[derive(sqlx::FromRow)]
pub struct StoredTorrent {
    pub magnet_link: String,
    pub torrent_file: Vec<u8>,
}

#[derive(sqlx::FromRow)]
pub struct TorrentDownloadInfo {
    pub repo_name: String,
    pub torrent_file: Vec<u8>,
}

pub async fn top_recent(pool: &PgPool, limit: i64) -> Result<Vec<TorrentRow>> {
    let rows = sqlx::query_as::<_, TorrentRow>(
        "SELECT author, repo_name FROM torrents ORDER BY updated_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn top_popular(pool: &PgPool, limit: i64) -> Result<Vec<TorrentRow>> {
    let rows = sqlx::query_as::<_, TorrentRow>(
        "SELECT author, repo_name FROM torrents ORDER BY page_hits DESC, updated_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn search_local(pool: &PgPool, query: &str, limit: i64) -> Result<Vec<TorrentRow>> {
    const SIMILARITY_THRESHOLD: f32 = 0.1;
    match sqlx::query_as::<_, TorrentRow>(
        "SELECT author, repo_name FROM torrents \
         WHERE similarity(author, $1) > $2 OR similarity(repo_name, $1) > $2 \
            OR similarity(author || '/' || repo_name, $1) > $2 \
         ORDER BY GREATEST(similarity(author, $1), similarity(repo_name, $1), similarity(author || '/' || repo_name, $1)) DESC, page_hits DESC \
         LIMIT $3",
    )
    .bind(query)
    .bind(SIMILARITY_THRESHOLD)
    .bind(limit)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => Ok(rows),
        Err(e) => {
            error!("trigram search failed, falling back to ILIKE: {e}");
            let pattern = format!("%{query}%");
            let rows = sqlx::query_as::<_, TorrentRow>(
                "SELECT author, repo_name FROM torrents \
                 WHERE author ILIKE $1 OR repo_name ILIKE $1 OR (author || '/' || repo_name) ILIKE $1 \
                 ORDER BY page_hits DESC, updated_at DESC LIMIT $2",
            )
            .bind(&pattern)
            .bind(limit)
            .fetch_all(pool)
            .await?;
            Ok(rows)
        }
    }
}

pub async fn get_by_sha(pool: &PgPool, sha: &str) -> Result<Option<StoredTorrent>> {
    let row = sqlx::query_as::<_, StoredTorrent>(
        "SELECT magnet_link, torrent_file FROM torrents WHERE sha = $1",
    )
    .bind(sha)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn bump_page_hits(pool: &PgPool, sha: &str) {
    if let Err(e) = sqlx::query("UPDATE torrents SET page_hits = page_hits + 1 WHERE sha = $1")
        .bind(sha)
        .execute(pool)
        .await
    {
        error!("Failed to increment page_hits for SHA {sha}: {e}");
    }
}

pub async fn get_download(pool: &PgPool, sha: &str) -> Result<Option<TorrentDownloadInfo>> {
    let row = sqlx::query_as::<_, TorrentDownloadInfo>(
        "SELECT repo_name, torrent_file FROM torrents WHERE sha = $1",
    )
    .bind(sha)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn upsert_torrent(
    pool: &PgPool,
    sha: &str,
    author: &str,
    repo_name: &str,
    magnet_link: &str,
    torrent_file: &[u8],
) {
    match sqlx::query!(
        "INSERT INTO torrents (sha, author, repo_name, magnet_link, torrent_file, page_hits) VALUES ($1, $2, $3, $4, $5, 1)
                         ON CONFLICT(sha) DO UPDATE SET magnet_link = excluded.magnet_link, torrent_file = excluded.torrent_file, page_hits = torrents.page_hits + 1",
        sha,
        author,
        repo_name,
        magnet_link,
        torrent_file
    )
    .execute(pool)
    .await
    {
        Ok(_) => info!("Stored torrent in DB for SHA {sha}"),
        Err(e) => error!("Failed to insert/update torrent in DB for SHA {sha}: {e}"),
    }
}
