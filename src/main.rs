use actix_files::{Files, NamedFile};
use actix_web::middleware::Logger;
use actix_web::{
    get,
    web::{self, Json, ServiceConfig},
    Error as ActixError, HttpResponse, Responder,
};
use shuttle_actix_web::ShuttleActixWeb;

// file system + io
use directories::BaseDirs;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};
use tokio::fs::{self};

// misc
use anyhow::Result;
use bendy::{decoding::FromBencode, encoding::ToBencode, value::Value};
use hex;
use hf_hub::api::sync::Api;
use librqbit;
use reqwest;
use sha1::{Digest, Sha1};
use shuttle_runtime::{SecretStore, Secrets};
use tera::{Context, Tera};
use tracing::{error, info};
use urlencoding;

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
        std::fs::create_dir_all(dir_path).map_err(|e| anyhow::anyhow!("Failed to create directory {}: {}", dir_path.display(), e))?;
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
async fn index() -> impl Responder {
    NamedFile::open(PathBuf::from("static/index.html"))
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

                                {
                                    let mut magnet_links_guard = state.magnet_links.lock().unwrap();
                                    magnet_links_guard
                                        .insert(full_repo.clone(), magnet_link_str.clone());
                                }

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

                        // store it in the shared state
                        {
                            let mut magnet_links_guard = state.magnet_links.lock().unwrap();
                            magnet_links_guard.insert(full_repo.clone(), magnet_link_str.clone());
                            info!("Magnet link for {} (from local torrent) stored.", full_repo);
                        }

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

            tokio::spawn(async move {
                let api = Api::new().unwrap();
                let model = api.model(full_repo_clone.clone());

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

                    match model.get(&file_info.rfilename) {
                        Ok(cache_path) => {
                            if let Err(e) = std::fs::copy(&cache_path, &file_path) {
                                error!("Failed to copy file from cache to target: {}", e);
                                continue;
                            }

                            let file_len = match fs::metadata(&file_path).await {
                                Ok(metadata) => metadata.len(),
                                Err(e) => {
                                    error!(
                                        "Failed to get metadata for file {}: {}",
                                        file_path.display(),
                                        e
                                    );
                                    0
                                }
                            };
                        }
                        Err(e) => {
                            error!("Failed to download file {}: {}", file_info.rfilename, e);
                            continue;
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

                {
                    let mut magnet_links_guard = state_clone.magnet_links.lock().unwrap();
                    magnet_links_guard.insert(full_repo_clone.clone(), magnet_link_str);
                    info!("Magnet link for {} stored.", full_repo_clone);
                }
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

// Helper function to calculate directory size (recursive)
async fn calculate_directory_size(path: &Path) -> std::io::Result<u64> {
    let mut total_size = 0;
    
    if !path.exists() { // Early exit if path doesn't exist
        return Ok(0);
    }

    if path.is_file() { // If it's a file (e.g. called on a file path directly), return its size
        let metadata = tokio::fs::metadata(path).await?;
        return Ok(metadata.len());
    }

    if path.is_dir() {
        let mut dir_entries = match tokio::fs::read_dir(path).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0), // Dir disappeared
            Err(e) => return Err(e), // Other read_dir error
        };
        while let Some(entry_result) = dir_entries.next_entry().await? { // Propagates IO errors
            let entry_path = entry_result.path();
            if entry_path.is_dir() {
                total_size += Box::pin(calculate_directory_size(&entry_path)).await?;
            } else if entry_path.is_file() {
                match tokio::fs::metadata(&entry_path).await {
                    Ok(metadata) => total_size += metadata.len(),
                    Err(e) => {
                        // Log error but continue calculation for other files
                        error!("Failed to get metadata for file {} during size calculation: {}", entry_path.display(), e);
                    }
                }
            }
        }
    }
    Ok(total_size)
}

// this is where we do the progress work, and the unwrapping the values
#[get("/{user}/{repo}/progress_json")]
async fn progress_json(
    path: web::Path<(String, String)>,
    state: web::Data<Arc<AppState>>,
) -> Result<Json<Progress>, ActixError> {
    let (user, repo) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);

    let target_dir_path_option = { // Scope the lock guard
        match state.repo_target_paths.lock() {
            Ok(guard) => guard.get(&full_repo).cloned(),
            Err(poison_error) => {
                error!("Mutex for repo_target_paths was poisoned for repo {}: {}", full_repo, poison_error);
                None // Treat as if path is not found
            }
        }
    };

    let downloaded = if let Some(target_dir_path) = target_dir_path_option {
        match calculate_directory_size(&target_dir_path).await {
            Ok(size) => size,
            Err(e) => {
                error!("Error calculating directory size for {}: {}", target_dir_path.display(), e);
                0 // Default to 0 if calculation fails
            }
        }
    } else {
        // Path not found in map (e.g., download not started/recorded) or mutex poisoned
        info!("No target directory path found for {} in progress_json, assuming 0 downloaded.", full_repo);
        0
    };

    let total = match state.total_sizes.lock() {
        Ok(guard) => guard.get(&full_repo).copied().unwrap_or(1), // Default to 1 to prevent division by zero
        Err(poison_error) => {
            error!("Mutex for total_sizes was poisoned for repo {}: {}", full_repo, poison_error);
            1 // Default to 1 in case of poison error
        }
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

// #[derive(Clone)]
struct AppState {
    hf_api: Api,
    magnet_links: Mutex<HashMap<String, String>>,
    repo_target_paths: Mutex<HashMap<String, PathBuf>>,
    total_sizes: Mutex<HashMap<String, u64>>,
    tera: Tera,
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

    let tera = Tera::new("static/**/*.html").expect("Failed to parse templates");
    let app_state = Arc::new(AppState {
        hf_api: Api::new().unwrap(),
        magnet_links: Mutex::new(HashMap::new()),
        repo_target_paths: Mutex::new(HashMap::new()),
        total_sizes: Mutex::new(HashMap::new()),
        tera,
    });
    let config = move |cfg: &mut ServiceConfig| {
        cfg.service(
            web::scope("")
                .service(index)
                .service(Files::new("/static", "static/").index_file("index.html"))
                .service(progress_json)
                .service(repo_info)
                .app_data(web::Data::new(app_state.clone()))
                .app_data(web::Data::new(secrets))
                .wrap(Logger::default()),
        );
    };

    Ok(config.into())
}
