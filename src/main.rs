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
use librqbit;
use reqwest;
use shuttle_runtime::{SecretStore, Secrets};
use tera::{Context, Tera};
use tracing::{info, error};

const DATA_DIR: &str = "data";
const TORRENTS_DIR: &str = "torrents";
const ZIPS_DIR: &str = "zips";
const BUCKET_NAME: &str = "gerbiltestman";

fn get_base_dir() -> Result<PathBuf, Box<dyn Error>> {
    let base_dirs = BaseDirs::new()
        .ok_or_else(|| "Could not determine home directory")?;
    Ok(base_dirs.home_dir().join(DATA_DIR))
}

fn initialize_server_directories() -> Result<(), Box<dyn Error>> {
    let base_dir = get_base_dir()?;
    let torrents_dir = base_dir.join(TORRENTS_DIR);
    let zips_dir = base_dir.join(ZIPS_DIR);
    
    for dir in &[&base_dir, &torrents_dir, &zips_dir] {
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

#[get("/{user}/{repo}{tail:.*}")]
async fn repo_info(
    path: web::Path<(String, String, String)>,
    state: web::Data<Arc<AppState>>,
) -> impl Responder {
    let (user, repo, _tail) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);
    info!("Requesting repo info for {}", full_repo);
    let repo = state.hf_api.model(full_repo.clone());
    match repo.info() {
        Ok(info) => {
            // Get base directory from user's home
            let base_dir = match get_base_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    error!("Failed to get base directory: {}", e);
                    return HttpResponse::InternalServerError()
                        .body("Failed to access data directory");
                }
            };
            
            let torrents_dir = base_dir.join(TORRENTS_DIR);
            let zips_dir = base_dir.join(ZIPS_DIR);
            
            // Create directories if they don't exist
            for dir in &[&base_dir, &torrents_dir, &zips_dir] {
                if let Err(e) = ensure_directory_exists(dir) {
                    error!("Failed to create directory {}: {}", dir.display(), e);
                    return HttpResponse::InternalServerError()
                        .body(format!("Failed to create required directory: {}", e));
                }
            }

            // getting the total size of the repo
            let mut total_size = 0u64;
            let zip_name = format!("{}-{}.zip", full_repo.replace("/", "-"), info.sha);
            for file in &info.siblings {
                let url = format!(
                    "https://huggingface.co/{}/resolve/main/{}?download=true",
                    full_repo, file.rfilename
                );
                // building client for getting the size of the file
                let client = reqwest::Client::builder()
                    .user_agent("muggingface/1.0")
                    .redirect(reqwest::redirect::Policy::limited(20))
                    .build()
                    .unwrap();
                // sending request + header
                let res = client.get(&url).header("Range", "bytes=0-0").send().await;
                // checking if the request was successful
                if let Ok(resp) = res {
                    // going into header
                    if let Some(content_range) = resp.headers().get("Content-Range") {
                        // converting to string
                        if let Ok(s) = content_range.to_str() {
                            if let Some(size_str) = s.split('/').nth(1) {
                                // parsing the size
                                if let Ok(size) = size_str.parse::<u64>() {
                                    total_size += size;
                                    continue;
                                }
                            }
                        }
                    }
                    // fallback size if needed
                    if let Some(cl) = resp.content_length() {
                        total_size += cl;
                    }
                } else {
                    info!("Failed to fetch size for: {}", file.rfilename);
                }
            }
            // start download progress, saving it in memory
            {
                let mut progress = state.download_progress.lock().unwrap();
                progress.insert(full_repo.clone(), 0); // start at 0 bytes
            }

            // saving the total size
            {
                let mut sizes = state.total_sizes.lock().unwrap();
                sizes.insert(full_repo.clone(), total_size);
            }
            
            let file_names: Vec<String> = info.siblings.iter().map(|f| f.rfilename.clone()).collect();
            
            // Use server-appropriate paths
            let target_dir = base_dir.join(format!("{}-{}", full_repo.replace("/", "-"), info.sha));
            
            // Ensure target directory exists
            if let Err(e) = ensure_directory_exists(&target_dir) {
                error!("Failed to create target directory {}: {}", target_dir.display(), e);
                return HttpResponse::InternalServerError()
                    .body(format!("Failed to create target directory: {}", e));
            }

            match r2_object_exists(&zip_name, &state).await {
                Ok(true) => {
                    info!("File already exists in R2, skipping upload");
                    let magnet: String = format!("https://gerbil.muggingface.co/{}.torrent", info.sha);

                    return HttpResponse::Ok().content_type("text/html").body(
                        render_finished_html_response(&full_repo, &info.sha, &file_names, &magnet, &state.tera),
                    );
                }
                Ok(false) => {
                    info!("File not found in R2, continuing with download...");
                }
                Err(e) => {
                    error!("Error checking R2: {}", e);
                    return HttpResponse::InternalServerError().body("Error checking R2 storage");
                }
            }

            let full_repo_for_response = full_repo.clone();
            let sha_for_response = info.sha.clone();
            let target_dir_clone = target_dir.clone();
            let info_clone = info.clone();
            let full_repo_clone = full_repo.clone();
            let state_clone = state.clone();
            
            tokio::spawn(async move {
                for file in &info_clone.siblings {
                    // checking / setting directories
                    let file_path = target_dir_clone.join(&file.rfilename);

                    if let Some(parent) = file_path.parent() {
                        if let Err(e) = ensure_directory_exists(parent) {
                            error!("Failed to create parent directory {}: {}", parent.display(), e);
                            continue;
                        }
                    }

                    // actual download time (get the website, download the file, create the file, write the file.)
                    let url = format!(
                        "https://huggingface.co/{}/resolve/main/{}?download=true",
                        full_repo_clone, file.rfilename
                    );
                    let response = reqwest::get(&url).await.expect("Failed to access file");

                    let bytes = response.bytes().await.expect("Failed to download file");
                    let mut file = File::create(&file_path)
                        .await
                        .expect("Failed to create file");
                    file.write_all(&bytes).await.expect("Failed to write file");

                    {
                        let mut progress = match state_clone.download_progress.lock() {
                            Ok(guard) => guard,
                            Err(_) => {
                                error!("Mutex was poisoned, skipping progress update");
                                return;
                            }
                        };
                        if let Some(downloaded) = progress.get_mut(&full_repo_clone) {
                            *downloaded += bytes.len() as u64;
                            info!("Updated progress for {}: {} bytes", full_repo_clone, *downloaded);
                        }
                    }
                }

                // Use the server-appropriate paths for torrents and zips
                let torrent_path = torrents_dir.join(format!("{}.torrent", info_clone.sha));
                let zip_path = zips_dir.join(&zip_name);

                if let Some(torrent_parent) = torrent_path.parent() {
                    if let Err(e) = ensure_directory_exists(torrent_parent) {
                        error!("Failed to create torrent directory {}: {}", torrent_parent.display(), e);
                        return;
                    }
                }

                let zip_file = StdFile::create(&zip_path).expect("Failed to create zip file");
                let mut zip = ZipWriter::new(zip_file);
                let options: FileOptions<()> = FileOptions::default().compression_method(zip::CompressionMethod::Stored);
                
                for entry in WalkDir::new(&target_dir_clone)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path() != zip_path)
                {
                    let path = entry.path();
                    if path.is_file() {
                        let name_in_zip = path
                            .strip_prefix(&target_dir_clone)
                            .unwrap()
                            .to_string_lossy();
                        zip.start_file(name_in_zip, options).expect("Failed to start file in zip");
                        let mut f = StdFile::open(path).expect("Failed to open file for zipping");
                        std::io::copy(&mut f, &mut zip).expect("Failed to write file into zip");
                    }
                }
                
                zip.finish().expect("Failed to finish zip file");
                
                info!("Attempting to upload zip file: {}", zip_path.to_string_lossy());
                let zip_url = r2_upload(
                    &zip_path.to_string_lossy(),
                    &state_clone
                ).await;

                match &zip_url {
                    Ok(url) => info!("Successfully uploaded zip to: {}", url),
                    Err(e) => info!(
                        "Failed to upload zip to R2: {} for file {}",
                        e,
                        zip_path.to_string_lossy()
                    ),
                }

                // creating the torrent
                let bruh2 = zip_name.clone();

                let options = librqbit::CreateTorrentOptions {
                    name: Some(&bruh2),
                    piece_length: Some(1_048_576),
                };

                let zip_metadata = std::fs::metadata(&zip_path).expect("Failed to get zip metadata");
                info!("Zip file size: {} bytes", zip_metadata.len());

                info!("Creating torrent from zip file: {}", zip_path.to_string_lossy());
                let torrent_file = librqbit::create_torrent(&zip_path, options)
                    .await
                    .expect("Failed to create torrent");

                let torrent_bytes = torrent_file
                    .as_bytes()
                    .expect("Failed to get torrent bytes");
                let bytes_clone = torrent_bytes.clone();

                std::fs::write(&torrent_path, torrent_bytes).expect("Failed to write torrent file");

                info!("Attempting to upload torrent file: {}", torrent_path.to_string_lossy());
                match r2_upload(
                    &torrent_path.to_string_lossy(),   // or zip_path.to_str().unwrap()
                    &state_clone
                ).await {
                    Ok(url) => info!("Successfully uploaded zip to: {}", url),
                    Err(e) => info!(
                        "Failed to upload zip to R2: {} for file {}",
                        e,
                        torrent_path.to_string_lossy()
                    ),
                }

                let bencode_value = Value::from_bencode(&bytes_clone).expect("Failed to parse bencode");

                let file_name = zip_path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .expect("Failed to get file name");

                let web_seed_url = format!("https://gerbil.muggingface.co/{}", file_name);

                let mut dict = match bencode_value {
                    Value::Dict(d) => d,
                    _ => panic!("Invalid torrent format"),
                };
                
                dict.insert(
                    Cow::Borrowed(b"url-list"),
                    Value::Bytes(Cow::Owned(web_seed_url.as_bytes().to_vec())),
                );
                
                let modified_bencode = Value::Dict(dict).to_bencode().expect("Failed to re-bencode");
                let modified_bencode_clone = modified_bencode.clone();
                std::fs::write(&torrent_path, &modified_bencode_clone).expect("Failed to write modified torrent");
                
                let bencode_value = Value::from_bencode(&modified_bencode)
                    .expect("Failed to re-parse modified bencode");
                let info_val = match bencode_value {
                    Value::Dict(d) => d.get(b"info" as &[u8]).cloned().expect("No info dict found"),
                    _ => panic!("Invalid torrent format"),
                };
                
                let mut info_buf = Vec::new();
                Write::write_all(
                    &mut info_buf,
                    &info_val.to_bencode().expect("Failed to encode info dict"),
                )
                .expect("Failed to write to buffer");
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
