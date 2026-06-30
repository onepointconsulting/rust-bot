use std::path::Path;
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

// Gmail API base URL
const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // --- Step 1: Load the client secret JSON downloaded from Google Cloud Console ---
    let secret_path = "./credentials/client_secret.json";
    if !Path::new(secret_path).exists() {
        eprintln!("ERROR: {} not found. Place your downloaded client secret JSON in the project root.", secret_path);
        std::process::exit(1);
    }

    let secret = yup_oauth2::read_application_secret(secret_path)
        .await
        .expect("Failed to read client secret JSON");

    // --- Step 2: Build the authenticator ---
    // InstalledFlowReturnMethod::HTTPRedirect spins up a local server on port 8080,
    // catches the OAuth callback automatically — no manual copy-pasting of codes.
    // The token is persisted to token_cache.json so you only log in once.
    let auth = InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
        .persist_tokens_to_disk("token_cache.json")
        .build()
        .await?;

    // --- Step 3: Request an access token for the Gmail read-only scope ---
    let scopes = &[
        "https://www.googleapis.com/auth/gmail.readonly", "https://www.googleapis.com/auth/gmail.send"
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
        .query(&[
            ("labelIds", "INBOX"),
            ("maxResults", "10"),
        ])
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await?;
        eprintln!("Gmail API error {}: {}", status, body);
        std::process::exit(1);
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

    println!("\nFetching subjects for {} messages...\n", message_list.len());

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
                    h["name"].as_str().map(|n| n.eq_ignore_ascii_case("subject")).unwrap_or(false)
                })
            })
            .and_then(|h| h["value"].as_str())
            .unwrap_or("(no subject)");

        println!("  [{}] {}", msg_id, subject);
    }

    Ok(())
}
