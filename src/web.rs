use crate::db;
use crate::git_lfs::GitLfsCloner;
use crate::hf::HfClient;
use crate::magnet::{display_name, magnet_from_torrent_bytes};
use crate::over_size_limit;
use crate::paths::{ensure_server_directories, seeding_dir, torrent_path};
use crate::search::{merge_suggestions, SearchSuggestion};
use crate::split_repo_id;
use crate::state::SharedState;
use actix_files::Files;
use actix_web::middleware::Logger;
use actix_web::{
    get,
    web::{self, Json},
    App, Error as ActixError, HttpResponse, HttpServer, Responder,
};
use std::sync::Arc;
use tera::Context;
use tracing::{error, info};

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Progress {
    downloaded: u64,
    total: u64,
}

fn render(tera: &tera::Tera, name: &str, context: &Context) -> HttpResponse {
    match tera.render(name, context) {
        Ok(html) => HttpResponse::Ok().content_type("text/html; charset=utf-8").body(html),
        Err(e) => {
            error!("Failed to render {name}: {e}");
            HttpResponse::InternalServerError().body("Server error: could not render page.")
        }
    }
}

fn finished_page(state: &SharedState, full_repo: &str, sha: &str, files: &[String], magnet: &str) -> HttpResponse {
    let mut context = Context::new();
    context.insert("full_repo", full_repo);
    context.insert("sha", sha);
    context.insert("file_names", files);
    context.insert("magnet_link", magnet);
    render(&state.tera, "finished.html", &context)
}

fn loading_page(state: &SharedState, full_repo: &str, sha: &str) -> HttpResponse {
    let mut context = Context::new();
    context.insert("full_repo", full_repo);
    context.insert("sha", sha);
    render(&state.tera, "loading.html", &context)
}

fn donate_page(state: &SharedState, full_repo: &str) -> HttpResponse {
    let mut context = Context::new();
    context.insert("full_repo", full_repo);
    context.insert(
        "max_size_gb",
        &(crate::MAX_REPO_SIZE_BYTES / (1024 * 1024 * 1024)),
    );
    render(&state.tera, "donate.html", &context)
}

fn not_found_page(state: &SharedState, full_repo: &str, suggestions: &[SearchSuggestion]) -> HttpResponse {
    let mut context = Context::new();
    context.insert("full_repo", full_repo);
    context.insert("suggestions", suggestions);
    render(&state.tera, "not_found.html", &context)
}

#[get("/healthz")]
async fn healthz() -> impl Responder {
    HttpResponse::Ok().content_type("text/plain").body("ok")
}

#[get("/")]
async fn index(state: web::Data<SharedState>) -> impl Responder {
    let mut context = Context::new();
    match db::top_recent(&state.db_pool, 10).await {
        Ok(top_torrents) => context.insert("top_torrents", &top_torrents),
        Err(e) => error!("Failed to fetch top torrents: {e}"),
    }
    render(&state.tera, "index.html", &context)
}

#[get("/about")]
async fn about_page(state: web::Data<SharedState>) -> impl Responder {
    render(&state.tera, "about.html", &Context::new())
}

#[get("/search")]
async fn search_torrents(
    query_params: web::Query<SearchQuery>,
    state: web::Data<SharedState>,
) -> impl Responder {
    let q = query_params.q.as_deref().unwrap_or("").trim();
    let local_rows = if q.is_empty() {
        db::top_popular(&state.db_pool, 5).await
    } else {
        db::search_local(&state.db_pool, q, 8).await
    };
    let local = match local_rows {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|row| {
                SearchSuggestion::from_full_repo(format!("{}/{}", row.author, row.repo_name), "local")
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("local search failed: {e}");
            Vec::new()
        }
    };
    let hf_hits = match state.hf.search_models(q, 10).await {
        Ok(hits) => hits,
        Err(e) => {
            error!("HF search failed: {e}");
            Vec::new()
        }
    };
    HttpResponse::Ok().json(merge_suggestions(local, hf_hits, 10))
}

