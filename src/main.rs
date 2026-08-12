#[actix_web::main]
async fn main() -> std::io::Result<()> {
    muggingface::web::run().await
}
