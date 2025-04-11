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
use std::fs;
use std::io::Write;
use std::fs::File;
use reqwest;
use tracing::info;

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
            for file in &info.siblings {
                let url = format!("https://huggingface.co/{}/resolve/main/{}?download=true", full_repo, file.rfilename);
                let response = reqwest::get(&url).await.expect("Failed to access file");
                let bytes = response.bytes().await.expect("Failed to download file");
                let mut file = File::create(&file.rfilename).expect("Failed to create file");
                file.write_all(&bytes).expect("Failed to write file");
            };
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
