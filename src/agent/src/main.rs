use clap::Parser;
use rust_bot::cli::{Cli, eprint_error, run};
use rust_bot::utils::exit_codes::{self, GENERAL_ERROR};

/// Runs before `main` (and so before tokio spawns worker threads) to cap
/// glibc malloc arenas; see [`rust_bot::utils::malloc_tuning`].
#[ctor::ctor(unsafe)]
fn tune_allocator() {
    rust_bot::utils::malloc_tuning::apply_defaults();
}

#[tokio::main]
async fn main() {
    // Load `.env` when present; missing file is fine.
    let _ = dotenv::dotenv();

    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprint_error(e);
        exit_codes::exit(GENERAL_ERROR);
    }
}
