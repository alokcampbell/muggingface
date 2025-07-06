use actix_files::Files;
use actix_web::middleware::Logger;
use actix_web::{
    get,
    web::{self, Json, ServiceConfig},
    Error as ActixError, HttpResponse, Responder,
};
use shuttle_actix_web::ShuttleActixWeb;
use directories::BaseDirs;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use anyhow::Result;
use bendy::{decoding::FromBencode, encoding::ToBencode, value::Value};
use hex;
use hf_hub::api::sync::Api;
use librqbit;
use reqwest;
use sha1::{Digest, Sha1};
use shuttle_runtime::{SecretStore, Secrets};
use sqlx::PgPool;
use tera::{Context, Tera};
use tracing::{error, info};
use urlencoding;

#[derive(serde::Serialize, sqlx::FromRow)]
struct TopTorrent {
    author: String,
    repo_name: String,
}

#[derive(serde::Serialize, sqlx::FromRow)]
struct SearchSuggestion {
    author: String,
    repo_name: String,
    full_repo: String,
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: String,
}

// New Git LFS-aware repository cloning module
mod git_lfs_clone {
    use super::*;
    use tokio::process::Command as TokioCommand;
    
    pub struct GitLfsCloner {
        seeding_dir: PathBuf,
        hf_token: Option<String>,
        progress_callback: Option<Box<dyn Fn(u64) + Send + Sync>>,
    }
    
    impl GitLfsCloner {
        pub fn new(seeding_dir: PathBuf, hf_token: Option<String>) -> Self {
            Self {
                seeding_dir,
                hf_token,
                progress_callback: None,
            }
        }
        
        pub fn with_progress_callback<F>(mut self, callback: F) -> Self 
        where
            F: Fn(u64) + Send + Sync + 'static,
        {
            self.progress_callback = Some(Box::new(callback));
            self
        }
        
        pub async fn clone_repository(&self, full_repo: &str, sha: &str) -> Result<PathBuf> {
            let target_dir = self.seeding_dir.join(format!("{}-{}", full_repo.replace("/", "-"), sha));
            
            // Check if repository already exists and is complete
            if self.is_repository_complete(&target_dir).await? {
                info!("Repository {} already complete at {}", full_repo, target_dir.display());
                return Ok(target_dir);
            }
            
            // Step 1: Git clone with LFS skip
            self.git_clone_with_lfs_skip(&target_dir, full_repo).await?;
            
            // Step 2: Identify and download LFS files
            self.download_lfs_files(&target_dir, full_repo).await?;
            
            Ok(target_dir)
        }
        
        async fn is_repository_complete(&self, target_dir: &Path) -> Result<bool> {
            if !target_dir.exists() || !target_dir.join(".git").exists() {
                return Ok(false);
            }
            
            // Check if all LFS files are properly downloaded (not just pointer files)
            let lfs_files = self.get_lfs_files(target_dir).await?;
            for lfs_file in lfs_files {
                let file_path = target_dir.join(&lfs_file);
                if !file_path.exists() {
                    return Ok(false);
                }
                
                // Check if file is just an LFS pointer (small file starting with "version https://git-lfs.github.com/spec/v1")
                if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
                    if content.starts_with("version https://git-lfs.github.com/spec/v1") {
                        return Ok(false); // Still a pointer file, not the actual content
                    }
                }
            }
            
            Ok(true)
        }
        
