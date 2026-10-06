use rust_bot::utils::exit_codes::{self, GENERAL_ERROR};
use std::env;
use std::path::{Path, PathBuf};
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

// Gmail API base URL
const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1";

/// File name of the persisted OAuth token cache.
const TOKEN_CACHE_FILE_NAME: &str = "token_cache.json";

/// Path of the token cache for a given client secret file: `token_cache.json`
/// in the same directory as the client secret, so both files live together.
fn token_cache_path_for(client_secret_path: &Path) -> PathBuf {
    client_secret_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(TOKEN_CACHE_FILE_NAME)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    // --- Step 1: Load the client secret JSON downloaded from Google Cloud Console ---
    let secret_path = if args.len() < 2 {
        "./credentials/client_secret.json"
    } else {
        args[1].as_str()
    };
    if !Path::new(secret_path).exists() {
        eprintln!(
            "ERROR: {} not found. Place your downloaded client secret JSON in the project root.",
            secret_path
        );
        exit_codes::exit(GENERAL_ERROR);
    }
    println!("Using credentials in {secret_path}");

    let secret = yup_oauth2::read_application_secret(secret_path)
        .await
        .expect("Failed to read client secret JSON");

    // --- Step 2: Build the authenticator ---
    // InstalledFlowReturnMethod::HTTPRedirect spins up a local server on port 8080,
    // catches the OAuth callback automatically — no manual copy-pasting of codes.
    // The token is persisted to token_cache.json (next to the client secret file)
    // so you only log in once.
    let token_cache_path = token_cache_path_for(Path::new(secret_path));
    println!("Token cache will be written to {}", token_cache_path.display());
    let auth = InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
        .persist_tokens_to_disk(token_cache_path)
        .build()
        .await?;

    // --- Step 3: Request an access token for the Gmail read-only scope ---
    let scopes = &[
        "https://www.googleapis.com/auth/gmail.readonly",
        "https://www.googleapis.com/auth/gmail.send",
    ];
    let token = auth.token(scopes).await?;
    let access_token = token.token().expect("No access token returned");

    println!("Successfully authenticated!");
    println!("Access token (first 20 chars): {}...", &access_token[..20]);

    // --- Step 4: Call the Gmail API to list inbox messages ---
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{}/users/me/messages", GMAIL_API))
        .bearer_auth(access_token)
        .query(&[("labelIds", "INBOX"), ("maxResults", "10")])
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await?;
        eprintln!("Gmail API error {}: {}", status, body);
        exit_codes::exit(GENERAL_ERROR);
    }

    let messages: serde_json::Value = response.json().await?;

    // --- Step 5: Print each message subject ---
    let message_list = messages["messages"]
        .as_array()
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    if message_list.is_empty() {
        println!("No messages found in inbox.");
        return Ok(());
    }

    println!(
        "\nFetching subjects for {} messages...\n",
        message_list.len()
    );

    for msg in message_list {
        let msg_id = msg["id"].as_str().unwrap_or_default();

        // Re-fetch token for each request (yup-oauth2 auto-refreshes if expired)
        let token = auth.token(scopes).await?;
        let access_token = token.token().expect("No access token");

        let detail: serde_json::Value = client
            .get(format!("{}/users/me/messages/{}", GMAIL_API, msg_id))
            .bearer_auth(access_token)
            .query(&[("format", "metadata"), ("metadataHeaders", "Subject")])
            .send()
            .await?
            .json()
            .await?;

        // Extract subject from headers
        let subject = detail["payload"]["headers"]
            .as_array()
            .and_then(|headers| {
                headers.iter().find(|h| {
                    h["name"]
                        .as_str()
                        .map(|n| n.eq_ignore_ascii_case("subject"))
                        .unwrap_or(false)
                })
            })
            .and_then(|h| h["value"].as_str())
            .unwrap_or("(no subject)");

        println!("  [{}] {}", msg_id, subject);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_cache_is_placed_next_to_client_secret() {
        let path = token_cache_path_for(Path::new("/some/dir/client_secret.json"));
        assert_eq!(path, Path::new("/some/dir/token_cache.json"));
    }

    #[test]
    fn token_cache_for_default_location_is_in_credentials_dir() {
        let path = token_cache_path_for(Path::new("./credentials/client_secret.json"));
        assert_eq!(path, Path::new("./credentials/token_cache.json"));
    }

    #[test]
    fn token_cache_for_bare_file_name_is_in_current_dir() {
        let path = token_cache_path_for(Path::new("client_secret.json"));
        assert_eq!(path, Path::new("token_cache.json"));
    }
}
