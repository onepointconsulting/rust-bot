//! Legacy standalone Gmail sign-in helper. Prefer `rust-bot gmail-auth`, which
//! reads the paths from the config file.
//!
//! Usage: `gmail-auth [client_secret.json]` (default
//! `./credentials/client_secret.json`). The token cache is written next to the
//! client secret as `token_cache.json`.

use rust_bot::agent::tools::gmail_oauth::{
    authorise_gmail, token_cache_path_for, verify_inbox_access,
};
use rust_bot::utils::exit_codes::{self, GENERAL_ERROR};
use std::env;
use std::path::Path;

/// Client secret used when no path is given on the command line.
const DEFAULT_CLIENT_SECRET_PATH: &str = "./credentials/client_secret.json";

/// Print `message` to stderr and exit with [`GENERAL_ERROR`].
fn fail(message: &str) -> ! {
    eprintln!("ERROR: {message}");
    exit_codes::exit(GENERAL_ERROR);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let secret_path = Path::new(
        args.get(1)
            .map(String::as_str)
            .unwrap_or(DEFAULT_CLIENT_SECRET_PATH),
    );
    if !secret_path.is_file() {
        fail(&format!(
            "{} not found. Download an OAuth Desktop client from Google Cloud Console.",
            secret_path.display()
        ));
    }
    println!("Using credentials in {}", secret_path.display());

    let token_cache_path = token_cache_path_for(secret_path);
    println!(
        "Token cache will be written to {}",
        token_cache_path.display()
    );
    let access_token = authorise_gmail(secret_path, &token_cache_path)
        .await
        .unwrap_or_else(|e| fail(&e));
    println!("Successfully authenticated!");

    match verify_inbox_access(&access_token).await {
        Ok(count) => println!("Inbox check passed ({count} message(s) listed)."),
        Err(e) => fail(&e),
    }
    println!("Tip: `rust-bot gmail-auth` writes the token straight to tools.gmail.tokenCachePath.");
}
