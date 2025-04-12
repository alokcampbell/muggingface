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
            let user_home = env::var("USERPROFILE").unwrap();
            let target_dir: PathBuf = PathBuf::from(user_home.clone()).join("data").join(format!("{}-{}", full_repo, info.sha));

            if target_dir.exists() {
                return HttpResponse::NotFound().body(format!("Repository {} already cloned", full_repo));
            }

            std::fs::create_dir_all(&target_dir).expect("Failed to create directory");
            
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
                        <h3>muggingface.com</h1>
                        <img src="/static/muggingface_large.png" alt="muggingface.com" style="max-width: 10%; height: auto;">
                    </div>
                </body>
                </html>
                "#,
                info.sha,
                info.siblings
                    .iter()
                    .map(|f| format!("<li>{}</li>", f.rfilename))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            let fullstring = format!("{}-{}", full_repo, info.sha);
            for file in &info.siblings {
                // checking / setting directories (i hate this)
                let file_path = target_dir.join(&file.rfilename);
                info!("Creating file at: {:?}", file_path);

                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await.expect("Failed to create subdirectories");
                }

                // actual download time (get the website, download the file, create the file, write the file.)
                let url = format!("https://huggingface.co/{}/resolve/main/{}?download=true", full_repo, file.rfilename);
                let response = reqwest::get(&url).await.expect("Failed to access file");
                
                if let Some(content_length) = response.content_length() {
                    if content_length > 1_073_741_824 { // 1GB in bytes
                        info!("File {} is too big ({} bytes)", file.rfilename, content_length);
                        continue;
                    }
                }

                let bytes = response.bytes().await.expect("Failed to download file");
                let mut file = File::create(&file_path).await.expect("Failed to create file");
                file.write_all(&bytes).await.expect("Failed to write file");
            };

            // make torrent (i hate this)
            let options = librqbit::CreateTorrentOptions {
                name: Some(&fullstring),
                piece_length: Some(2_097_152),
            };
            let torrent_file = librqbit::create_torrent(&target_dir, options)
                .await
                .expect("Failed to create torrent");
            // make sure directory exists
            let torrents_dir = target_dir.parent().unwrap().join("Torrents");
            std::fs::create_dir_all(&torrents_dir).expect("Failed to create Torrents directory");
            
            let torrent_path = torrents_dir.join(format!("{}.torrent", info.sha));
            std::fs::create_dir_all(torrent_path.parent().unwrap()).expect("Failed to create torrent directory");
            // ok now write the file
            std::fs::write(torrent_path, torrent_file.as_bytes().expect("Failed to get torrent bytes")).expect("Failed to write torrent file");
            HttpResponse::Ok().content_type("text/html").body(html)
        }
        Err(_) => HttpResponse::NotFound().body(format!("Repository {} not found", full_repo)),
    }
}

// #[derive(Clone)]
struct AppState {
    hf_api: Api,
}

#[shuttle_runtime::main]
async fn main() -> ShuttleActixWeb<impl FnOnce(&mut ServiceConfig) + Send + Clone + 'static> {
    let app_state = Arc::new(AppState {
        hf_api: Api::new().unwrap(),
    });
    let config = move |cfg: &mut ServiceConfig| {
        cfg.service(
            web::scope("")
                .service(index)
                .service(Files::new("/static", "static/").index_file("index.html"))
                .service(repo_info)
                .app_data(web::Data::new(app_state.clone()))
                .wrap(Logger::default()),
        );
    };

    Ok(config.into())
}
