use crate::hf::HfClient;
use sqlx::PgPool;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tera::Tera;

pub struct AppState {
    pub hf: HfClient,
    pub download_progress: Mutex<HashMap<String, u64>>,
    pub total_sizes: Mutex<HashMap<String, u64>>,
    pub tera: Tera,
    pub db_pool: PgPool,
    pub hf_token: Option<String>,
}

pub type SharedState = Arc<AppState>;
