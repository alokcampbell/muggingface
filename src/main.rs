use actix_files::Files;
use actix_web::middleware::Logger;
use actix_web::HttpResponse;
use actix_web::{
    get,
    web::{self, Path, ServiceConfig},
    Responder,
};
use hf_hub::api::sync::Api;
use shuttle_actix_web::ShuttleActixWeb;
use std::sync::Arc;
use tracing::info;

#[get("/hello")]
async fn hello_world() -> &'static str {
    "Hello World!"
}

#[get("/{user}/{repo}")]
async fn repo_info(
    path: Path<(String, String)>,
    state: web::Data<Arc<AppState>>,
) -> impl Responder {
    info!("Requesting repo info");
    let (user, repo) = path.into_inner();
    let full_repo = format!("{}/{}", user, repo);
    info!("Requesting repo info for {}", full_repo);

    let repo = state.hf_api.model(full_repo.clone());

    match repo.info() {
        Ok(info) => {
            // let files = info
            //     .files
            //     .iter()
            //     .map(|f| f.path.clone())
            //     .collect::<Vec<_>>();
            let html = format!(
                r#"
                <!DOCTYPE html>
                <html lang="en">
                <head>
                    <meta charset="UTF-8">
                    <meta name="viewport" content="width=device-width, initial-scale=1.0">
                    <title>{} Repository</title>
                </head>
                <body>
                    <h1>{} Repository</h1>
                    <h2>SHA: {}</h2>
                    <h2>Files:</h2>
                    <ul>
                        {}
                    </ul>
                </body>
                </html>
                "#,
                full_repo,
                full_repo,
                info.sha,
                info.siblings
                    .iter()
                    .map(|f| format!("<li>{}</li>", f.rfilename))
                    .collect::<Vec<_>>()
                    .join("\n") // files
                                //     .iter()
                                //     .map(|f| format!("<li>{}</li>", f))
                                //     .collect::<Vec<_>>()
                                //     .join("\n")
            );
            HttpResponse::Ok().content_type("text/html").body(html)
        }
        Err(_) => HttpResponse::NotFound().body(format!("Repository {} not found", full_repo)),
        // Err(_) => HttpResponse::NotFound().body(format!("Repository {} not found", full_repo)),
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
                .service(repo_info)
                .service(hello_world)
                .service(Files::new("/", "static/").index_file("index.html"))
                .app_data(web::Data::new(app_state.clone()))
                .wrap(Logger::default()),
        );
    };

    Ok(config.into())
}
