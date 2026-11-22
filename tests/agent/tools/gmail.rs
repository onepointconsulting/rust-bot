use std::path::PathBuf;

use chrono::{Datelike, Local, Months};
use dotenv::dotenv;
use rust_bot::{
    agent::tools::{
        base::Tool,
        gmail::{GmailEmailSendTool, GmailEmailsTool},
    },
    config::loader::load_config,
};

/// Gmail `after` is inclusive; `before` is exclusive — use the 1st of next month
/// to cover the full current calendar month.
fn current_month_gmail_params(limit: u32) -> serde_json::Value {
    let today = Local::now().date_naive();
    let month_start = today
        .with_day(1)
        .expect("first day of month should always exist");
    let next_month_start = month_start
        .checked_add_months(Months::new(1))
        .expect("next month start should be valid");

    serde_json::json!({
        "limit": limit,
        "after": month_start.format("%Y-%m-%d").to_string(),
        "before": next_month_start.format("%Y-%m-%d").to_string(),
    })
}

#[tokio::test]
#[ignore]
async fn test_gmail_tool() {
    // Note: this test will only work if you have a valid client_secret.json and token_cache.json in the default workspace.
    dotenv().ok();
    let config = load_config(Some(PathBuf::from(
        "./configs/openai-compat/config_gmail.json",
    )));
    let gmail_tool = GmailEmailsTool::new(config.tools.gmail);
    let params = current_month_gmail_params(10);
    let result = gmail_tool.execute(&params).await;
    println!("Gmail query params: {}", params);
    println!("{}", result);
}

#[tokio::test]
#[ignore]
async fn test_gmail_send_tool() {
    dotenv().ok();
    let config = load_config(Some(PathBuf::from(
        "./configs/openai-compat/config_gmail.json",
    )));
    let gmail_tool = GmailEmailSendTool::new(config.tools.gmail);
    let params = serde_json::json!({
        "to": "gil.fernandes@gmail.com",
        "subject": "Test Email",
        "body": "<p>This is a test email. This is <b>bold</b> and <i>italic</i>.</p>",
        "format": "html",
    });
    let result = gmail_tool.execute(&params).await;
    println!("{}", result);
}

/// Gmail config whose credential files would live in `dir` (none are created).
fn gmail_config_without_credentials(
    dir: &std::path::Path,
) -> rust_bot::config::schema::GmailToolConfig {
    rust_bot::config::schema::GmailToolConfig {
        enable: true,
        client_secret_path: dir.join("client_secret.json").to_string_lossy().to_string(),
        token_cache_path: dir.join("token_cache.json").to_string_lossy().to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn gmail_tool_without_credentials_reports_the_missing_file_instead_of_exiting() {
    let dir = tempfile::tempdir().unwrap();
    let gmail_tool = GmailEmailsTool::new(gmail_config_without_credentials(dir.path()));
    let result = gmail_tool.execute(&current_month_gmail_params(1)).await;
    // Builds with a built-in client only ask to sign in; others name the missing file.
    let expected = if rust_bot::agent::tools::gmail_oauth::built_in_client_secret().is_some() {
        "not signed in"
    } else {
        "client_secret.json"
    };
    assert!(result.contains(expected), "{result}");
    assert!(result.contains("gmail-auth"), "{result}");
}

#[tokio::test]
async fn gmail_send_tool_without_token_cache_asks_to_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("client_secret.json"), "{}").unwrap();
    let gmail_tool = GmailEmailSendTool::new(gmail_config_without_credentials(dir.path()));
    let params = serde_json::json!({"to": "a@example.com", "subject": "s", "body": "b"});
    let result = gmail_tool.execute(&params).await;
    assert!(result.contains("not signed in"), "{result}");
    assert!(result.contains("gmail-auth"), "{result}");
}
