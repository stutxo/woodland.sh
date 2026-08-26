#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = woodland::regtest_bootstrap::run_cli().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