#[get("/{name}")]
async fn unscoped_repo(
    path: web::Path<String>,
    state: web::Data<SharedState>,
) -> impl Responder {
    let name = path.into_inner();
    if crate::is_reserved_path(&name) {
        return HttpResponse::NotFound().body("Not found");
    }
    match state.hf.repo_info(&name).await {
        Ok(Some(info)) => HttpResponse::Found()
            .insert_header(("Location", format!("/{}", info.id)))
            .finish(),
        Ok(None) => {
            let suggestions = hf_suggestions(&state.hf, &name).await;
            not_found_page(&state, &name, &suggestions)
        }
        Err(e) => {
            error!("HF lookup failed for {name}: {e}");
            HttpResponse::BadGateway().body("Failed to reach Hugging Face. Try again in a moment.")
        }
    }
}

#[get("/{user}/{repo}/progress_json")]
async fn progress_json(
    path: web::Path<(String, String)>,
    state: web::Data<SharedState>,
) -> Result<Json<Progress>, ActixError> {
    let (user, repo) = path.into_inner();
    let full_repo = format!("{user}/{repo}");
    let downloaded = state
        .download_progress
        .lock()
        .ok()
        .and_then(|m| m.get(&full_repo).copied())
        .unwrap_or(0);
    let total = state
        .total_sizes
        .lock()
        .ok()
        .and_then(|m| m.get(&full_repo).copied())
        .unwrap_or(1)
        .max(1);
    Ok(web::Json(Progress { downloaded, total }))
}

#[get("/{user}/{repo}{tail:.*}")]
async fn repo_info(
    path: web::Path<(String, String, String)>,
    state: web::Data<SharedState>,
) -> impl Responder {
    let (user, repo, _tail) = path.into_inner();
    let requested = format!("{user}/{repo}");
    info!("Requesting repo info for {requested}");

    let info = match state.hf.repo_info(&requested).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            let suggestions = hf_suggestions(&state.hf, &requested).await;
            return not_found_page(&state, &requested, &suggestions);
        }
        Err(e) => {
            error!("HF lookup failed for {requested}: {e}");
            return HttpResponse::BadGateway()
                .body("Failed to reach Hugging Face. Try again in a moment.");
        }
    };

    if info.id != requested {
        return HttpResponse::Found()
            .insert_header(("Location", format!("/{}", info.id)))
            .finish();
    }

    let (author, repo_name) = match split_repo_id(&info.id) {
        Some(parts) => parts,
        None => {
            return HttpResponse::BadRequest().body("Invalid repository id");
        }
    };

    if let Ok(Some(record)) = db::get_by_sha(&state.db_pool, &info.sha).await {
        db::bump_page_hits(&state.db_pool, &info.sha).await;
        return finished_page(&state, &info.id, &info.sha, &info.siblings, &record.magnet_link);
    }

    if over_size_limit(info.used_storage) {
        info!(
            "Repository {} is {} bytes, over the clone limit",
            info.id,
            info.used_storage.unwrap_or(0)
        );
        return donate_page(&state, &info.id);
    }

    let seeding = match seeding_dir() {
        Ok(dir) => dir,
        Err(e) => {
            error!("Failed to get seeding directory: {e}");
            return HttpResponse::InternalServerError()
                .body("Failed to determine server storage directory.");
        }
    };
    let _ = ensure_server_directories();

    let target_dir = seeding.join(format!("{}-{}", info.id.replace('/', "-"), info.sha));
    let torrent_file_path = torrent_path(&seeding, &info.sha);
    let all_files_exist = !info.siblings.is_empty()
        && info
            .siblings
            .iter()
            .all(|name| target_dir.join(name).exists());

    if all_files_exist || torrent_file_path.exists() {
        if let Some(response) =
            serve_existing_torrent(&state, &info.id, &author, &repo_name, &info.sha, &info.siblings, &target_dir, &torrent_file_path)
                .await
        {
            return response;
        }
    }

    let total_size = info.used_storage.unwrap_or(0).max(1);
    {
        let mut progress = state.download_progress.lock().unwrap();
        progress.insert(info.id.clone(), 0);
    }
    {
        let mut sizes = state.total_sizes.lock().unwrap();
        sizes.insert(info.id.clone(), total_size);
    }

    spawn_clone_job(
        state.get_ref().clone(),
        info.id.clone(),
        author,
        repo_name,
        info.sha.clone(),
        info.siblings.clone(),
        seeding,
    );

    loading_page(&state, &info.id, &info.sha)
}