        async fn git_clone_with_lfs_skip(&self, target_dir: &Path, full_repo: &str) -> Result<()> {
            if target_dir.exists() {
                // If directory exists, try to pull latest changes
                info!("Repository exists, pulling latest changes");
                self.git_pull_with_lfs_skip(target_dir).await?;
                return Ok(());
            }
            
            let repo_url = self.build_repo_url(full_repo);
            
            info!("Cloning repository {} to {}", repo_url, target_dir.display());
            
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(600), // 10 minute timeout for git clone
                TokioCommand::new("git")
                    .env("GIT_LFS_SKIP_SMUDGE", "1")
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .args(&["clone", &repo_url, &target_dir.to_string_lossy()])
                    .output()
            ).await??;
            
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow::anyhow!("Git clone failed: {}", stderr));
            }
            
            info!("Successfully cloned repository structure");
            Ok(())
        }
        
        async fn git_pull_with_lfs_skip(&self, target_dir: &Path) -> Result<()> {
            let output = TokioCommand::new("git")
                .env("GIT_LFS_SKIP_SMUDGE", "1")
                .current_dir(target_dir)
                .args(&["pull"])
                .output()
                .await?;
            
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                info!("Git pull failed (this might be normal): {}", stderr);
            }
            
            Ok(())
        }
        
        pub async fn get_lfs_files(&self, target_dir: &Path) -> Result<Vec<String>> {
            info!("Identifying LFS files in repository...");
            
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(60), // 1 minute timeout for git lfs ls-files
                TokioCommand::new("git")
                    .current_dir(target_dir)
                    .args(&["lfs", "ls-files"])
                    .output()
            ).await??;
            
            if !output.status.success() {
                // If git lfs ls-files fails, return empty list (repository might not use LFS)
                info!("git lfs ls-files failed or repository doesn't use LFS");
                return Ok(Vec::new());
            }
            
            let stdout = String::from_utf8_lossy(&output.stdout);
            let files: Vec<String> = stdout
                .lines()
                .filter_map(|line| {
                    // git lfs ls-files output format: "hash * filename"
                    line.split_whitespace().nth(2).map(|s| s.to_string())
                })
                .collect();
            
            info!("Found {} LFS files", files.len());
            Ok(files)
        }
        
        async fn download_lfs_files(&self, target_dir: &Path, full_repo: &str) -> Result<()> {
            let lfs_files = self.get_lfs_files(target_dir).await?;
            
            if lfs_files.is_empty() {
                info!("No LFS files found in repository");
                return Ok(());
            }
            
            info!("Found {} LFS files to download", lfs_files.len());
            
            let client = reqwest::Client::builder()
                .user_agent("muggingface/1.0")
                .redirect(reqwest::redirect::Policy::limited(20))
                .timeout(std::time::Duration::from_secs(300)) // 5 minute timeout per request
                .build()?;
            
            for (i, lfs_file) in lfs_files.iter().enumerate() {
                let file_path = target_dir.join(&lfs_file);
                
                info!("Processing LFS file {}/{}: {}", i + 1, lfs_files.len(), lfs_file);
                
                // Skip if file already exists and is not a pointer
                if file_path.exists() {
                    if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
                        if !content.starts_with("version https://git-lfs.github.com/spec/v1") {
                            info!("File {} already downloaded, skipping", lfs_file);
                            continue;
                        }
                    }
                }
                
                // Add timeout and retry logic for individual file downloads
                let mut attempts = 0;
                const MAX_ATTEMPTS: u32 = 3;
                
                while attempts < MAX_ATTEMPTS {
                    attempts += 1;
                    info!("Attempting to download {} (attempt {}/{})", lfs_file, attempts, MAX_ATTEMPTS);
                    
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(600), // 10 minute timeout per file
                        self.download_single_lfs_file(&client, target_dir, full_repo, &lfs_file)
                    ).await {
                        Ok(Ok(())) => {
                            info!("Successfully downloaded {} on attempt {}", lfs_file, attempts);
                            break;
                        }
                        Ok(Err(e)) => {
                            error!("Failed to download {} (attempt {}): {}", lfs_file, attempts, e);
                            if attempts == MAX_ATTEMPTS {
                                return Err(anyhow::anyhow!("Failed to download {} after {} attempts: {}", lfs_file, MAX_ATTEMPTS, e));
                            }
                            // Wait before retrying
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        Err(_) => {
                            error!("Download timed out for {} (attempt {})", lfs_file, attempts);
                            if attempts == MAX_ATTEMPTS {
                                return Err(anyhow::anyhow!("Download timed out for {} after {} attempts", lfs_file, MAX_ATTEMPTS));
                            }
                            // Wait before retrying
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            }
            
            info!("Completed downloading all {} LFS files", lfs_files.len());
            Ok(())
        }
        
        async fn download_single_lfs_file(&self, client: &reqwest::Client, target_dir: &Path, full_repo: &str, lfs_file: &str) -> Result<()> {
            let download_url = format!("https://huggingface.co/{}/resolve/main/{}", full_repo, lfs_file);
            let file_path = target_dir.join(lfs_file);
            
            // Ensure parent directory exists
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            
            info!("Downloading LFS file: {} from {}", lfs_file, download_url);
            
            let mut request_builder = client.get(&download_url);
            
            if let Some(token) = &self.hf_token {
                request_builder = request_builder.header("Authorization", format!("Bearer {}", token));
            }
            
            let mut response = request_builder.send().await?;
            
            if !response.status().is_success() {
                return Err(anyhow::anyhow!("Failed to download {}: HTTP {}", download_url, response.status()));
            }
            
            let content_length = response.content_length().unwrap_or(0);
            info!("Starting download of {} (expected size: {} bytes)", lfs_file, content_length);
            
            let mut dest_file = File::create(&file_path).await?;
            let mut downloaded_bytes = 0u64;
            let mut last_progress_log = std::time::Instant::now();
            
            while let Some(chunk) = response.chunk().await? {
                dest_file.write_all(&chunk).await?;
                downloaded_bytes += chunk.len() as u64;
                
                // Report progress if callback is provided
                if let Some(callback) = &self.progress_callback {
                    callback(chunk.len() as u64);
                }
                
                // Log progress every 30 seconds for large files
                if last_progress_log.elapsed().as_secs() >= 30 {
                    let progress_percent = if content_length > 0 {
                        (downloaded_bytes as f64 / content_length as f64) * 100.0
                    } else {
                        0.0
                    };
                    info!("Download progress for {}: {} bytes ({:.1}%)", lfs_file, downloaded_bytes, progress_percent);
                    last_progress_log = std::time::Instant::now();
                }
            }
            
            dest_file.flush().await?;
            info!("Successfully downloaded: {} ({} bytes)", lfs_file, downloaded_bytes);
            Ok(())
        }
        
        fn build_repo_url(&self, full_repo: &str) -> String {
            if let Some(token) = &self.hf_token {
                format!("https://user:{}@huggingface.co/{}", token, full_repo)
            } else {
                format!("https://huggingface.co/{}", full_repo)
            }
        }
    }
}

fn get_seeding_dir() -> Result<PathBuf> {
    const SEEDING_DIR: &str = "seeding";
    let base_dirs =
        BaseDirs::new().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(base_dirs.home_dir().join(SEEDING_DIR))
}

/// Ensures server directories exist. Returns true if directories were created, false if they already existed, error if failed to create directories.
fn ensure_server_directories() -> Result<bool> {
    let seeding_dir = get_seeding_dir()?;

    let mut created_dirs = false;
    for dir in &[seeding_dir] {
        if !dir.exists() {
            created_dirs = true;
            ensure_directory_exists(dir)?;
        }
    }
    if created_dirs {
        info!("Server directories initialized successfully");
    } else {
        info!("Server directories already existed");
    }
    Ok(created_dirs)
}

fn ensure_directory_exists(dir_path: &Path) -> Result<()> {
    if !dir_path.exists() {
        info!("Creating directory: {}", dir_path.display());
        std::fs::create_dir_all(dir_path).map_err(|e| {
            anyhow::anyhow!("Failed to create directory {}: {}", dir_path.display(), e)
        })?;
    }
    Ok(())
}

fn render_loading_html_response(full_repo: &str, sha: &str, tera: &Tera) -> String {
    let mut context = Context::new();
    context.insert("full_repo", full_repo);
    context.insert("sha", sha);
    tera.render("loading.html", &context)
        .expect("Failed to render loading template")
}

fn render_finished_html_response(
    full_repo: &str,
    sha: &str,
    file_names: &[String],
    magnet_link: &str,
    tera: &Tera,
) -> String {
    let mut context = Context::new();
    context.insert("full_repo", full_repo);
    context.insert("sha", sha);
    context.insert("file_names", file_names);
    context.insert("magnet_link", magnet_link);
    tera.render("finished.html", &context)
        .expect("Failed to render finished template")
}

