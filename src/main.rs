use clap::Parser;
use rust_bot::cli::{eprint_error, Cli, run};

#[tokio::main]
async fn main() {
    // Load `.env` when present; missing file is fine.
    let _ = dotenv::dotenv();

    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprint_error(e);
        std::process::exit(1);
    }
}
