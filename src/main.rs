use actix_files::{Files, NamedFile};
use actix_web::middleware::Logger;
use actix_web::{
    get,
    web::{self, ServiceConfig, Json},
    Error as ActixError, HttpResponse, Responder
};
use shuttle_actix_web::ShuttleActixWeb;

// aws
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::{
    config::{Builder as S3ConfigBuilder, Credentials, Region},
    primitives::ByteStream,
    Client,
    types::ObjectCannedAcl,
};

// file system + io
use directories::BaseDirs;
use std::{
    borrow::Cow,
    collections::HashMap,
    env,
    error::Error,
    fs::File as StdFile,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};
use walkdir::WalkDir;
use zip::{ZipWriter, write::FileOptions};
// misc
// use anyhow::Result;
use bendy::{decoding::FromBencode, encoding::ToBencode, value::Value};
use hf_hub::api::sync::Api;
use hf_hub::{Repo, RepoType};
use librqbit;
use reqwest;
use shuttle_runtime::{SecretStore, Secrets};
use tera::{Context, Tera};
use tracing::{info, error};
use sha1::{Sha1, Digest};
use hex;
use urlencoding;

const DATA_DIR: &str = "data";
const TORRENTS_DIR: &str = "/home/youruser/watch";
const SEEDING_DIR: &str = "/home/youruser/seeding";
const BUCKET_NAME: &str = "gerbiltestman";

fn get_base_dir() -> Result<PathBuf, Box<dyn Error>> {
    let base_dirs = BaseDirs::new()
        .ok_or_else(|| "Could not determine home directory")?;
    Ok(base_dirs.home_dir().join(DATA_DIR))
}

fn initialize_server_directories() -> Result<(), Box<dyn Error>> {
    let base_dir = get_base_dir()?;
    let torrents_dir = PathBuf::from(TORRENTS_DIR);
    
    for dir in &[&base_dir, &torrents_dir] {
        if !dir.exists() {
            info!("Creating directory: {}", dir.display());
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("Failed to create directory {}: {}", dir.display(), e))?;
        }
    }
    info!("Server directories initialized successfully");
    Ok(())
}

fn ensure_directory_exists(path: &Path) -> Result<(), Box<dyn Error>> {
    if !path.exists() {
        info!("Creating directory: {}", path.display());
        std::fs::create_dir_all(path)
            .map_err(|e| format!("Failed to create directory {}: {}", path.display(), e))?;
    }
    Ok(())
}

fn render_loading_html_response(full_repo: &str, sha: &str, tera: &Tera) -> String {
    let mut context = Context::new();
    context.insert("full_repo", full_repo);
    context.insert("sha", sha);
    tera.render("loading.html", &context).expect("Failed to render loading template")
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
    tera.render("finished.html", &context).expect("Failed to render finished template")
}

#[get("/")]
async fn index() -> impl Responder {
    NamedFile::open(PathBuf::from("static/index.html"))
}

async fn r2_object_exists(
    object_key: &str,
    state: &Arc<AppState>,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    info!("pre load upload");
    let access_key = &state.api_key;
    let secret_key = &state.api_key2;
    let account_id = &state.api_key3;
    info!("post load upload");

    // endpoint + bucket name
    let endpoint = format!("https://{}.r2.cloudflarestorage.com", account_id);

    let shared_config = aws_config::from_env()
        .credentials_provider(
            aws_sdk_s3::config::Credentials::new(
                access_key,
                secret_key,
                None, // no session token
                None,
                "custom",
            )
        )
        .region("auto")
        .load()
        .await;
    
    info!("config loaded");

    let s3_conf = S3ConfigBuilder::from(&shared_config)
        .endpoint_url(endpoint)
        .force_path_style(true)
        .build();
    let client = Client::from_conf(s3_conf);
    // finding the object
    info!("pre head object");
    match client
        .head_object()
        .bucket(BUCKET_NAME)
        .key(object_key)
        .send()
        .await
    {
        Ok(_) => {
            info!("object exists");
            Ok(true)
        },
        // Err(e) if e.to_string().contains("NotFound") ||
        // e.to_string().contains("NoSuchKey") ||
        // e.to_string().contains("404") ||
        // e.to_string().contains("NoSuchObject") ||
        // e.to_string().contains("ResourceNotFound") => {
        //     info!("object not found: {}", e);
        //     Ok(false)
        // }
        Err(e) => {
            info!("error checking object: {}", e.to_string());
          //  Err(Box::new(e))
          Ok(false)
        }
    }
}