#[get("/")]
async fn index(state: web::Data<Arc<AppState>>) -> impl Responder {
    let mut context = Context::new();

    match sqlx::query_as::<_, TopTorrent>(
        "SELECT author, repo_name FROM torrents ORDER BY updated_at DESC LIMIT 10"
    )
    .fetch_all(&state.db_pool)
    .await
    {
        Ok(top_torrents) => {
            context.insert("top_torrents", &top_torrents);
        }
        Err(e) => {
            error!("Failed to fetch top torrents: {}", e);
        }
    }

    match state.tera.render("index.html", &context) {
        Ok(html_body) => HttpResponse::Ok().content_type("text/html").body(html_body),
        Err(e) => {
            error!("Failed to render index.html: {}", e);
            HttpResponse::InternalServerError().body("Server error: Could not render homepage.")
        }
    }
}

#[get("/{user}/{repo}{tail:.*}")]
async fn repo_info(
    path: web::Path<(String, String, String)>,
    state: web::Data<Arc<AppState>>,
) -> impl Responder {
    let (user, repo, _tail) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);
    info!("Requesting repo info for {}", full_repo);
    let hf_repo_info = state.hf_api.model(full_repo.clone());

    match hf_repo_info.info() {
        Ok(info) => {
            // Attempt to retrieve torrent info from the database first
            match sqlx::query!(
                "SELECT magnet_link, torrent_file FROM torrents WHERE sha = $1",
                info.sha
            )
            .fetch_optional(&state.db_pool)
            .await
            {
                Ok(Some(record)) => {
                    info!(
                        "Found torrent for SHA {} in database. Incrementing page hits.",
                        info.sha
                    );
                    // Increment page hits
                    if let Err(e) = sqlx::query!(
                        "UPDATE torrents SET page_hits = page_hits + 1 WHERE sha = $1",
                        info.sha
                    )
                    .execute(&state.db_pool)
                    .await
                    {
                        error!("Failed to increment page_hits for SHA {}: {}", info.sha, e);
                    }

                    let file_names: Vec<String> =
                        info.siblings.iter().map(|f| f.rfilename.clone()).collect();
                    // We might not need to serve the torrent_file directly if we have the magnet link,
                    // but it's good that it's stored.
                    // For now, directly render finished page with DB magnet link.
                    return HttpResponse::Ok().content_type("text/html").body(
                        render_finished_html_response(
                            &full_repo,
                            &info.sha,
                            &file_names,
                            &record.magnet_link,
                            &state.tera,
                        ),
                    );
                }
                Ok(None) => {
                    info!(
                        "No torrent found in database for SHA {}, proceeding with generation.",
                        info.sha
                    );
                }
                Err(e) => {
                    error!(
                        "Database error when checking for torrent with SHA {}: {}",
                        info.sha, e
                    );
                    // Proceed as if not found, but log the error
                }
            }

            let seeding_dir = match get_seeding_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    error!("Failed to get seeding directory: {}", e);
                    return HttpResponse::InternalServerError()
                        .body("Failed to determine server storage directory. Please check server logs for details.");
                }
            };

            let _ = ensure_server_directories().map_err(|e| {
                error!("Failed to create server directories: {}", e);
                HttpResponse::InternalServerError()
                    .body(format!("Failed to create server directories: {}", e))
            });

            // getting the total size of the repo
            let mut total_size = 0u64;
            let mut size_breakdown = Vec::new();
            
            info!("Calculating repository size for {} files...", info.siblings.len());
            
            for file_info in &info.siblings {
                let url = format!(
                    "https://huggingface.co/{}/resolve/main/{}?download=true",
                    full_repo, file_info.rfilename
                );
                let client = reqwest::Client::builder()
                    .user_agent("muggingface/1.0")
                    .redirect(reqwest::redirect::Policy::limited(20))
                    .build()
                    .unwrap();
                let res = client.get(&url).header("Range", "bytes=0-0").send().await;
                if let Ok(resp) = res {
                    let mut file_size = 0u64;
                    if let Some(content_range) = resp.headers().get("Content-Range") {
                        if let Ok(s) = content_range.to_str() {
                            if let Some(size_str) = s.split('/').nth(1) {
                                if let Ok(size) = size_str.parse::<u64>() {
                                    file_size = size;
                                }
                            }
                        }
                    } else if let Some(cl) = resp.content_length() {
                        file_size = cl;
                    }
                    
                    if file_size > 0 {
                        total_size += file_size;
                        size_breakdown.push((file_info.rfilename.clone(), file_size));
                        info!("File: {} = {} bytes", file_info.rfilename, file_size);
                    } else {
                        info!("Could not determine size for: {}", file_info.rfilename);
                    }
                } else {
                    info!("Failed to fetch size for: {}", file_info.rfilename);
                }
            }
            
            info!("Total repository size: {} bytes from {} files", total_size, size_breakdown.len());
            
            // Log size breakdown for debugging
            size_breakdown.sort_by(|a, b| b.1.cmp(&a.1)); // Sort by size descending
            info!("Largest files:");
            for (filename, size) in size_breakdown.iter().take(10) {
                info!("  {} = {} bytes", filename, size);
            }
            const MAX_REPO_SIZE_BYTES: u64 = 60 * 1024 * 1024 * 1024;

            if total_size >= MAX_REPO_SIZE_BYTES {
                info!(
                  // Define the 60GB limit
              "Repository {} (Size: {} bytes) exceeds the {} byte limit. Rendering donate.html.",
                    full_repo,
                    total_size,
                    MAX_REPO_SIZE_BYTES
                );
                let mut context = Context::new();
                context.insert("full_repo", &full_repo);
                match state.tera.render("donate.html", &context) {
                    Ok(html_body) => {
                        return HttpResponse::Ok().content_type("text/html").body(html_body);
                    }
                    Err(e) => {
                        error!("Failed to render donate.html for {}: {}", full_repo, e);
                        return HttpResponse::InternalServerError()
                            .body("Server error: Could not render donation page.");
                    }
                }
            }

            {
                let mut progress = state.download_progress.lock().unwrap();
                progress.insert(full_repo.clone(), 0);
            }
            {
                let mut sizes = state.total_sizes.lock().unwrap();
                sizes.insert(full_repo.clone(), total_size);
            }
            
            info!("Progress tracking initialized - Total size: {} bytes", total_size);

            let file_names: Vec<String> =
                info.siblings.iter().map(|f| f.rfilename.clone()).collect();

            let target_dir =
                seeding_dir.join(format!("{}-{}", full_repo.replace("/", "-"), info.sha));

            let all_files_exist = info.siblings.iter().all(|file_info| {
                let file_path = target_dir.join(&file_info.rfilename);
                file_path.exists()
            });

            let torrent_path = seeding_dir.join(format!("{}.torrent", info.sha));

            if all_files_exist {
                info!(
                    "All files already exist in {}, generating magnet link",
                    target_dir.display()
                );

                let torrent_name = target_dir
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();

                let options = librqbit::CreateTorrentOptions {
                    name: Some(&torrent_name),
                    piece_length: Some(1_048_576),
                };

                match librqbit::create_torrent(&target_dir, options).await {
                    Ok(torrent_file) => {
                        if let Ok(torrent_bytes) = torrent_file.as_bytes() {
                            if let Err(e) = std::fs::write(&torrent_path, &torrent_bytes) {
                                error!(
                                    "Failed to write torrent file {}: {}",
                                    torrent_path.display(),
                                    e
                                );
                            } else {
                                let bencode_value = match Value::from_bencode(&torrent_bytes) {
                                    Ok(val) => val,
                                    Err(e) => {
                                        error!("Failed to parse bencode from torrent bytes: {}", e);
                                        return HttpResponse::InternalServerError()
                                            .body("Failed to generate magnet link");
                                    }
                                };
                                let info_dict = match bencode_value {
                                    Value::Dict(d) => match d.get(&b"info"[..]) {
                                        Some(val) => val.clone(),
                                        None => {
                                            error!("No 'info' dict found in torrent bencode");
                                            return HttpResponse::InternalServerError()
                                                .body("Failed to generate magnet link");
                                        }
                                    },
                                    _ => {
                                        error!("Invalid torrent format: not a dictionary at root");
                                        return HttpResponse::InternalServerError()
                                            .body("Failed to generate magnet link");
                                    }
                                };
                                let info_bytes = match info_dict.to_bencode() {
                                    Ok(bytes) => bytes,
                                    Err(e) => {
                                        error!("Failed to re-encode info_dict: {}", e);
                                        return HttpResponse::InternalServerError()
                                            .body("Failed to generate magnet link");
                                    }
                                };
                                let info_hash = Sha1::digest(&info_bytes);
                                let display_name =
                                    format!("{}-{}", full_repo.replace("/", "-"), info.sha);
                                let magnet_link_str = format!(
                                    "magnet:?xt=urn:btih:{}&dn={}&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=udp%3A%2F%2Ftracker.openbittorrent.com%3A6969%2Fannounce&tr=udp%3A%2F%2Ftracker.torrent.eu.org%3A451%2Fannounce",
                                    hex::encode(info_hash),
                                    urlencoding::encode(&display_name)
                                );

                                // Store in DB
                                let torrent_bytes_clone_for_db = torrent_bytes.clone();
                                let user_clone_for_db = user.clone();
                                let repo_clone_for_db = repo.clone();
                                let magnet_link_clone_for_db = magnet_link_str.clone();
                                let sha_clone_for_db = info.sha.clone();
                                let db_pool_clone = state.db_pool.clone();

                                tokio::spawn(async move {
                                    if let Err(e) = sqlx::query!(
                                        "INSERT INTO torrents (sha, author, repo_name, magnet_link, torrent_file, page_hits) VALUES ($1, $2, $3, $4, $5, 1)
                                         ON CONFLICT(sha) DO UPDATE SET magnet_link = excluded.magnet_link, torrent_file = excluded.torrent_file, page_hits = torrents.page_hits + 1",
                                        sha_clone_for_db,
                                        user_clone_for_db,
                                        repo_clone_for_db,
                                        magnet_link_clone_for_db,
                                        torrent_bytes_clone_for_db.as_ref()
                                    )
                                    .execute(&db_pool_clone)
                                    .await {
                                        error!("Failed to insert/update torrent in DB for SHA {}: {}", sha_clone_for_db, e);
                                    } else {
                                        info!("Successfully stored/updated torrent in DB for SHA {}", sha_clone_for_db);
                                    }
                                });

                                return HttpResponse::Ok().content_type("text/html").body(
                                    render_finished_html_response(
                                        &full_repo,
                                        &info.sha,
                                        &file_names,
                                        &magnet_link_str,
                                        &state.tera,
                                    ),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to create torrent for {}: {}",
                            target_dir.display(),
                            e
                        );
                    }
                }
            }

            // check if torrent file exists locally
            if torrent_path.exists() {
                info!("Torrent file {} already exists locally. Generating magnet link from local .torrent file.", torrent_path.display());
                match std::fs::read(&torrent_path) {
                    Ok(torrent_bytes) => {
                        let bencode_value = match Value::from_bencode(&torrent_bytes) {
                            Ok(val) => val,
                            Err(e) => {
                                error!(
                                    "Failed to parse bencode from local torrent file {}: {}",
                                    torrent_path.display(),
                                    e
                                );
                                return HttpResponse::InternalServerError().body("Failed to generate magnet link from existing torrent (parse error)");
                            }
                        };
                        let info_dict = match bencode_value {
                            Value::Dict(d) => {
                                match d.get(&b"info"[..]) {
                                    Some(val) => val.clone(),
                                    None => {
                                        error!("No 'info' dict found in local torrent bencode for {}: {}", torrent_path.display(), info.sha);
                                        return HttpResponse::InternalServerError().body("Failed to generate magnet link from existing torrent (no info dict)");
                                    }
                                }
                            }
                            _ => {
                                error!(
                                    "Invalid local torrent format (not a dict at root) for {}: {}",
                                    torrent_path.display(),
                                    info.sha
                                );
                                return HttpResponse::InternalServerError().body("Failed to generate magnet link from existing torrent (format error)");
                            }
                        };
                        let info_bytes = match info_dict.to_bencode() {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                error!(
                                    "Failed to re-encode info_dict for {}: {}",
                                    torrent_path.display(),
                                    e
                                );
                                return HttpResponse::InternalServerError().body("Failed to generate magnet link from existing torrent (re-encode error)");
                            }
                        };
                        let info_hash = Sha1::digest(&info_bytes);
                        let display_name = format!("{}-{}", full_repo.replace("/", "-"), info.sha);
                        let magnet_link_str = format!(
                            "magnet:?xt=urn:btih:{}&dn={}&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=udp%3A%2F%2Ftracker.openbittorrent.com%3A6969%2Fannounce&tr=udp%3A%2F%2Ftracker.torrent.eu.org%3A451%2Fannounce",
                            hex::encode(info_hash),
                            urlencoding::encode(&display_name)
                        );

                        // Store in DB
                        let torrent_bytes_clone_for_db = torrent_bytes.clone();
                        let user_clone_for_db = user.clone();
                        let repo_clone_for_db = repo.clone();
                        let magnet_link_clone_for_db = magnet_link_str.clone();
                        let sha_clone_for_db = info.sha.clone();
                        let db_pool_clone = state.db_pool.clone();

                        tokio::spawn(async move {
                            if let Err(e) = sqlx::query!(
                                "INSERT INTO torrents (sha, author, repo_name, magnet_link, torrent_file, page_hits) VALUES ($1, $2, $3, $4, $5, 1)
                                 ON CONFLICT(sha) DO UPDATE SET magnet_link = excluded.magnet_link, torrent_file = excluded.torrent_file, page_hits = torrents.page_hits + 1",
                                sha_clone_for_db,
                                user_clone_for_db,
                                repo_clone_for_db,
                                magnet_link_clone_for_db,
                                torrent_bytes_clone_for_db
                            )
                            .execute(&db_pool_clone)
                            .await {
                                error!("Failed to insert/update torrent in DB (from local .torrent) for SHA {}: {}", sha_clone_for_db, e);
                            } else {
                                info!("Successfully stored/updated torrent in DB (from local .torrent) for SHA {}", sha_clone_for_db);
                            }
                        });

                        return HttpResponse::Ok().content_type("text/html").body(
                            render_finished_html_response(
                                &full_repo,
                                &info.sha,
                                &file_names,
                                &magnet_link_str,
                                &state.tera,
                            ),
                        );
                    }
                    Err(e) => {
                        error!(
                            "Failed to read local torrent file {}: {}",
                            torrent_path.display(),
                            e
                        );
                        return HttpResponse::InternalServerError()
                            .body(format!("Failed to read local torrent file: {}", e));
                    }
                }
            }

            info!("No existing torrent file found, continuing with Git LFS clone and processing...");

            let full_repo_for_response = full_repo.clone();
            let sha_for_response = info.sha.clone();
            let info_clone = info.clone();
            let full_repo_clone = full_repo.clone();
            let state_clone = state.clone();
            let seeding_dir_clone = seeding_dir.clone();
            let user_clone_for_spawn = user.clone();
            let repo_clone_for_spawn = repo.clone();

            tokio::spawn(async move {
                // Use Git LFS-aware cloner with progress tracking
                let full_repo_for_progress = full_repo_clone.clone();
                let state_for_progress = state_clone.clone();
                
                let cloner = git_lfs_clone::GitLfsCloner::new(seeding_dir_clone.clone(), state_clone.hf_token.clone())
                    .with_progress_callback(move |bytes_downloaded| {
                        let mut progress_map = match state_for_progress.download_progress.lock() {
                            Ok(map) => map,
                            Err(poisoned) => {
                                error!("download_progress mutex poisoned");
                                poisoned.into_inner()
                            }
                        };
                        // Add to existing progress (which includes regular git files)
                        let current_progress = progress_map.get(&full_repo_for_progress).copied().unwrap_or(0);
                        let new_progress = current_progress + bytes_downloaded;
                        progress_map.insert(full_repo_for_progress.clone(), new_progress);
                        
                        // Log significant progress updates
                        if bytes_downloaded > 1024 * 1024 { // Log every MB
                            info!("LFS download progress: +{} bytes (total: {} bytes)", bytes_downloaded, new_progress);
                        }
                    });
                
                let target_dir_clone = match cloner.clone_repository(&full_repo_clone, &info_clone.sha).await {
                    Ok(dir) => dir,
                    Err(e) => {
                        error!("Failed to clone repository {}: {}", full_repo_clone, e);
                        return;
                    }
                };
                
                info!("Successfully cloned repository to: {}", target_dir_clone.display());
                
                // Verify all expected files are present and analyze file types
                let mut missing_files = Vec::new();
                let mut lfs_files = Vec::new();
                let mut regular_files = Vec::new();
                let mut lfs_total_size = 0u64;
                let mut regular_total_size = 0u64;
                
                // Get the list of LFS files from git
                let git_lfs_files = match git_lfs_clone::GitLfsCloner::new(seeding_dir_clone.clone(), state_clone.hf_token.clone()).get_lfs_files(&target_dir_clone).await {
                    Ok(files) => files,
                    Err(e) => {
                        error!("Failed to get LFS files list: {}", e);
                        Vec::new()
                    }
                };
                
                for file_info in &info_clone.siblings {
                    let file_path = target_dir_clone.join(&file_info.rfilename);
                    if !file_path.exists() {
                        missing_files.push(&file_info.rfilename);
                    } else {
                        // Categorize file type
                        if git_lfs_files.contains(&file_info.rfilename) {
                            lfs_files.push(&file_info.rfilename);
                            // Try to get the file size (this is approximate from the API)
                            if let Ok(metadata) = std::fs::metadata(&file_path) {
                                lfs_total_size += metadata.len();
                            }
                        } else {
                            regular_files.push(&file_info.rfilename);
                            if let Ok(metadata) = std::fs::metadata(&file_path) {
                                regular_total_size += metadata.len();
                            }
                        }
                    }
                }
                
                if !missing_files.is_empty() {
                    error!("Repository clone incomplete. Missing {} files: {:?}", missing_files.len(), missing_files);
                    return;
                }
                
                info!("Repository clone verification successful - all {} files are present", info_clone.siblings.len());
                info!("File breakdown:");
                info!("  - Regular Git files: {} files, {} bytes", regular_files.len(), regular_total_size);
                info!("  - LFS files: {} files, {} bytes", lfs_files.len(), lfs_total_size);
                info!("  - Total: {} bytes", regular_total_size + lfs_total_size);
                
                // Update progress to account for regular Git files (already downloaded via git clone)
                {
                    let mut progress_map = match state_clone.download_progress.lock() {
                        Ok(map) => map,
                        Err(poisoned) => {
                            error!("download_progress mutex poisoned during git clone completion");
                            poisoned.into_inner()
                        }
                    };
                    progress_map.insert(full_repo_clone.clone(), regular_total_size);
                    info!("Updated progress after git clone: {} bytes (regular files)", regular_total_size);
                }
                
                // Log the corrected progress calculation
                if let Some(total_size) = state_clone.total_sizes.lock().unwrap().get(&full_repo_clone) {
                    info!("Progress calculation:");
                    info!("  - Total expected: {} bytes", total_size);
                    info!("  - Regular files (completed): {} bytes", regular_total_size);
                    info!("  - LFS files (to download): {} bytes", lfs_total_size);
                    info!("  - Expected final progress: {} bytes", regular_total_size + lfs_total_size);
                    
                    if regular_total_size + lfs_total_size != *total_size {
                        error!("SIZE MISMATCH DETECTED!");
                        error!("  Expected total: {} bytes", total_size);
                        error!("  Calculated total: {} bytes", regular_total_size + lfs_total_size);
                        error!("  Difference: {} bytes", (*total_size as i64) - ((regular_total_size + lfs_total_size) as i64));
                    }
                }

                let torrent_path = seeding_dir_clone.join(format!("{}.torrent", info_clone.sha));
                let torrent_name = target_dir_clone
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();

                let options = librqbit::CreateTorrentOptions {
                    name: Some(&torrent_name),
                    piece_length: Some(1_048_576),
                };

                info!(
                    "Creating torrent from folder: {} (this may take several minutes for large repositories)",
                    target_dir_clone.display()
                );
                
                let torrent_file = match tokio::time::timeout(
                    std::time::Duration::from_secs(1800), // 30 minute timeout for torrent creation
                    librqbit::create_torrent(&target_dir_clone, options)
                ).await {
                    Ok(Ok(tf)) => {
                        info!("Successfully created torrent file");
                        tf
                    }
                    Ok(Err(e)) => {
                        error!(
                            "Failed to create torrent for {}: {}",
                            target_dir_clone.display(),
                            e
                        );
                        return;
                    }
                    Err(_) => {
                        error!("Torrent creation timed out after 30 minutes for {}", target_dir_clone.display());
                        return;
                    }
                };

                let torrent_bytes = match torrent_file.as_bytes() {
                    Ok(tb) => tb,
                    Err(e) => {
                        error!(
                            "Failed to get torrent bytes for {}: {}",
                            target_dir_clone.display(),
                            e
                        );
                        return;
                    }
                };
                let torrent_bytes_clone = torrent_bytes.clone();

                if let Err(e) = std::fs::write(&torrent_path, &torrent_bytes_clone) {
                    error!(
                        "Failed to write torrent file {}: {}",
                        torrent_path.display(),
                        e
                    );
                    return;
                }
                info!("Torrent file created at: {}", torrent_path.display());

                let bencode_value = match Value::from_bencode(&torrent_bytes_clone) {
                    Ok(val) => val,
                    Err(e) => {
                        error!("Failed to parse bencode from torrent bytes: {}", e);
                        return;
                    }
                };

                let info_dict = match bencode_value {
                    Value::Dict(d) => match d.get(&b"info"[..]) {
                        Some(val) => val.clone(),
                        None => {
                            error!("No 'info' dict found in torrent bencode");
                            return;
                        }
                    },
                    _ => {
                        error!("Invalid torrent format: not a dictionary at root");
                        return;
                    }
                };
                let info_bytes = match info_dict.to_bencode() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        error!("Failed to re-encode info_dict: {}", e);
                        return;
                    }
                };
                let info_hash = Sha1::digest(&info_bytes);
                let display_name =
                    format!("{}-{}", full_repo_clone.replace("/", "-"), info_clone.sha);
                let magnet_link_str = format!(
                    "magnet:?xt=urn:btih:{}&dn={}&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=udp%3A%2F%2Ftracker.openbittorrent.com%3A6969%2Fannounce&tr=udp%3A%2F%2Ftracker.torrent.eu.org%3A451%2Fannounce",
                    hex::encode(info_hash),
                    urlencoding::encode(&display_name)
                );

                // Store in DB
                let db_pool_clone_spawn = state_clone.db_pool.clone();
                let sha_clone_for_db_spawn = info_clone.sha.clone();
                // user_clone_for_spawn and repo_clone_for_spawn are already available
                let magnet_link_clone_for_db_spawn = magnet_link_str.clone();
                // torrent_bytes_clone is already available from the previous step

                tokio::spawn(async move {
                    if let Err(e) = sqlx::query!(
                        "INSERT INTO torrents (sha, author, repo_name, magnet_link, torrent_file, page_hits) VALUES ($1, $2, $3, $4, $5, 1)
                         ON CONFLICT(sha) DO UPDATE SET magnet_link = excluded.magnet_link, torrent_file = excluded.torrent_file, page_hits = torrents.page_hits + 1",
                        sha_clone_for_db_spawn,
                        user_clone_for_spawn, // Use cloned user
                        repo_clone_for_spawn, // Use cloned repo
                        magnet_link_clone_for_db_spawn,
                        torrent_bytes_clone.as_ref() // Use torrent_bytes_clone.as_ref() directly
                    )
                    .execute(&db_pool_clone_spawn)
                    .await {
                        error!("Failed to insert/update torrent in DB (from download task) for SHA {}: {}", sha_clone_for_db_spawn, e);
                    } else {
                        info!("Successfully stored/updated torrent in DB (from download task) for SHA {}", sha_clone_for_db_spawn);
                    }
                });
                
                // Final size verification and progress completion
                {
                    let mut progress_map = match state_clone.download_progress.lock() {
                        Ok(map) => map,
                        Err(poisoned) => {
                            error!("download_progress mutex poisoned during final update");
                            poisoned.into_inner()
                        }
                    };
                    
                    let current_progress = progress_map.get(&full_repo_clone).copied().unwrap_or(0);
                    let expected_total = state_clone.total_sizes.lock().unwrap().get(&full_repo_clone).copied().unwrap_or(0);
                    
                    info!("Final progress analysis:");
                    info!("  - Current progress: {} bytes", current_progress);
                    info!("  - Expected total: {} bytes", expected_total);
                    info!("  - Difference: {} bytes", (expected_total as i64) - (current_progress as i64));
                    
                    // Calculate actual file sizes on disk
                    let mut actual_total_size = 0u64;
                    let mut file_count = 0;
                    for file_info in &info_clone.siblings {
                        let file_path = target_dir_clone.join(&file_info.rfilename);
                        if let Ok(metadata) = std::fs::metadata(&file_path) {
                            actual_total_size += metadata.len();
                            file_count += 1;
                        }
                    }
                    
                    info!("  - Actual files on disk: {} files, {} bytes", file_count, actual_total_size);
                    
                    // Set progress to the actual size (most accurate)
                    if actual_total_size > 0 {
                        progress_map.insert(full_repo_clone.clone(), actual_total_size);
                        info!("Set progress to actual disk size: {} bytes", actual_total_size);
                    } else {
                        progress_map.insert(full_repo_clone.clone(), expected_total);
                        info!("Set progress to expected total: {} bytes", expected_total);
                    }
                }
                
                info!("Repository {} processing completed successfully!", full_repo_clone);
            });
            return HttpResponse::Ok().content_type("text/html").body(
                render_loading_html_response(
                    &full_repo_for_response,
                    &sha_for_response,
                    &state.tera,
                ),
            );
        }
        Err(_) => HttpResponse::NotFound().body(format!("Repository {} not found", full_repo)),
    }
}

