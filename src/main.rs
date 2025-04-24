use actix_files::{Files, NamedFile};
use actix_web::middleware::Logger;
use actix_web::HttpResponse;
use actix_web::{
    get,
    web::{self, Path, ServiceConfig},
    Responder,
};
use bendy::decoding::FromBencode;
use bendy::encoding::ToBencode;
use bendy::value::Value;
use directories::BaseDirs;
use hex;
use hf_hub::api::sync::Api;
use librqbit;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest;
use serde_json;
use sha1::{Digest, Sha1};
use shuttle_actix_web::ShuttleActixWeb;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tokio;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tracing::info;
use s3::bucket::Bucket;
use s3::creds::Credentials;
use s3::region::Region;
use zip::{ZipWriter, write::FileOptions};
use std::fs::File as StdFile;
use std::io::Write;
use walkdir::WalkDir;
use shuttle_runtime::Secrets;
use shuttle_runtime::SecretStore;

fn render_loading_html_response(full_repo: &str, sha: &str) -> String {
    let html = format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="UTF-8">
                <meta name="viewport" content="width=device-width, initial-scale=1.0">
                <title>{full_repo}</title>
                <link rel="icon" href="favicon.ico" type="image/x-icon">
                <style>
                    #p-container {{
                        width: 100%;
                        background-color: #000;
                        border-radius: 10px;
                        overflow: hidden;
                        margin: 10px 0;
                    }}
                    #p-bar {{
                        width: 0%;
                        height: 20px;
                        background-color: red;
                        text-align: center;
                        color: white;
                        line-height: 20px;
                        transition: width 0.4s ease;
                    }}
                </style>
            </head>
            <body>
                <h1>{full_repo}</h1>
                <h2>SHA: {sha}</h2>

                <div id="p-container">
                    <div id="p-bar">0%</div>
                </div>
    <script>
        function getRepoFromUrl() {{
            const pathParts = window.location.pathname.split('/');
            const user = pathParts[1];
            const repo = pathParts[2];
            return `${{user}}/${{repo}}`;
        }}

        const fullRepo = getRepoFromUrl();
        const bar = document.getElementById("p-bar");
        let lastProgress = 0;

        async function updateProgress() {{
            try {{
                    const res = await fetch(`/${{fullRepo}}/progress_json`);
                if (!res.ok) {{
                    return;
                }}

                const data = await res.json();
                const percent = Math.round((data.downloaded / data.total) * 100);

                if (percent !== lastProgress) {{
                    bar.style.width = percent + "%";
                    bar.textContent = percent + "%";
                    lastProgress = percent;
                }}

                if (percent >= 100) {{
                    location.reload();
                }}
            }} catch (err) {{
            }}
        }}

        updateProgress();
        setInterval(updateProgress, 1000);
    </script>
                <div>
                    <h3>muggingface.com</h3>
                    <img src="/static/muggingface_large.png" alt="muggingface.com" style="max-width: 10%; height: auto;">
                </div>
            </body>
        </html>
        "#
    );

    return html;
}

fn render_finished_html_response(
    full_repo: &str,
    sha: &str,
    file_names: &[String],
    magnet_link: &str,
) -> String {
    let files_list = file_names
        .iter()
        .map(|f| format!("<li>{}</li>", f))
        .collect::<Vec<_>>()
        .join("\n");

    let html = format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>{full_repo}</title>
            <link rel="icon" href="favicon.ico" type="image/x-icon">
        </head>
        <body>
            <h1>{full_repo}</h1>
            <h2>SHA: {sha}</h2>
            <h2>Files:</h2>
            <ul>
                {files_list}
            </ul>
            <div>
                <h3>muggingface.com</h3>
                <h3><a href="{magnet_link}">MAGNET LINK</a></h3>
                <img src="/static/muggingface_large.png" alt="muggingface.com" style="max-width: 10%; height: auto;">
            </div>
        </body>
        </html>
        "#
    );

    return html;
}

#[get("/")]
async fn index() -> impl Responder {
    NamedFile::open(PathBuf::from("static/index.html"))
}