async fn r2_upload(
    file_path: &str,
    state: &Arc<AppState>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let access_key = &state.api_key;
    let secret_key = &state.api_key2;
    let account_id = &state.api_key3;
    
    let bucket_name = "gerbiltestman";
    let endpoint_str = format!("https://{}.r2.cloudflarestorage.com", account_id);

    let region_provider = RegionProviderChain::first_try(Region::new("auto"))
        .or_default_provider()
        .or_else(Region::new("us-east-1"));
    let shared_config = aws_config::from_env()
        .region(region_provider)
        .credentials_provider(
            Credentials::new(
                access_key.clone(),
                secret_key.clone(),
                None,
                None,
                "custom"
            )
        )
        .load()
        .await;

    let s3_config = S3ConfigBuilder::from(&shared_config)
        .endpoint_url(endpoint_str.clone())
        .force_path_style(true)
        .build();
    let client = Client::from_conf(s3_config);

    let data = fs::read(file_path).await?;
    let object_key = Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Invalid filename")?;

    client
        .put_object()
        .bucket(bucket_name)
        .key(object_key)
        .body(ByteStream::from(data.clone()))
        .acl(ObjectCannedAcl::Private)
        .send()
        .await?;

    let file_url = format!("{}/{}/{}", endpoint_str, bucket_name, object_key);

    Ok(file_url)
}