// this is where we do the progress work, and the unwrapping the values
#[get("/{user}/{repo}/progress_json")]
async fn progress_json(
    path: web::Path<(String, String)>,
    state: web::Data<Arc<AppState>>,
) -> Result<Json<Progress>, ActixError> {
    let (user, repo) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);

    fn get_from_map<'a>(
        map: &'a Mutex<HashMap<String, u64>>,
        key: &str,
        default: u64,
    ) -> Result<u64, PoisonError<std::sync::MutexGuard<'a, HashMap<String, u64>>>> {
        let guard = map.lock()?;
        Ok(guard.get(key).copied().unwrap_or(default))
    }

    let downloaded = match get_from_map(&state.download_progress, &full_repo, 0) {
        Ok(v) => v,
        Err(_) => 0,
    };

    let total = match get_from_map(&state.total_sizes, &full_repo, 1) {
        Ok(v) => v,
        Err(_) => 1,
    };

    info!(
        target: "progress",
        "{} → downloaded: {} / total: {}",
        full_repo,
        downloaded,
        total
    );

    info!("Progress for {}: {}/{} bytes", full_repo, downloaded, total);

    Ok(web::Json(Progress { downloaded, total }))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Progress {
    downloaded: u64,
    total: u64,
}

#[derive(sqlx::FromRow)] // Helper struct for fetching the torrent file and repo name
struct TorrentDownloadInfo {
    repo_name: String,
    torrent_file: Vec<u8>,
}