#[get("/torrent/{sha}/download")]
async fn download_torrent_by_sha(
    path: web::Path<String>,
    state: web::Data<SharedState>,
) -> impl Responder {
    let sha = path.into_inner();
    match db::get_download(&state.db_pool, &sha).await {
        Ok(Some(record)) => {
            let filename = format!("{}.torrent", record.repo_name.replace('/', "-"));
            HttpResponse::Ok()
                .content_type("application/x-bittorrent")
                .insert_header((
                    "Content-Disposition",
                    format!("attachment; filename=\"{filename}\""),
                ))
                .body(record.torrent_file)
        }
        Ok(None) => HttpResponse::NotFound().body(format!("Torrent file for SHA {sha} not found.")),
        Err(e) => {
            error!("Database error fetching torrent file for SHA {sha}: {e}");
            HttpResponse::InternalServerError().body("Error retrieving torrent file.")
        }
    }
}

async fn hf_suggestions(hf: &HfClient, query: &str) -> Vec<SearchSuggestion> {
    let mut hits = match hf.search_models(query, 8).await {
        Ok(hits) => hits,
        Err(e) => {
            error!("HF suggestions failed: {e}");
            Vec::new()
        }
    };
    if let Some((_, repo)) = split_repo_id(query) {
        if repo != query {
            match hf.search_models(&repo, 8).await {
                Ok(more) => hits.extend(more),
                Err(e) => error!("HF suggestions (repo name) failed: {e}"),
            }
        }
    }
    merge_suggestions([], hits, 8)
}

async fn serve_existing_torrent(
    state: &SharedState,
    full_repo: &str,
    author: &str,
    repo_name: &str,
    sha: &str,
    files: &[String],
    target_dir: &std::path::Path,
    torrent_file_path: &std::path::Path,
) -> Option<HttpResponse> {
    let torrent_bytes = if torrent_file_path.exists() {
        std::fs::read(torrent_file_path).ok()?
    } else {
        let torrent_name = target_dir.file_name()?.to_string_lossy().into_owned();
        let options = librqbit::CreateTorrentOptions {
            name: Some(&torrent_name),
            piece_length: Some(1_048_576),
        };
        let torrent_file = librqbit::create_torrent(target_dir, options).await.ok()?;
        let bytes = torrent_file.as_bytes().ok()?.to_vec();
        let _ = std::fs::write(torrent_file_path, &bytes);
        bytes
    };
    let magnet = magnet_from_torrent_bytes(&torrent_bytes, &display_name(full_repo, sha)).ok()?;
    let pool = state.db_pool.clone();
    let sha_s = sha.to_string();
    let author_s = author.to_string();
    let repo_s = repo_name.to_string();
    let magnet_s = magnet.clone();
    let bytes_s = torrent_bytes;
    tokio::spawn(async move {
        db::upsert_torrent(&pool, &sha_s, &author_s, &repo_s, &magnet_s, &bytes_s).await;
    });
    Some(finished_page(state, full_repo, sha, files, &magnet))
}

