#![forbid(unsafe_code)]

#[tokio::main]
async fn main() {
    if let Err(error) = woodland::emulator_gate::run_cli().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