async fn wasabi_upload(
    file_path: &str,
    bucket_name: &str,
    base_dir: &std::path::Path,
    state: &Arc<AppState>,
    secrets: &SecretStore,
) -> Result<String, Box<dyn std::error::Error>> {
    let access_key = secrets.get("AWS_ACCESS_KEY_ID").ok_or_else(|| "Missing AWS_ACCESS_KEY_ID".to_string())?;
    let secret_key = secrets.get("AWS_SECRET_ACCESS_KEY").ok_or_else(|| "Missing AWS_SECRET_ACCESS_KEY".to_string())?;

    let credentials = Credentials::new(Some(access_key.as_str()), Some(secret_key.as_str()), None, None, None)?;
        
    let region = Region::Custom {
        region: "us-central-1".into(),
        endpoint: "https://s3.us-central-1.wasabisys.com".into(),
    };

    let bucket = Bucket::new(bucket_name, region, credentials)?.with_path_style();
    let data = tokio::fs::read(file_path).await?;
    let file_path = std::path::Path::new(file_path);
    let relative_path = file_path.strip_prefix(base_dir)?.to_string_lossy();

    let folder = match file_path.extension().and_then(|ext| ext.to_str()) {
        Some("zip") => "Zips",
        Some("torrent") => "Torrents",
        _ => "Misc",     
    };

    let wasabi_key = format!("{}/{}", folder, relative_path);

    let response = bucket.put_object(&wasabi_key, &data).await?;
    if response.status_code() != 200 {
        info!("Failed to upload to Wasabi: {} for {}", response.status_code(), wasabi_key);
        return Err(format!("Failed to upload {}: {}", wasabi_key, response.status_code()).into());
    }
    

    let file_url = format!(
        "https://{}.s3.us-central-1.wasabisys.com/{}",
        bucket_name, 
        wasabi_key
    );
    
    let magnet_link = magnet_link(file_url.clone());
    let mut magnet_links = state.magnet_links.lock().unwrap();
    magnet_links.insert(file_url.clone(), magnet_link.clone());

    Ok(file_url)
}



fn magnet_link(url: String) -> String {
    let mut hasher = Sha1::new();
    hasher.update(url.as_bytes());
    let info_hash = hasher.finalize();
    let info_hash_hex = hex::encode(info_hash);
    let name_encoded = utf8_percent_encode(&url, NON_ALPHANUMERIC).to_string();
    format!(
        "magnet:?xt=urn:btih:{}&dn={}&tr={}",
        info_hash_hex,
        name_encoded,
        utf8_percent_encode("udp://tracker.openbittorrent.com:80/announce", NON_ALPHANUMERIC)
    )
}

