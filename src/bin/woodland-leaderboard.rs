#[tokio::main]
async fn main() {
    if let Err(error) = woodland::leaderboard::run_cli().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
