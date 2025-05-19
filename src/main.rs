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
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;

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

// #[derive(Clone)]
struct AppState {
    hf_api: Api,
    magnet_links: Mutex<HashMap<String, String>>,
    download_progress: Mutex<HashMap<String, u64>>,
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
        download_progress: Mutex::new(HashMap::new()),
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