#[get("/{user}/{repo}{tail:.*}")]
async fn repo_info(
    path: web::Path<(String, String)>,
    state: web::Data<Arc<AppState>>,
    secrets: web::Data<SecretStore>,
) -> impl Responder {
    let (user, repo) = path.into_inner();
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
            if target_dir.exists() {
                let torrents_dir = target_dir.parent().unwrap().join("Torrents");
                let torrent_path = torrents_dir.join(format!("{}.torrent", info.sha));
                if torrent_path.exists() {
                    let magnet_link = state.magnet_links.lock().unwrap().get(&full_repo).cloned().unwrap_or_else(|| {
                        let url = format!("https://muggingface.co/{}/{}", full_repo, info.sha);
                        magnet_link(url)
                    });
                    let html2 = render_finished_html_response(&full_repo, &info.sha, &file_names, &magnet_link);
                    return HttpResponse::Ok().content_type("text/html").body(html2);
                }
            }

            std::fs::create_dir_all(&target_dir).expect("Failed to create directory");

            let full_repo_for_response = full_repo.clone();
            let sha_for_response = info.sha.clone();
            let target_dir_clone = target_dir.clone();
            let info_clone = info.clone();
            let full_repo_clone = full_repo.clone();
            // cloning the state so we can use it in the thread, otherwise it will be a different state
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

                //    // if let Some(content_length) = response.content_length() {
                //         // only needing for testing otherwise wont download full model
                //         if content_length > 1_073_741_824 {
                //             info!(
                //                 "File {} is too big ({} bytes)",
                //                 file.rfilename, content_length
                //             );
                //             continue;
                //         }
                //     }

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

                let zip_name = format!("{}-{}.zip", full_repo_clone.replace("/", "-"), info_clone.sha);
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
                match wasabi_upload(&zip_path.to_string_lossy(), "muggingface.co", &zip_dir, &state_clone, &secrets_clone).await {
                    Ok(url) => info!("Successfully uploaded zip to: {}", url),
                    Err(e) => info!("Failed to upload zip to Wasabi: {} for file {}", e, zip_path.to_string_lossy()),
                }

                // creating the torrent
                let bruh2 = zip_name.clone();

                let options = librqbit::CreateTorrentOptions {
                    name: Some(&bruh2),
                    piece_length: Some(2_097_152),
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
                match wasabi_upload(&torrent_path.to_string_lossy(), "muggingface.co", &torrents_dir, &state_clone, &secrets_clone).await {
                    Ok(url) => info!("Successfully uploaded torrent to: {}", url),
                    Err(e) => info!("Failed to upload torrent to Wasabi: {} for file {}", e, torrent_path.to_string_lossy()),
                }

                let bencode_val =
                    Value::from_bencode(&bytes_clone).expect("Failed to parse bencode");
                let dict = match bencode_val {
                    Value::Dict(d) => d,
                    _ => panic!("Invalid torrent format"),
                };

                let info_val = dict
                    .iter()
                    .find(|(k, _)| k.as_ref() == b"info")
                    .map(|(_, v)| v.clone())
                    .expect("No info dict found");
                // formatting the hash
                let mut info_buf = Vec::new();
                Write::write_all(
                    &mut info_buf,
                    &info_val.to_bencode().expect("Failed to encode info dict"),
                )
                .expect("Failed to write to buffer");

                let mut hasher = Sha1::new();
                hasher.update(&info_buf);
                let info_hash = hasher.finalize();
                let info_hash_hex = hex::encode(&info_hash);

                // creating magnet link

                let name_encoded = utf8_percent_encode(
                    &format!("{}-{}", full_repo_clone, info_clone.sha),
                    NON_ALPHANUMERIC,
                )
                .to_string();
                let magnet_link = format!(
                    "magnet:?xt=urn:btih:{}&dn={}&tr={}",
                    info_hash_hex,
                    name_encoded,
                    utf8_percent_encode(
                        "udp://tracker.openbittorrent.com:80/announce",
                        NON_ALPHANUMERIC
                    )
                );

                // inserting the magnet link into memory so we can use it later
                {
                    let mut map = state_clone.magnet_links.lock().unwrap();
                    map.insert(full_repo_clone.clone(), magnet_link.clone());
                }
                let file_names: Vec<String> = info_clone
                    .siblings
                    .iter()
                    .map(|f| f.rfilename.clone())
                    .collect();
                let html2 =
                    render_finished_html_response(&full_repo, &info.sha, &file_names, &magnet_link);
            });
            return HttpResponse::Ok().content_type("text/html").body(
                render_loading_html_response(&full_repo_for_response, &sha_for_response),
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


// #[derive(Clone)]
struct AppState {
    hf_api: Api,
    magnet_links: Mutex<HashMap<String, String>>,
    download_progress: Mutex<HashMap<String, u64>>,
    total_sizes: Mutex<HashMap<String, u64>>,
}

#[shuttle_runtime::main]
async fn main(#[Secrets] secrets: shuttle_runtime::SecretStore) -> ShuttleActixWeb<impl FnOnce(&mut ServiceConfig) + Send + Clone + 'static> {
    let app_state = Arc::new(AppState {
        hf_api: Api::new().unwrap(),
        magnet_links: Mutex::new(HashMap::new()),
        download_progress: Mutex::new(HashMap::new()),
        total_sizes: Mutex::new(HashMap::new()),
    });
    let config = move |cfg: &mut ServiceConfig| {
        cfg.service(
            web::scope("")
                .service(index)
                // main website
                .service(Files::new("/static", "static/").index_file("index.html"))
                // downloading + torrent creation + magnet link
                .service(repo_info)
                .service(progress_json)
                .app_data(web::Data::new(app_state.clone()))
                .app_data(web::Data::new(secrets))
                .wrap(Logger::default()),
        );
    };

    Ok(config.into())
}