#[get("/torrent/{sha}/download")]
async fn download_torrent_by_sha(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> impl Responder {
    let sha = path.into_inner();
    info!("Attempting to download .torrent file for SHA: {}", sha);

    match sqlx::query_as::<_, TorrentDownloadInfo>(
        "SELECT repo_name, torrent_file FROM torrents WHERE sha = $1",
    )
    .bind(&sha)
    .fetch_optional(&state.db_pool)
    .await
    {
        Ok(Some(record)) => {
            let filename = format!("{}.torrent", record.repo_name.replace("/", "-"));
            info!("Serving torrent file {} for SHA: {}", filename, sha);
            HttpResponse::Ok()
                .content_type("application/x-bittorrent")
                .insert_header((
                    "Content-Disposition",
                    format!("attachment; filename=\"{}\"", filename),
                ))
                .body(record.torrent_file)
        }
        Ok(None) => {
            error!("Torrent file not found in DB for SHA: {}", sha);
            HttpResponse::NotFound().body(format!("Torrent file for SHA {} not found.", sha))
        }
        Err(e) => {
            error!("Database error fetching torrent file for SHA {}: {}", sha, e);
            HttpResponse::InternalServerError().body("Error retrieving torrent file.")
        }
    }
}

#[get("/about")]
async fn about_page(state: web::Data<Arc<AppState>>) -> impl Responder {
    let context = Context::new();
    match state.tera.render("about.html", &context) {
        Ok(html_body) => HttpResponse::Ok().content_type("text/html").body(html_body),
        Err(e) => {
            error!("Failed to render about.html: {}", e);
            HttpResponse::InternalServerError().body("Server error: Could not render About page.")
        }
    }
}

#[get("/search")]
async fn search_torrents(
    query_params: web::Query<SearchQuery>,
    state: web::Data<Arc<AppState>>,
) -> impl Responder {
    let search_query = query_params.q.trim();

    if search_query.is_empty() {
        // Fetch top 3 most popular torrents
        match sqlx::query_as::<_, TopTorrent>(
            "SELECT author, repo_name FROM torrents ORDER BY page_hits DESC, updated_at DESC LIMIT 3"
        )
        .fetch_all(&state.db_pool)
        .await
        {
            Ok(results) => {
                let suggestions: Vec<SearchSuggestion> = results.into_iter().map(|t| SearchSuggestion {
                    full_repo: format!("{}/{}", t.author, t.repo_name),
                    author: t.author,
                    repo_name: t.repo_name,
                }).collect();
                HttpResponse::Ok().json(suggestions)
            }
            Err(e) => {
                error!("Failed to fetch top torrents for search suggestions: {}", e);
                HttpResponse::Ok().json(Vec::<SearchSuggestion>::new()) // Return empty on error but 200 OK for frontend
            }
        }
    } else {
        // Using pg_trgm for similarity search
        // Ensure the pg_trgm extension is created in your database:
        // CREATE EXTENSION IF NOT EXISTS pg_trgm;
        // And create GIN indexes for better performance:
        // CREATE INDEX IF NOT EXISTS idx_torrents_author_trgm ON torrents USING gin (author gin_trgm_ops);
        // CREATE INDEX IF NOT EXISTS idx_torrents_repo_name_trgm ON torrents USING gin (repo_name gin_trgm_ops);
        const SIMILARITY_THRESHOLD: f32 = 0.1; // Adjust as needed

        match sqlx::query_as::<_, TopTorrent>(
            "SELECT author, repo_name FROM torrents WHERE similarity(author, $1) > $2 OR similarity(repo_name, $1) > $2 ORDER BY GREATEST(similarity(author, $1), similarity(repo_name, $1)) DESC, page_hits DESC LIMIT 10"
        )
        .bind(search_query)
        .bind(SIMILARITY_THRESHOLD)
        .fetch_all(&state.db_pool)
        .await
        {
            Ok(results) => {
                let suggestions: Vec<SearchSuggestion> = results.into_iter().map(|t| SearchSuggestion {
                    full_repo: format!("{}/{}", t.author, t.repo_name),
                    author: t.author,
                    repo_name: t.repo_name,
                }).collect();
                HttpResponse::Ok().json(suggestions)
            }
            Err(e) => {
                error!("Failed to search torrents with query '{}' using trigram: {}", search_query, e);
                // Fallback to ILIKE if trigram query fails (e.g., extension not enabled)
                let fallback_search_term = format!("%{}%", search_query);
                match sqlx::query_as::<_, TopTorrent>(
                    "SELECT author, repo_name FROM torrents WHERE author ILIKE $1 OR repo_name ILIKE $1 ORDER BY page_hits DESC, updated_at DESC LIMIT 10"
                )
                .bind(&fallback_search_term)
                .fetch_all(&state.db_pool)
                .await
                {
                    Ok(fallback_results) => {
                         let suggestions: Vec<SearchSuggestion> = fallback_results.into_iter().map(|t| SearchSuggestion {
                            full_repo: format!("{}/{}", t.author, t.repo_name),
                            author: t.author,
                            repo_name: t.repo_name,
                        }).collect();
                        HttpResponse::Ok().json(suggestions)
                    }
                    Err(fallback_e) => {
                        error!("Fallback ILIKE search also failed for query '{}': {}", search_query, fallback_e);
                        HttpResponse::InternalServerError().json(Vec::<SearchSuggestion>::new())
                    }
                }
            }
        }
    }
}

struct AppState {
    hf_api: Api,
    download_progress: Mutex<HashMap<String, u64>>,
    total_sizes: Mutex<HashMap<String, u64>>,
    tera: Tera,
    db_pool: PgPool,
    hf_token: Option<String>,
}

#[shuttle_runtime::main]
async fn main(
    #[Secrets] secrets: SecretStore,
) -> ShuttleActixWeb<impl FnOnce(&mut ServiceConfig) + Send + Clone + 'static> {
    // Initialize server directories at startup
    if let Err(e) = ensure_server_directories() {
        error!("Failed to initialize server directories: {}", e);
        // Continue anyway, as we'll try to create directories as needed
    }

    // Get database URL from secrets
    let database_url = secrets
        .get("DATABASE_URL")
        .ok_or_else(|| anyhow::anyhow!("DATABASE_URL must be set in Secrets.toml"))
        .expect("DATABASE_URL not found in secrets");

    // Create a connection pool
    let db_pool = match PgPool::connect(&database_url).await {
        Ok(pool) => {
            info!("Successfully connected to the database");
            pool
        }
        Err(e) => {
            error!("Failed to connect to the database: {}", e);
            panic!("Database connection failed: {}", e); // Or handle more gracefully
        }
    };

    // Run database migrations
    match sqlx::migrate!("./migrations").run(&db_pool).await {
        Ok(_) => info!("Database migrations ran successfully"),
        Err(e) => {
            error!("Failed to run database migrations: {}", e);
            // Depending on the error, you might want to panic or handle it gracefully
            // For now, we log and continue, but this might not be ideal for all migration errors
        }
    }

    // Get HF token from secrets if available
    let hf_token = secrets.get("HF_TOKEN");
    if let Some(_) = hf_token {
        info!("Using HF token for authentication");
    } else {
        info!("No HF token provided, will only work with public repositories");
    }

    let tera = Tera::new("static/**/*.html").expect("Failed to parse templates");
    let app_state = Arc::new(AppState {
        hf_api: Api::new().unwrap(),
        download_progress: Mutex::new(HashMap::new()),
        total_sizes: Mutex::new(HashMap::new()),
        tera,
        db_pool,
        hf_token,
    });
    let config = move |cfg: &mut ServiceConfig| {
        cfg.service(
            web::scope("")
                .service(index)
                .service(Files::new("/static", "static/").index_file("index.html"))
                .service(progress_json)
                .service(repo_info)
                .service(download_torrent_by_sha)
                .service(about_page)
                .service(search_torrents)
                .app_data(web::Data::new(app_state.clone()))
                .app_data(web::Data::new(secrets))
                .wrap(Logger::default()),
        );
    };

    Ok(config.into())
}