fn spawn_clone_job(
    state: SharedState,
    full_repo: String,
    author: String,
    repo_name: String,
    sha: String,
    siblings: Vec<String>,
    seeding: std::path::PathBuf,
) {
    tokio::spawn(async move {
        let progress_repo = full_repo.clone();
        let progress_state = state.clone();
        let cloner = GitLfsCloner::new(seeding.clone(), state.hf_token.clone()).with_progress_callback(
            move |bytes_downloaded| {
                let mut map = match progress_state.download_progress.lock() {
                    Ok(map) => map,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let current = map.get(&progress_repo).copied().unwrap_or(0);
                map.insert(progress_repo.clone(), current + bytes_downloaded);
            },
        );

        let target_dir = match cloner.clone_repository(&full_repo, &sha).await {
            Ok(dir) => dir,
            Err(e) => {
                error!("Failed to clone repository {full_repo}: {e}");
                return;
            }
        };

        let torrent_name = target_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| full_repo.replace('/', "-"));
        let options = librqbit::CreateTorrentOptions {
            name: Some(&torrent_name),
            piece_length: Some(1_048_576),
        };
        let torrent_file = match tokio::time::timeout(
            std::time::Duration::from_secs(1800),
            librqbit::create_torrent(&target_dir, options),
        )
        .await
        {
            Ok(Ok(tf)) => tf,
            Ok(Err(e)) => {
                error!("Failed to create torrent for {full_repo}: {e}");
                return;
            }
            Err(_) => {
                error!("Torrent creation timed out for {full_repo}");
                return;
            }
        };
        let torrent_bytes = match torrent_file.as_bytes() {
            Ok(bytes) => bytes.to_vec(),
            Err(e) => {
                error!("Failed to get torrent bytes for {full_repo}: {e}");
                return;
            }
        };
        let magnet = match magnet_from_torrent_bytes(&torrent_bytes, &display_name(&full_repo, &sha)) {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to build magnet for {full_repo}: {e}");
                return;
            }
        };
        let _ = std::fs::write(torrent_path(&seeding, &sha), &torrent_bytes);
        db::upsert_torrent(
            &state.db_pool,
            &sha,
            &author,
            &repo_name,
            &magnet,
            &torrent_bytes,
        )
        .await;

        let actual: u64 = siblings
            .iter()
            .filter_map(|name| std::fs::metadata(target_dir.join(name)).ok().map(|m| m.len()))
            .sum();
        if let Ok(mut map) = state.download_progress.lock() {
            let total = actual.max(
                state
                    .total_sizes
                    .lock()
                    .ok()
                    .and_then(|m| m.get(&full_repo).copied())
                    .unwrap_or(actual),
            );
            map.insert(full_repo.clone(), total);
        }
        info!("Repository {full_repo} processing completed successfully");
    });
}

pub async fn run() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = ensure_server_directories() {
        error!("Failed to initialize server directories: {e}");
    }
    if let Ok(dir) = seeding_dir() {
        match crate::gc::prune_if_needed(&dir) {
            Ok(deleted) if !deleted.is_empty() => {
                info!("startup GC removed {} old model dirs", deleted.len());
            }
            Err(e) => error!("startup GC failed: {e}"),
            _ => {}
        }
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
            loop {
                interval.tick().await;
                if let Err(e) = crate::gc::prune_if_needed(&dir) {
                    error!("periodic GC failed: {e}");
                }
            }
        });
    }

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let db_pool = sqlx::PgPool::connect(&database_url)
        .await
        .unwrap_or_else(|e| panic!("Database connection failed: {e}"));
    info!("Successfully connected to the database");

    match sqlx::migrate!("./migrations").run(&db_pool).await {
        Ok(_) => info!("Database migrations ran successfully"),
        Err(e) => error!("Failed to run database migrations: {e}"),
    }

    let hf_token = std::env::var("HF_TOKEN").ok().filter(|s| !s.is_empty());
    if hf_token.is_some() {
        info!("Using HF token for authentication");
    } else {
        info!("No HF token provided; gated/private repos will not resolve");
    }

    let tera = tera::Tera::new("static/**/*.html").expect("Failed to parse templates");
    let hf = HfClient::new(hf_token.clone()).expect("Failed to build Hugging Face client");
    let state = Arc::new(crate::state::AppState {
        hf,
        download_progress: std::sync::Mutex::new(std::collections::HashMap::new()),
        total_sizes: std::sync::Mutex::new(std::collections::HashMap::new()),
        tera,
        db_pool,
        hf_token,
    });

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    info!("Listening on 0.0.0.0:{port}");

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .wrap(Logger::default())
            .service(healthz)
            .service(index)
            .service(about_page)
            .service(search_torrents)
            .service(Files::new("/static", "static/").index_file("index.html"))
            .service(progress_json)
            .service(download_torrent_by_sha)
            .service(repo_info)
            .service(unscoped_repo)
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, App};

    #[actix_web::test]
    async fn healthz_ok() {
        let app = test::init_service(App::new().service(healthz)).await;
        let req = test::TestRequest::get().uri("/healthz").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let body = test::read_body(resp).await;
        assert_eq!(body, "ok");
    }
}
