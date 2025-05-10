use actix_files::{Files, NamedFile};
use actix_web::middleware::Logger;
use actix_web::{
    get,
    web::{self, ServiceConfig, Bytes},
    HttpRequest, HttpResponse, Responder,
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
    sync::{Arc, Mutex},
};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};
use walkdir::WalkDir;
use zip::{ZipWriter, write::FileOptions};

// misc
use async_stream;
use bendy::{decoding::FromBencode, encoding::ToBencode, value::Value};
use hf_hub::api::sync::Api;
use librqbit;
use reqwest;
use serde_json;
use shuttle_runtime::{SecretStore, Secrets};
use tera::{Context, Tera};
use tracing::info;

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
    secrets: &SecretStore,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let access_key = secrets
        .get("AWS_ACCESS_KEY_ID")
        .ok_or("Missing AWS_ACCESS_KEY_ID")?;
    let secret_key = secrets
        .get("AWS_SECRET_ACCESS_KEY")
        .ok_or("Missing AWS_SECRET_ACCESS_KEY")?;
    let account_id = secrets
        .get("CLOUDFLARE_ACCOUNT_ID")
        .ok_or("Missing CLOUDFLARE_ACCOUNT_ID")?;

    // endpoint + bucket name
    let bucket_name = "gerbiltestman";
    let endpoint = format!("https://{}.r2.cloudflarestorage.com", account_id);

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

    let s3_conf = S3ConfigBuilder::from(&shared_config)
        .endpoint_url(endpoint)
        .force_path_style(true)
        .build();
    let client = Client::from_conf(s3_conf);
    // finding the object
    match client
        .head_object()
        .bucket(bucket_name)
        .key(object_key)
        .send()
        .await
    {
        Ok(_) => Ok(true), // 200 
        Err(e) if e.to_string().contains("NotFound") => {
            Ok(false) // 404
        }
        Err(e) => Err(Box::new(e)), // other errors 
    }
}

