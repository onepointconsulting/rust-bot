use std::path::PathBuf;

use chrono::{Datelike, Local, Months};
use dotenv::dotenv;
use rust_bot::{
    agent::tools::{base::Tool, gmail::GmailEmailsTool},
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
async fn test_gmail_tool() {
    // Note: this test will only work if you have a valid client_secret.json and token_cache.json in the default workspace.
    dotenv().ok();
    let config =
        load_config(Some(PathBuf::from("./configs/openai-compat/config_gmail.json")));
    let gmail_tool = GmailEmailsTool::new(config.tools.gmail);
    let params = current_month_gmail_params(10);
    let result = gmail_tool.execute(&params).await;
    println!("Gmail query params: {}", params);
    println!("{}", result);
}
