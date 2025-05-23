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
                    if let Some(content_range) = resp.headers().get("Content-Range") {
                        if let Ok(s) = content_range.to_str() {
                            if let Some(size_str) = s.split('/').nth(1) {
                                if let Ok(size) = size_str.parse::<u64>() {
                                    total_size += size;
                                    continue;
                                }
                            }
                        }
                    }
                    if let Some(cl) = resp.content_length() {
                        total_size += cl;
                    }
                } else {
                    info!("Failed to fetch size for: {}", file_info.rfilename);
                }
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

            info!("No existing torrent file found, continuing with download and processing...");

            let full_repo_for_response = full_repo.clone();
            let sha_for_response = info.sha.clone();
            let target_dir_clone = target_dir.clone();
            let info_clone = info.clone();
            let full_repo_clone = full_repo.clone();
            let state_clone = state.clone();
            let seeding_dir_clone = seeding_dir.clone();
            let user_clone_for_spawn = user.clone(); // Clone user
            let repo_clone_for_spawn = repo.clone(); // Clone repo

            tokio::spawn(async move {
                let client = reqwest::Client::builder()
                    .user_agent("muggingface/1.0")
                    .redirect(reqwest::redirect::Policy::limited(20))
                    .build()
                    .unwrap_or_else(|e| {
                        error!("Failed to build reqwest client: {}", e);
                        panic!("Failed to build reqwest client: {}", e);
                    });

                for file_info in &info_clone.siblings {
                    let file_path = target_dir_clone.join(&file_info.rfilename);
                    if let Some(parent) = file_path.parent() {
                        if let Err(e) = ensure_directory_exists(parent) {
                            error!(
                                "Failed to create parent directory {}: {}",
                                parent.display(),
                                e
                            );
                            continue;
                        }
                    }

                    let download_url = format!(
                        "https://huggingface.co/{}/resolve/main/{}",
                        full_repo_clone, file_info.rfilename
                    );
                    
                    // state_clone.hf_api.authorization_header(); // ERROR: No such method - Commented out
                    // let auth_header_val = state_clone.hf_api.authorization_header(); 

                    let request_builder = client.get(&download_url);
                    // if let Some(auth_val) = &auth_header_val { // Commented out as auth_header_val is commented out
                    //     request_builder = request_builder.header(reqwest::header::AUTHORIZATION, auth_val);
                    // }
                    
                    info!("Starting download for {} from URL: {}", file_info.rfilename, download_url);

                    match request_builder.send().await {
                        Ok(mut response) => {
                            if response.status().is_success() {
                                match File::create(&file_path).await {
                                    Ok(mut dest_file) => {
                                        let mut file_downloaded_successfully = true;
                                        // Corrected loop structure for handling reqwest stream
                                        loop {
                                            match response.chunk().await { // This returns Result<Option<Bytes>>
                                                Ok(Some(chunk)) => { // chunk is reqwest::Bytes here
                                                    if let Err(e) = dest_file.write_all(&chunk).await { // &chunk is fine, Bytes AsRef [u8]
                                                        error!("Failed to write chunk to {}: {}", file_path.display(), e);
                                                        file_downloaded_successfully = false;
                                                        break; 
                                                    }
                                                    let chunk_len = chunk.len() as u64;
                                                    {
                                                        let mut progress_map = state_clone.download_progress.lock().unwrap_or_else(|poisoned| {
                                                            error!("download_progress mutex poisoned: {}", poisoned);
                                                            poisoned.into_inner()
                                                        });
                                                        *progress_map.entry(full_repo_clone.clone()).or_default() += chunk_len;
                                                        info!("Repo {}: downloaded {} bytes for file {}, total {} for repo", 
                                                              full_repo_clone, chunk_len, file_info.rfilename, 
                                                              progress_map.get(&full_repo_clone).unwrap_or(&0));
                                                    }
                                                }
                                                Ok(None) => { // End of stream signifies success for this file
                                                    info!("Finished streaming file successfully: {}", file_info.rfilename);
                                                    break; // Exit the loop for this file
                                                }
                                                Err(e) => { // An error occurred while streaming chunks
                                                    error!("Error while streaming chunk for {}: {}", file_info.rfilename, e);
                                                    file_downloaded_successfully = false;
                                                    break; // Exit the loop for this file
                                                }
                                            }
                                        }
                                        // After the loop, check file_downloaded_successfully
                                        if file_downloaded_successfully {
                                            info!("Successfully downloaded and wrote file: {}", file_path.display());
                                        } else {
                                            error!("Failed to complete download for file: {}. It might be partial.", file_path.display());
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to create file {}: {}", file_path.display(), e);
                                    }
                                }
                            } else {
                                error!("Download request for {} failed with status: {}. Body: {:?}", 
                                       download_url, response.status(), response.text().await.unwrap_or_else(|_| String::from("[could not read body]")));
                            }
                        }
                        Err(e) => {
                            error!("Failed to send download request for {}: {}", download_url, e);
                        }
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
                    "Creating torrent from folder: {}",
                    target_dir_clone.display()
                );
                let torrent_file = match librqbit::create_torrent(&target_dir_clone, options).await
                {
                    Ok(tf) => tf,
                    Err(e) => {
                        error!(
                            "Failed to create torrent for {}: {}",
                            target_dir_clone.display(),
                            e
                        );
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

#[derive(sqlx::FromRow)] // Helper struct for fetching torrent file and repo name
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

    let tera = Tera::new("static/**/*.html").expect("Failed to parse templates");
    let app_state = Arc::new(AppState {
        hf_api: Api::new().unwrap(),
        download_progress: Mutex::new(HashMap::new()),
        total_sizes: Mutex::new(HashMap::new()),
        tera,
        db_pool,
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
