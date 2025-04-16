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
use librqbit::*;
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
#[get("/")]
async fn index() -> impl Responder {
    NamedFile::open(PathBuf::from("static/index.html"))
}



#[get("/{user}/{repo}")]
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
            // getting the home directory
            let user_home = base_dirs.home_dir();
            let target_dir: PathBuf = PathBuf::from(user_home).join("data").join(format!("{}-{}", full_repo, info.sha));
            if target_dir.exists() {
                return HttpResponse::NotFound().body(format!("Repository {} already cloned", full_repo));
            }

            std::fs::create_dir_all(&target_dir).expect("Failed to create directory");

            for file in &info.siblings {
                // checking / setting directories (i hate this)
                let file_path = target_dir.join(&file.rfilename);

                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await.expect("Failed to create subdirectories");
                }

                // actual download time (get the website, download the file, create the file, write the file.)
                let url = format!("https://huggingface.co/{}/resolve/main/{}?download=true", full_repo, file.rfilename);
                let response = reqwest::get(&url).await.expect("Failed to access file");
                
                if let Some(content_length) = response.content_length() {
                    if content_length > 1_073_741_824 {

                        info!("File {} is too big ({} bytes)", file.rfilename, content_length);
                        continue;
                    }
                }

                let bytes = response.bytes().await.expect("Failed to download file");
                let mut file = File::create(&file_path).await.expect("Failed to create file");
                file.write_all(&bytes).await.expect("Failed to write file");
            }

            // creating directories for torrents
            let torrents_dir = if let Some(parent) = target_dir.parent() {
                parent.join("Torrents")
            } else {
                return HttpResponse::InternalServerError().body("something messed up");
            };

            std::fs::create_dir_all(&torrents_dir).expect("Failed to create Torrents directory");
            
            let torrent_path = torrents_dir.join(format!("{}.torrent", info.sha));

            if let Some(torrent_parent) = torrent_path.parent() {
                std::fs::create_dir_all(torrent_parent).expect("Failed to create torrent directory");
            } else {
                return HttpResponse::InternalServerError().body("something messed up");
            }

            // creating the torrent
            let fullstring = format!("{}-{}", full_repo, info.sha);
            let options = librqbit::CreateTorrentOptions {
                name: Some(&fullstring),
                piece_length: Some(2_097_152),
            };

            let torrent_file = librqbit::create_torrent(&target_dir, options).await.expect("Failed to create torrent");
            
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
            
            let name_encoded = utf8_percent_encode(&format!("{}-{}", full_repo, info.sha), NON_ALPHANUMERIC).to_string();
            let magnet_link = format!(
                "magnet:?xt=urn:btih:{}&dn={}&tr={}",
                info_hash_hex,
                name_encoded,
                utf8_percent_encode("udp://tracker.openbittorrent.com:80/announce", NON_ALPHANUMERIC)
            );
            
            info!("Magnet link: {}", magnet_link);

            // inserting the magnet link into memory so we can use it later
            {
                let mut map = state.magnet_links.lock().unwrap();
                map.insert(full_repo.clone(), magnet_link.clone());
            }
            
            
            // redirecting to the finished page
            return HttpResponse::Found().append_header(("Location", format!("/{}", full_repo) + "/finished")).finish();
        }
        Err(_) => HttpResponse::NotFound().body(format!("Repository {} not found", full_repo)),
    }
}



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
        Err(_) => HttpResponse::NotFound().body("uh oh"),
    }
}


// #[derive(Clone)]
struct AppState {
    hf_api: Api,
    magnet_links: Mutex<HashMap<String, String>>,
}

#[shuttle_runtime::main]
async fn main() -> ShuttleActixWeb<impl FnOnce(&mut ServiceConfig) + Send + Clone + 'static> {
    let app_state = Arc::new(AppState {
        hf_api: Api::new().unwrap(),
        magnet_links: Mutex::new(HashMap::new()),
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
                .app_data(web::Data::new(app_state.clone()))
                .wrap(Logger::default()),
        );
    };

    Ok(config.into())
}