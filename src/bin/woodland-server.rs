#[tokio::main]
async fn main() {
    if let Err(error) = woodland::server::run_cli().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