async fn r2_upload_folder(
    folder_to_upload: &Path,
    r2_base_key_prefix: &str,
    state: &Arc<AppState>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    info!(
        "Starting upload of folder {} to R2 with base key prefix: {}",
        folder_to_upload.display(),
        r2_base_key_prefix
    );

    let access_key = &state.api_key;
    let secret_key = &state.api_key2;
    let account_id = &state.api_key3;
    let endpoint_str = format!("https://{}.r2.cloudflarestorage.com", account_id);

    let region_provider = RegionProviderChain::first_try(Region::new("auto"))
        .or_default_provider()
        .or_else(Region::new("us-east-1"));
    let shared_config = aws_config::from_env()
        .region(region_provider)
        .credentials_provider(Credentials::new(
            access_key.clone(),
            secret_key.clone(),
            None,
            None,
            "custom",
        ))
        .load()
        .await;

    let s3_config = S3ConfigBuilder::from(&shared_config)
        .endpoint_url(endpoint_str.clone())
        .force_path_style(true)
        .build();
    let client = Client::from_conf(s3_config);

    for entry in WalkDir::new(folder_to_upload).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.is_file() {
            let relative_path = match path.strip_prefix(folder_to_upload) {
                Ok(rp) => rp,
                Err(e) => {
                    error!(
                        "Failed to get relative path for {} from base {}: {}",
                        path.display(),
                        folder_to_upload.display(),
                        e
                    );
                    continue;
                }
            };

            let object_key = format!(
                "{}",
                Path::new(r2_base_key_prefix)
                    .join(relative_path)
                    .to_string_lossy()
            );

            info!("Uploading file {} to R2 key: {}", path.display(), object_key);

            match fs::read(path).await {
                Ok(data) => {
                    match client
                        .put_object()
                        .bucket(BUCKET_NAME)
                        .key(&object_key) // Pass as &str
                        .body(ByteStream::from(data))
                        .acl(ObjectCannedAcl::Private)
                        .send()
                        .await
                    {
                        Ok(_) => info!(
                            "Successfully uploaded {} to R2 key: {}",
                            path.display(),
                            object_key
                        ),
                        Err(e) => error!(
                            "Failed to upload {} to R2 key {}: {}",
                            path.display(),
                            object_key,
                            e
                        ),
                    }
                }
                Err(e) => {
                    error!("Failed to read file {} for R2 upload: {}", path.display(), e);
                }
            }
        }
    }
    info!("Finished uploading folder {}.", folder_to_upload.display());
    Ok(())
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
            let base_dir = match get_base_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    error!("Failed to get base directory: {}", e);
                    return HttpResponse::InternalServerError()
                        .body("Failed to access data directory");
                }
            };
            
            let torrents_dir = PathBuf::from(TORRENTS_DIR);
            let seeding_dir = PathBuf::from(SEEDING_DIR);
            
            // Create directories if they don't exist
            for dir in &[&base_dir, &torrents_dir, &seeding_dir] {
                if let Err(e) = ensure_directory_exists(dir) {
                    error!("Failed to create directory {}: {}", dir.display(), e);
                    return HttpResponse::InternalServerError()
                        .body(format!("Failed to create required directory: {}", e));
                }
            }

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
            {
                let mut progress = state.download_progress.lock().unwrap();
                progress.insert(full_repo.clone(), 0);
            }
            {
                let mut sizes = state.total_sizes.lock().unwrap();
                sizes.insert(full_repo.clone(), total_size);
            }
            
            let file_names: Vec<String> = info.siblings.iter().map(|f| f.rfilename.clone()).collect();
            
            let target_dir = seeding_dir.join(format!("{}-{}", full_repo.replace("/", "-"), info.sha));
            
            if let Err(e) = ensure_directory_exists(&target_dir) {
                error!("Failed to create target directory {}: {}", target_dir.display(), e);
                return HttpResponse::InternalServerError()
                    .body(format!("Failed to create target directory: {}", e));
            }

            // Check if all files already exist
            let all_files_exist = info.siblings.iter().all(|file_info| {
                let file_path = target_dir.join(&file_info.rfilename);
                file_path.exists()
            });

            if all_files_exist {
                info!("All files already exist in {}, generating magnet link", target_dir.display());
                let torrent_path = torrents_dir.join(format!("{}.torrent", info.sha));
                
                // Generate torrent and magnet link
                let torrent_name = target_dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let options = librqbit::CreateTorrentOptions {
                    name: Some(&torrent_name),
                    piece_length: Some(1_048_576),
                };

                match librqbit::create_torrent(&target_dir, options).await {
                    Ok(torrent_file) => {
                        if let Ok(torrent_bytes) = torrent_file.as_bytes() {
                            if let Err(e) = std::fs::write(&torrent_path, &torrent_bytes) {
                                error!("Failed to write torrent file {}: {}", torrent_path.display(), e);
                            } else {
                                let bencode_value = match Value::from_bencode(&torrent_bytes) {
                                    Ok(val) => val,
                                    Err(e) => {
                                        error!("Failed to parse bencode from torrent bytes: {}", e);
                                        return HttpResponse::InternalServerError().body("Failed to generate magnet link");
                                    }
                                };
                                let info_dict = match bencode_value {
                                    Value::Dict(d) => match d.get(&b"info"[..]) {
                                        Some(val) => val.clone(),
                                        None => {
                                            error!("No 'info' dict found in torrent bencode");
                                            return HttpResponse::InternalServerError().body("Failed to generate magnet link");
                                        }
                                    },
                                    _ => {
                                        error!("Invalid torrent format: not a dictionary at root");
                                        return HttpResponse::InternalServerError().body("Failed to generate magnet link");
                                    }
                                };
                                let info_bytes = match info_dict.to_bencode() {
                                    Ok(bytes) => bytes,
                                    Err(e) => {
                                        error!("Failed to re-encode info_dict: {}", e);
                                        return HttpResponse::InternalServerError().body("Failed to generate magnet link");
                                    }
                                };
                                let info_hash = Sha1::digest(&info_bytes);
                                let display_name = format!("{}-{}", full_repo.replace("/", "-"), info.sha);
                                let magnet_link_str = format!("magnet:?xt=urn:btih:{}&dn={}", 
                                    hex::encode(info_hash), 
                                    urlencoding::encode(&display_name)
                                );

                                {
                                    let mut magnet_links_guard = state.magnet_links.lock().unwrap();
                                    magnet_links_guard.insert(full_repo.clone(), magnet_link_str.clone());
                                }

                                return HttpResponse::Ok().content_type("text/html").body(
                                    render_finished_html_response(&full_repo, &info.sha, &file_names, &magnet_link_str, &state.tera),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to create torrent for {}: {}", target_dir.display(), e);
                    }
                }
            }

            let torrent_r2_key = format!("{}.torrent", info.sha);
            match r2_object_exists(&torrent_r2_key, &state).await {
                Ok(true) => {
                    info!("Torrent file {} already exists in R2, skipping processing.", torrent_r2_key);
                    let magnet_link: String = format!("https://jerboa.muggingface.co/{}.torrent", info.sha);

                    return HttpResponse::Ok().content_type("text/html").body(
                        render_finished_html_response(&full_repo, &info.sha, &file_names, &magnet_link, &state.tera),
                    );
                }
                Ok(false) => {
                    info!("Torrent file {} not found in R2, continuing with download and processing...", torrent_r2_key);
                }
                Err(e) => {
                    error!("Error checking R2 for torrent {}: {}", torrent_r2_key, e);
                    return HttpResponse::InternalServerError().body("Error checking R2 storage");
                }
            }

            let full_repo_for_response = full_repo.clone();
            let sha_for_response = info.sha.clone();
            let target_dir_clone = target_dir.clone();
            let info_clone = info.clone();
            let full_repo_clone = full_repo.clone();
            let state_clone = state.clone();
            let torrents_dir_clone = torrents_dir.clone();
            
            tokio::spawn(async move {
                // Create API instance for this thread
                let api = Api::new().unwrap();
                let model = api.model(full_repo_clone.clone());

                // Download all files to the target directory
                for file_info in &info_clone.siblings {
                    let file_path = target_dir_clone.join(&file_info.rfilename);
                    if let Some(parent) = file_path.parent() {
                        if let Err(e) = ensure_directory_exists(parent) {
                            error!("Failed to create parent directory {}: {}", parent.display(), e);
                            continue;
                        }
                    }

                    // Download file using hf-hub API
                    match model.get(&file_info.rfilename) {
                        Ok(cache_path) => {
                            // Copy from cache to target directory
                            if let Err(e) = std::fs::copy(&cache_path, &file_path) {
                                error!("Failed to copy file from cache to target: {}", e);
                                continue;
                            }
                            
                            // Update progress
                            let file_len = match fs::metadata(&file_path).await {
                                Ok(metadata) => metadata.len(),
                                Err(e) => {
                                    error!("Failed to get metadata for file {}: {}", file_path.display(), e);
                                    0
                                }
                            };

                            {
                                let mut progress = match state_clone.download_progress.lock() {
                                    Ok(guard) => guard,
                                    Err(_) => {
                                        error!("Mutex was poisoned for download_progress, skipping progress update");
                                        return;
                                    }
                                };
                                if let Some(downloaded) = progress.get_mut(&full_repo_clone) {
                                    *downloaded += file_len;
                                    info!("Updated progress for {}: {} bytes", full_repo_clone, *downloaded);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to download file {}: {}", file_info.rfilename, e);
                            continue;
                        }
                    }
                }

                let torrent_path = torrents_dir_clone.join(format!("{}.torrent", info_clone.sha));
                let torrent_name = target_dir_clone.file_name().unwrap_or_default().to_string_lossy().into_owned();

                let options = librqbit::CreateTorrentOptions {
                    name: Some(&torrent_name),
                    piece_length: Some(1_048_576),
                };

                info!("Creating torrent from folder: {}", target_dir_clone.display());
                let torrent_file = match librqbit::create_torrent(&target_dir_clone, options).await {
                    Ok(tf) => tf,
                    Err(e) => {
                        error!("Failed to create torrent for {}: {}", target_dir_clone.display(), e);
                        return;
                    }
                };

                let torrent_bytes = match torrent_file.as_bytes() {
                    Ok(tb) => tb,
                    Err(e) => {
                        error!("Failed to get torrent bytes for {}: {}", target_dir_clone.display(), e);
                        return;
                    }
                };
                let torrent_bytes_clone = torrent_bytes.clone();

                if let Err(e) = std::fs::write(&torrent_path, &torrent_bytes_clone) {
                    error!("Failed to write torrent file {}: {}", torrent_path.display(), e);
                    return;
                }
                info!("Torrent file created at: {}", torrent_path.display());

                let r2_folder_key_prefix = format!("{}-{}", full_repo_clone.replace("/", "-"), info_clone.sha);
                info!("Uploading folder contents {} to R2 under prefix {}...", target_dir_clone.display(), r2_folder_key_prefix);
                if let Err(e) = r2_upload_folder(&target_dir_clone, &r2_folder_key_prefix, &state_clone).await {
                     error!("Failed to upload folder {} to R2: {}", target_dir_clone.display(), e);
                }

                info!("Uploading torrent file {} to R2...", torrent_path.display());
                match r2_upload(&torrent_path.to_string_lossy(), &state_clone).await {
                    Ok(url) => info!("Successfully uploaded torrent file to: {}", url),
                    Err(e) => error!("Failed to upload torrent file {} to R2: {}", torrent_path.display(), e),
                }

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
                let display_name = format!("{}-{}", full_repo_clone.replace("/", "-"), info_clone.sha);

                let magnet_link_str = format!("magnet:?xt=urn:btih:{}&dn={}", 
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
                render_loading_html_response(&full_repo_for_response, &sha_for_response, &state.tera),
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

    fn get_from_map<'a>(map: &'a Mutex<HashMap<String, u64>>, key: &str, default: u64) -> Result<u64, PoisonError<std::sync::MutexGuard<'a, HashMap<String, u64>>>> {
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

    info!(
    "Progress for {}: {}/{} bytes", 
    full_repo, 
    downloaded, 
    total
    );
    
    Ok(web::Json(Progress {
        downloaded,
        total,
    }))
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
    api_key: String,
    api_key2: String,
    api_key3: String,
}

#[shuttle_runtime::main]
async fn main(#[Secrets] secrets: shuttle_runtime::SecretStore) -> ShuttleActixWeb<impl FnOnce(&mut ServiceConfig) + Send + Clone + 'static> {
    // Initialize server directories at startup
    if let Err(e) = initialize_server_directories() {
        error!("Failed to initialize server directories: {}", e);
        // Continue anyway, as we'll try to create directories as needed
    }

    let api_key = secrets.get("AWS_ACCESS_KEY_ID").expect("Missing API key");
    let api_key2 = secrets.get("AWS_SECRET_ACCESS_KEY").expect("Missing API key");
    let api_key3 = secrets.get("CLOUDFLARE_ACCOUNT_ID").expect("Missing API key");
    let tera = Tera::new("static/**/*.html").expect("Failed to parse templates");
    let app_state = Arc::new(AppState {
        hf_api: Api::new().unwrap(),
        magnet_links: Mutex::new(HashMap::new()),
        download_progress: Mutex::new(HashMap::new()),
        total_sizes: Mutex::new(HashMap::new()),
        tera,
        api_key,
        api_key2,
        api_key3,
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
