use actix_files::{Files, NamedFile};
use actix_web::middleware::Logger;
use actix_web::HttpResponse;
use actix_web::{
    get,
    web::{self, Path, ServiceConfig},
    Responder,
};
use hf_hub::api::sync::Api;
use shuttle_actix_web::ShuttleActixWeb;
use std::path::PathBuf;
use std::sync::Arc;
use std::env;
use reqwest;
use tracing::info;
use librqbit;
use tokio;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use sha1::{Sha1, Digest};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use bendy::value::Value;
use hex;
use bendy::decoding::FromBencode;
use bendy::encoding::ToBencode;
use std::io::Write;
use directories::BaseDirs;
use std::collections::HashMap;
use std::sync::Mutex;
use serde_json;
use sha2::Sha256;

#[get("/")]
async fn index() -> impl Responder {
    NamedFile::open(PathBuf::from("static/index.html"))
}

#[get("/{user}/{repo}/{tail:.*}")]
async fn repo_info(
    path: Path<(String, String)>,
    state: web::Data<Arc<AppState>>,
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
                    full_repo,
                    file.rfilename
                );
                // building client for getting the size of the file
                let client = reqwest::Client::builder().user_agent("muggingface/1.0").redirect(reqwest::redirect::Policy::limited(10)).build().unwrap();
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
            
            // getting the home directory
            let user_home = base_dirs.home_dir();
            let target_dir: PathBuf = PathBuf::from(user_home).join("data").join(format!("{}-{}", full_repo, info.sha));
            if target_dir.exists() {
                return HttpResponse::NotFound().body(format!("Repository {} already cloned", full_repo));
            }

            std::fs::create_dir_all(&target_dir).expect("Failed to create directory");

            let html2 = format!(
                r#"
                <!DOCTYPE html>
                <html lang="en">
                    <head>
                        <meta charset="UTF-8">
                        <meta name="viewport" content="width=device-width, initial-scale=1.0">
                        <title>{full_repo}</title>
                        <link rel="icon" href="favicon.ico" type="image/x-icon">
                        <style>
                            /* progress bar container */
                            #p-container {{
                                width: 100%;
                                background-color: #000;
                                border-radius: 10px;
                                overflow: hidden;
                                margin: 10px 0;
                            }}
            
                            /* progress bar  */
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
                        <h2>SHA: {}</h2>
            
                        <!-- progress bar display -->
                        <div id="p-container">
                            <div id="p-bar">0%</div>
                        </div>
            
                        <script>
                            const bar = document.getElementById("p-bar");
            
                            async function updateProgress() {{
                                try {{
                                    // fetch progress data from the server
                                    const res = await fetch("/{}/progress_json");
                                    if (!res.ok) return;
            
                                    const data = await res.json();
            
                                    // update progress bar 
                                    const percent = (data.downloaded / data.total) * 100;
                                    bar.style.width = percent + "%";
                                    bar.textContent = percent.toFixed(2) + "%";
            
                                    // redirect if finished
                                    if (percent >= 100) {{
                                        window.location.href = "/{}/finished";
                                    }}
                                }} catch (err) {{
                                    console.error("Progress fetch error:", err);
                                }}
                            }}
            
                            // poll progress every second
                            setInterval(() => {{
                                console.log("Tick!");
                                updateProgress();
                            }}, 1000);
                        </script>
            
                        <div>
                            <h3>muggingface.com</h3>
                            <img src="/static/muggingface_large.png" alt="muggingface.com" style="max-width: 10%; height: auto;">
                        </div>
                    </body>
                </html>
                "#,
                info.sha,
                full_repo,
                full_repo,
            );
            


            let target_dir_clone = target_dir.clone();
            let info_clone = info.clone();
            let full_repo_clone = full_repo.clone();
            // cloning the state so we can use it in the thread, otherwise it will be a different state
            let state_clone = state.clone();
            tokio::spawn(async move {
                for file in &info_clone.siblings {
                    // checking / setting directories
                    let file_path = target_dir_clone.join(&file.rfilename);

                    if let Some(parent) = file_path.parent() {
                        tokio::fs::create_dir_all(parent).await.expect("Failed to create subdirectories");
                    }

                    // actual download time (get the website, download the file, create the file, write the file.)
                    let url = format!("https://huggingface.co/{}/resolve/main/{}?download=true", full_repo_clone, file.rfilename);
                    let response = reqwest::get(&url).await.expect("Failed to access file");
                    
                    //if let Some(content_length) = response.content_length() { // only needing for testing otherwise wont download full model
                    //    if content_length > 1_073_741_824 {
                    //        info!("File {} is too big ({} bytes)", file.rfilename, content_length);
                   //           continue;
                   //      }
                  //   }

                    let bytes = response.bytes().await.expect("Failed to download file");
                    let mut file = File::create(&file_path).await.expect("Failed to create file");
                    file.write_all(&bytes).await.expect("Failed to write file");
                

                    // updating the progress memory using the cloned state

                    {
                        let mut progress = state_clone.download_progress.lock().unwrap();
                        if let Some(downloaded) = progress.get_mut(&full_repo_clone) {
                            *downloaded += bytes.len() as u64;
                        }
                    }
                }

                // creating directories for torrents
                let torrents_dir = if let Some(parent) = target_dir_clone.parent() {
                    parent.join("Torrents")
                } else {
                    return;
                };

                std::fs::create_dir_all(&torrents_dir).expect("Failed to create Torrents directory");
                
                let torrent_path = torrents_dir.join(format!("{}.torrent", info_clone.sha));

                if let Some(torrent_parent) = torrent_path.parent() {
                    std::fs::create_dir_all(torrent_parent).expect("Failed to create torrent directory");
                } else {
                    return;
                }

                // creating the torrent
                let fullstring = format!("{}-{}", full_repo_clone, info_clone.sha);
                let options = librqbit::CreateTorrentOptions {
                    name: Some(&fullstring),
                    piece_length: Some(2_097_152),
                };

                let torrent_file = librqbit::create_torrent(&target_dir_clone, options).await.expect("Failed to create torrent");
                
                let torrent_bytes = torrent_file.as_bytes().expect("Failed to get torrent bytes");
                let bytes_clone = torrent_bytes.clone();
                
                std::fs::write(&torrent_path, torrent_bytes).expect("Failed to write torrent file");
                
                let bencode_val = Value::from_bencode(&bytes_clone).expect("Failed to parse bencode");
                let dict = match bencode_val {
                    Value::Dict(d) => d,
                    _ => panic!("Invalid torrent format"),
                };
                
                let info_val = dict.iter().find(|(k, _)| k.as_ref() == b"info").map(|(_, v)| v.clone()).expect("No info dict found");
                // formatting the hash
                let mut info_buf = Vec::new();
                Write::write_all(&mut info_buf, &info_val.to_bencode().expect("Failed to encode info dict")).expect("Failed to write to buffer");
                
                let mut hasher = Sha1::new();
                hasher.update(&info_buf);
                let info_hash = hasher.finalize();
                let info_hash_hex = hex::encode(&info_hash);

                // creating magnet link
                
                let name_encoded = utf8_percent_encode(&format!("{}-{}", full_repo_clone, info_clone.sha), NON_ALPHANUMERIC).to_string();
                let magnet_link = format!(
                    "magnet:?xt=urn:btih:{}&dn={}&tr={}",
                    info_hash_hex,
                    name_encoded,
                    utf8_percent_encode("udp://tracker.openbittorrent.com:80/announce", NON_ALPHANUMERIC)
                );
                
                // inserting the magnet link into memory so we can use it later
                {
                    let mut map = state.magnet_links.lock().unwrap();
                    map.insert(full_repo_clone.clone(), magnet_link.clone());
                }
            });
            return HttpResponse::Ok().content_type("text/html").body(html2);
            //HttpResponse::Found().append_header(("Location", format!("/{}", full_repo) + "/finished")).finish()
        }
        Err(_) => HttpResponse::NotFound().body(format!("Repository {} not found", full_repo)),
    }
}

