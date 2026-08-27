#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = woodland::operator::run_cli().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