async fn r2_upload(
    file_path: &str,
    secrets: &SecretStore,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let access_key = secrets
        .get("AWS_ACCESS_KEY_ID")
        .ok_or("Missing AWS_ACCESS_KEY_ID")?;
    let secret_key = secrets
        .get("AWS_SECRET_ACCESS_KEY")
        .ok_or("Missing AWS_SECRET_ACCESS_KEY")?;
    let account_id = secrets
        .get("CLOUDFLARE_ACCOUNT_ID")
        .ok_or("Missing CLOUDFLARE_ACCOUNT_ID")?;
    
    let bucket_name = "gerbiltestman";
    let endpoint_str = format!("https://{}.r2.cloudflarestorage.com", account_id);

    let region_provider = RegionProviderChain::first_try(Region::new("auto"))
        .or_default_provider()
        .or_else(Region::new("us-east-1"));
    let shared_config = aws_config::from_env()
        .region(region_provider)
        .credentials_provider(
            Credentials::new(
                access_key,
                secret_key,
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
    //  let proxy = "gerbil.muggingface.co";
    let file_url = format!("{}/{}/{}", endpoint_str, bucket_name, object_key);
    //  let file_url2 = format!("{}/{}", proxy, object_key);
    // let magnet = magnet_link(file_url2.clone());
    // {
    //     let mut links = state.magnet_links.lock().unwrap();
    //     links.insert(file_url2.clone(), magnet);
    // }

    Ok(file_url)
}

#[get("/{user}/{repo}/{tail:.*}")]
async fn repo_info(
    path: web::Path<(String, String, String)>,
    state: web::Data<Arc<AppState>>,
    secrets: web::Data<SecretStore>,
) -> impl Responder {
    let (user, repo, _tail) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);
    info!("Requesting repo info for {}", full_repo);
    let repo = state.hf_api.model(full_repo.clone());
    match repo.info() {
        Ok(info) => {
            let base_dirs = match BaseDirs::new() {
                Some(b) => b,
                None => return HttpResponse::InternalServerError().body("something messed ups"),
            };
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
                    .redirect(reqwest::redirect::Policy::limited(10))
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
            // getting the home directory
            let user_home = base_dirs.home_dir();
            let target_dir: PathBuf = PathBuf::from(user_home)
                .join("data")
                .join(format!("{}-{}", full_repo, info.sha));
            match r2_object_exists(&zip_name, &secrets).await {
                Ok(true) => {
                    println!("already exists, skipping upload");
                    let magnet: String = format!("https://gerbil.muggingface.co/{}.torrent", info.sha);

                    return HttpResponse::Ok().content_type("text/html").body(
                        render_finished_html_response(&full_repo, &info.sha, &file_names, &magnet, &state.tera),
                    );
                }
                Ok(false) => {
                    println!("continuing with upload...");
                }
                Err(e) => {
                    return HttpResponse::InternalServerError().body(e.to_string());
                }
            }

            std::fs::create_dir_all(&target_dir).expect("Failed to create directory");

            let full_repo_for_response = full_repo.clone();
            let sha_for_response = info.sha.clone();
            let target_dir_clone = target_dir.clone();
            let info_clone = info.clone();
            let full_repo_clone = full_repo.clone();
            let state_clone = state.clone();
            let secrets_clone = secrets.clone();
            
            tokio::spawn(async move {
                for file in &info_clone.siblings {
                    // checking / setting directories
                    let file_path = target_dir_clone.join(&file.rfilename);

                    if let Some(parent) = file_path.parent() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .expect("Failed to create subdirectories");
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
                                info!("Mutex was poisoned, skipping progress update");
                                return;
                            }
                        };
                        if let Some(downloaded) = progress.get_mut(&full_repo_clone) {
                            *downloaded += bytes.len() as u64;
                            info!("Updated progress for {}: {} bytes", full_repo_clone, *downloaded);
                        }
                    }
                }

                // creating directories for torrents
                let torrents_dir = if let Some(parent) = target_dir_clone.parent() {
                    parent.join("Torrents")
                } else {
                    return;
                };

                std::fs::create_dir_all(&torrents_dir)
                    .expect("Failed to create Torrents directory");

                let torrent_path = torrents_dir.join(format!("{}.torrent", info_clone.sha));

                if let Some(torrent_parent) = torrent_path.parent() {
                    std::fs::create_dir_all(torrent_parent)
                        .expect("Failed to create torrent directory");
                } else {
                    return;
                }

                let zip_dir = if let Some(parent) = target_dir_clone.parent() {
                    parent.join("Zips")
                } else {
                    return;
                };

                std::fs::create_dir_all(&zip_dir)
                    .expect("Failed to create Zips directory");

                let zip_path = zip_dir.join(&zip_name);
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
                    &secrets_clone
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
                    &secrets_clone
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
                
         //       let mut hasher = Sha1::new();
         //       hasher.update(&info_buf);
         //       let info_hash = hasher.finalize();
         //       let info_hash_hex = hex::encode(&info_hash);

                // creating magnet link

       //         let name_encoded = utf8_percent_encode(
       //             &format!("{}-{}", full_repo_clone, info_clone.sha),
       //             NON_ALPHANUMERIC,
       //         )
       //         .to_string();
       //         let tracker_encoded = utf8_percent_encode(
       //         "udp://tracker.openbittorrent.com:80/announce",
       //         NON_ALPHANUMERIC
       //         ).to_string();
       //         let ws_encoded = utf8_percent_encode(&web_seed_url, NON_ALPHANUMERIC).to_string();
       //         let magnet_link = format!(
       //             "magnet:?xt=urn:btih:{info_hash}&dn={dn}&tr={tr}&ws={ws}",
       //             info_hash = info_hash_hex,
       //             dn = name_encoded,
       //             tr = tracker_encoded,
       //             ws = ws_encoded,
       //         );


                // inserting the magnet link into memory so we can use it later
                // {
                //     let mut map = state_clone.magnet_links.lock().unwrap();
                //     map.insert(full_repo_clone.clone(), magnet_link.clone());
                // }
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
) -> impl Responder {
    let (user, repo) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);
    
    let downloaded = {
        let progress = state.download_progress.lock().unwrap();
        progress.get(&full_repo).copied().unwrap_or(0)
    };
    
    let total = {
        let sizes = state.total_sizes.lock().unwrap();
        sizes.get(&full_repo).copied().unwrap_or(1)
    };

    info!("Progress for {}: {}/{} bytes", full_repo, downloaded, total);
    
    HttpResponse::Ok().json(serde_json::json!({ "downloaded": downloaded, "total": total }))
}

#[get("/{user}/{repo}/progress_sse")]
async fn progress_sse(
    path: web::Path<(String, String)>,
    state: web::Data<Arc<AppState>>,
    _req: HttpRequest,
) -> HttpResponse {
    let (user, repo) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);
    info!("SSE connection established for {}", full_repo);
    
    let mut last_progress = 0;
    let last_magnet_link = None;
    let mut last_status = String::new();
    
    let stream = async_stream::stream! {
        // Send initial status
        let status = "Starting download...";
        let event = format!("event: status\ndata: {}\n\n", status);
        yield Ok::<Bytes, std::io::Error>(Bytes::from(event));

        loop {
            let (downloaded, total) = {
                let progress = state.download_progress.lock().unwrap();
                let sizes = state.total_sizes.lock().unwrap();
                (
                    progress.get(&full_repo).copied().unwrap_or(0),
                    sizes.get(&full_repo).copied().unwrap_or(1)
                )
            };

            let percent = if total > 0 {
                ((downloaded as f64 / total as f64) * 100.0) as i32
            } else {
                0
            };
            
            if percent != last_progress {
                info!("Sending progress update for {}: {}% ({} bytes)", full_repo, percent, downloaded);
                let event = format!("event: progress\ndata: {}\n\n", percent);
                yield Ok::<Bytes, std::io::Error>(Bytes::from(event));
                last_progress = percent;

                // Update status based on progress
                let status = if percent == 100 {
                    "Creating torrent...".to_string()
                } else {
                    format!("Downloading: {}%", percent)
                };
                if status != last_status {
                    let event = format!("event: status\ndata: {}\n\n", status);
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(event));
                    last_status = status;
                }
            }

            let magnet_link = {
                let magnet_links = state.magnet_links.lock().unwrap();
                magnet_links.get(&full_repo).cloned()
            };

            if let Some(link) = magnet_link {
                if last_magnet_link.as_ref() != Some(&link) {
                    info!("Sending completion event for {} with magnet link", full_repo);
                    // Get file names from the repo info
                    let file_names = {
                        let repo = state.hf_api.model(full_repo.clone());
                        match repo.info() {
                            Ok(info) => info.siblings.iter().map(|f| f.rfilename.clone()).collect::<Vec<_>>(),
                            Err(_) => Vec::new(),
                        }
                    };

                    let completion_data = serde_json::json!({
                        "magnet_link": link,
                        "file_names": file_names
                    });

                    // Send final status
                    let status = "Complete!";
                    let event = format!("event: status\ndata: {}\n\n", status);
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(event));

                    // Send completion event
                    let event = format!("event: complete\ndata: {}\n\n", completion_data.to_string());
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(event));
                    break;
                }
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };

    HttpResponse::Ok()
        .content_type("text/event-stream")
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header(("Connection", "keep-alive"))
        .insert_header(("Access-Control-Allow-Origin", "*"))
        .streaming(stream)
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
async fn main(#[Secrets] secrets: shuttle_runtime::SecretStore) -> ShuttleActixWeb<impl FnOnce(&mut ServiceConfig) + Send + Clone + 'static> {
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
                .service(progress_sse)
                .service(repo_info)
                .app_data(web::Data::new(app_state.clone()))
                .app_data(web::Data::new(secrets))
                .wrap(Logger::default()),
        );
    };

    Ok(config.into())
}