// this is where we do the progress work, and the unwrapping the values
#[get("/{user}/{repo}/progress_json")]
async fn progress_json(
    path: Path<(String, String)>,
    state: web::Data<Arc<AppState>>,
) -> impl Responder {
    let (user, repo) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);
    // get the downloaded size
    let downloaded = state.download_progress.lock().unwrap().get(&full_repo).copied().unwrap_or(0);
    // get the total size
    let total = state.total_sizes.lock().unwrap().get(&full_repo).copied().unwrap_or(1);
    // return the json
    HttpResponse::Ok().json(serde_json::json!({ "downloaded": downloaded, "total": total }))
}

// this is where the user is redirected to when the download is finished, and the magnet link is displayed
#[get("/{user}/{repo}/finished")]
async fn repo_finished(
    path: Path<(String, String)>,
    state: web::Data<Arc<AppState>>,
) -> impl Responder {
    let (user, repo) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);
    let repo = state.hf_api.model(full_repo.clone());

    match repo.info() {
        Ok(info) => {

            // take link out, and destroy it after so it doesnt linger in memory
            let magnet_link = {
                let mut map = state.magnet_links.lock().unwrap();
                map.remove(&full_repo).unwrap_or_else(|| "Magnet link not found.".to_string())
            };
            

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
                    <h2>SHA: {}</h2>
                    <h2>Files:</h2>
                    <ul>
                        {}
                    </ul>
                    <div>
                        <h3>muggingface.com</h3>
                        <h3><a href="{}">MAGNET LINK</a></h3>
                        <img src="/static/muggingface_large.png" alt="muggingface.com" style="max-width: 10%; height: auto;">
                    </div>
                </body>
                </html>
                "#,
                info.sha,
                info.siblings.iter().map(|f| format!("<li>{}</li>", f.rfilename)).collect::<Vec<_>>().join("\n"),
                magnet_link
            );
            return HttpResponse::Ok().content_type("text/html").body(html);
        }
        Err(_) => HttpResponse::NotFound().body(format!("Repository {} not found", full_repo)),
    }
}


// #[derive(Clone)]
struct AppState {
    hf_api: Api,
    magnet_links: Mutex<HashMap<String, String>>,
    download_progress: Mutex<HashMap<String, u64>>,
    total_sizes: Mutex<HashMap<String, u64>>,
}

#[shuttle_runtime::main]
async fn main() -> ShuttleActixWeb<impl FnOnce(&mut ServiceConfig) + Send + Clone + 'static> {
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
                // finished page
                .service(repo_finished)
                .service(progress_json)
                .app_data(web::Data::new(app_state.clone()))
                .wrap(Logger::default()),
        );
    };

    Ok(config.into())
}