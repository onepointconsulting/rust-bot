use crate::{agent::tools::base::Tool, config::schema::GmailToolConfig};
use async_trait::async_trait;
use base64::Engine;
use std::path::{Path, PathBuf};
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

const DEFAULT_LIMIT: u32 = 20;
const DEFAULT_BODY_LIMIT: usize = 500;

const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1";

fn gmail_err(msg: impl Into<String>) -> String {
    let msg = msg.into();
    log::error!("{}", msg);
    msg
}

fn sanitize_mime_header_value(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// RFC 2047 encoded-word for non-ASCII header values (Subject, etc.).
fn encode_mime_header_value(value: &str) -> String {
    let sanitized = sanitize_mime_header_value(value);
    if sanitized.is_ascii() {
        return sanitized;
    }
    use base64::engine::general_purpose::STANDARD;
    format!("=?UTF-8?B?{}?=", STANDARD.encode(sanitized.as_bytes()))
}

fn normalize_mime_body(body: &str) -> String {
    body.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n")
}

fn build_rfc2822_message(to: &str, subject: &str, body: &str, format: &str) -> String {
    let subject = encode_mime_header_value(subject);
    let body = normalize_mime_body(body);
    let content_type = match format {
        "plain" => "text/plain; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        _ => unreachable!("format validated by parse_email_format"),
    };
    format!(
        "To: {to}\r\nSubject: {subject}\r\nMIME-Version: 1.0\r\nContent-Type: {content_type}\r\n\r\n{body}"
    )
}

fn parse_email_format(format: &str) -> Result<&'static str, String> {
    match format {
        "plain" => Ok("plain"),
        "html" => Ok("html"),
        other => Err(format!("Error: invalid format: {other} (expected plain or html)")),
    }
}

fn encode_gmail_raw_message(mime: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.encode(mime.as_bytes())
}

struct GmailToolCommon {
    config: GmailToolConfig,
    secret_path: String,
    token_cache_path: String,
    scopes: Vec<String>,
}

impl GmailToolCommon {
    async fn access_token(&self) -> Result<String, String> {
        let secret = yup_oauth2::read_application_secret(self.secret_path.clone())
            .await
            .map_err(|_| "Error: failed to read client secret JSON".to_string())?;

        log::info!("Reading token from {}", self.token_cache_path);
        let auth =
            InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
                .persist_tokens_to_disk(self.token_cache_path.clone())
                .build()
                .await
                .map_err(|_| "Error: failed to read client secret JSON".to_string())?;

        let token = auth
            .token(self.scopes.as_slice())
            .await
            .map_err(|_| "Error: failed to get token".to_string())?;

        token
            .token()
            .map(str::to_string)
            .ok_or_else(|| "Error: failed to get token".to_string())
    }
}

pub struct GmailEmailsTool {
    name: String,
    description: String,
    common: GmailToolCommon,
}

fn show_error_and_exit(path: &PathBuf) {
    let error_message = format!("ERROR: {} not found. Make sure this path exists.", path.display());
    log::error!("{}", error_message);
    eprintln!("{}", error_message);
    std::process::exit(4); // EXIT_CONFIG_ERROR
}

impl GmailEmailsTool {
    pub fn new(config: GmailToolConfig) -> Self {
        let secret_path = config.client_secret_path();
        if !Path::new(&secret_path).exists() {
            show_error_and_exit(&secret_path); // EXIT_CONFIG_ERROR
        }
        let token_cache_path = config.token_cache_path();
        if !Path::new(&token_cache_path).exists() {
            show_error_and_exit(&token_cache_path); // EXIT_CONFIG_ERROR
        }
        let common = GmailToolCommon {
            config: config.clone(),
            secret_path: secret_path.to_string_lossy().to_string(),
            token_cache_path: token_cache_path.to_string_lossy().to_string(),
            scopes: vec!["https://www.googleapis.com/auth/gmail.readonly".to_string()],
        };
        Self {
            name: "gmail".to_string(),
            description: "Gmail Tool. Returns the latest emails from the user's inbox.".to_string(),
            common,
        }
    }

    fn normalize_gmail_date(date: &str) -> Result<String, ()> {
        let trimmed = date.trim();
        if trimmed.is_empty() {
            return Err(());
        }
        if trimmed.len() == 10 && trimmed.chars().nth(4) == Some('-') {
            let parts: Vec<&str> = trimmed.split('-').collect();
            if parts.len() == 3 {
                return Ok(format!("{}/{}/{}", parts[0], parts[1], parts[2]));
            }
        }
        if trimmed.contains('/') {
            return Ok(trimmed.to_string());
        }
        Err(())
    }

    fn build_gmail_query(after: Option<&str>, before: Option<&str>) -> Result<String, String> {
        let mut parts = vec!["in:inbox".to_string()];
        if let Some(d) = after {
            let normalized = Self::normalize_gmail_date(d)
                .map_err(|_| format!("Error: invalid after date: {}", d))?;
            parts.push(format!("after:{}", normalized));
        }
        if let Some(d) = before {
            let normalized = Self::normalize_gmail_date(d)
                .map_err(|_| format!("Error: invalid before date: {}", d))?;
            parts.push(format!("before:{}", normalized));
        }
        Ok(parts.join(" "))
    }

    fn limit_body(text: &str, body_limit: usize) -> String {
        if body_limit == 0 {
            return String::new();
        }
        if text.chars().count() <= body_limit {
            return text.to_string();
        }
        let truncated: String = text.chars().take(body_limit).collect();
        format!("{truncated}... (truncated)")
    }

    fn format_message_line(
        msg_id: &str,
        date: &str,
        subject: &str,
        only_subject: bool,
        body: &str,
    ) -> String {
        if only_subject {
            format!("{}: {} | {}", msg_id, date, subject)
        } else {
            format!("{}: {} | {}\n{}", msg_id, date, subject, body)
        }
    }

    async fn loop_messages(
        &self,
        client: &reqwest::Client,
        access_token: &str,
        message_list: &[serde_json::Value],
        only_subject: bool,
        body_limit: usize,
    ) -> String {
        let mut final_output: Vec<String> = Vec::new();
        let format = if only_subject { "metadata" } else { "full" };
        for msg in message_list {
            let msg_id = msg["id"].as_str().unwrap_or_default();
            let detail = client
                .get(format!("{}/users/me/messages/{}", GMAIL_API, msg_id))
                .bearer_auth(access_token)
                .query(&[("format", format)])
                .send()
                .await;
            if let Ok(detail) = detail {
                if !detail.status().is_success() {
                    return gmail_err(format!(
                        "Gmail API error {} fetching message {}",
                        detail.status(),
                        msg_id
                    ));
                }
                if let Ok(detail_json) = detail.json::<serde_json::Value>().await {
                    let payload = &detail_json["payload"];
                    let subject =
                        GmailEmailsTool::extract_header(payload, "Subject", "(no subject)");
                    let date = GmailEmailsTool::extract_header(payload, "Date", "(no date)");
                    let line = if only_subject {
                        GmailEmailsTool::format_message_line(
                            msg_id,
                            &date,
                            &subject,
                            true,
                            "",
                        )
                    } else {
                        let body = GmailEmailsTool::limit_body(
                            &GmailEmailsTool::extract_body(payload)
                                .unwrap_or("(no body)".to_string()),
                            body_limit,
                        );
                        GmailEmailsTool::format_message_line(
                            msg_id,
                            &date,
                            &subject,
                            false,
                            &body,
                        )
                    };
                    final_output.push(line);
                } else {
                    return gmail_err(format!(
                        "Failed to parse message details for ID: {}",
                        msg_id
                    ));
                }
            } else {
                return gmail_err(format!(
                    "Failed to fetch message details for ID: {}",
                    msg_id
                ));
            }
        }
        final_output.join("\n")
    }

    fn extract_header(payload: &serde_json::Value, name: &str, default: &str) -> String {
        payload["headers"]
            .as_array()
            .and_then(|headers| {
                headers.iter().find_map(|header| {
                    if header["name"].as_str() == Some(name) {
                        header["value"].as_str().map(str::to_string)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| default.to_string())
    }

    fn decode_gmail_body_data(data: &str) -> Option<String> {
        use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
        let bytes = URL_SAFE_NO_PAD
            .decode(data)
            .or_else(|_| URL_SAFE.decode(data))
            .ok();
        bytes.map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    fn extract_body(payload: &serde_json::Value) -> Option<String> {
        // Leaf part with body data
        if let Some(data) = payload["body"]["data"].as_str() {
            return GmailEmailsTool::decode_gmail_body_data(data);
        }
        // Recurse into multipart parts
        if let Some(parts) = payload["parts"].as_array() {
            // Prefer plain text, then HTML
            for mime in ["text/plain", "text/html"] {
                for part in parts {
                    if part["mimeType"].as_str() == Some(mime) {
                        if let Some(text) = GmailEmailsTool::extract_body(part) {
                            return Some(text);
                        }
                    }
                }
            }
            // Fallback: any nested part
            for part in parts {
                if let Some(text) = GmailEmailsTool::extract_body(part) {
                    return Some(text);
                }
            }
        }
        None
    }
}

#[async_trait]
impl Tool for GmailEmailsTool {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "",
                    "minimum": 1,
                    "maximum": self.common.config.max_results,
                    "default": DEFAULT_LIMIT,
                },
                "after": {
                    "type": "string",
                    "description": "Start date (inclusive), YYYY-MM-DD. Gmail search: after:...",
                },
                "before": {
                    "type": "string",
                    "description": "End date (exclusive), YYYY-MM-DD. Gmail search: before:...",
                },
                "only_subject": {
                    "type": "boolean",
                    "description": "Only return the subject of the email",
                    "default": false,
                },
                "body_limit": {
                    "type": "integer",
                    "description": "Maximum characters per email body (0 omits body text)",
                    "minimum": 0,
                    "default": DEFAULT_BODY_LIMIT,
                },
            },
            "required": ["limit"],
        })
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        let limit = params
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_LIMIT as u64);
        if limit < 1 {
            return "Error: limit must be greater than 0".to_string();
        }
        if limit > self.common.config.max_results as u64 {
            return format!(
                "Error: limit must be less than or equal to {}",
                self.common.config.max_results
            )
            .to_string();
        }

        let after = params
            .get("after")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let before = params
            .get("before")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let only_subject = params
            .get("only_subject")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let body_limit = params
            .get("body_limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_BODY_LIMIT as u64) as usize;
        let query = match Self::build_gmail_query(after, before) {
            Ok(q) => q,
            Err(e) => return e,
        };

        let token = match self.common.access_token().await {
            Ok(token) => token,
            Err(e) => return gmail_err(e),
        };

        log::info!("Successfully authenticated with Gmail API");
        let client = reqwest::Client::new();
        if let Ok(response) = client
            .get(format!("{}/users/me/messages", GMAIL_API))
            .bearer_auth(&token)
            .query(&[
                ("q", &query),
                ("maxResults", &limit.to_string()),
            ])
            .send()
            .await
        {
            if !response.status().is_success() {
                let status = response.status();
                if let Ok(body) = response.text().await {
                    return gmail_err(format!("Gmail API error {}: {}", status, body));
                } else {
                    return gmail_err(format!(
                        "Failed to read response body. Status: {}",
                        status
                    ));
                }
            }
            let messages_result: Result<serde_json::Value, reqwest::Error> = response.json().await;
            if let Ok(messages) = messages_result {
                let message_list = messages["messages"]
                    .as_array()
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                log::info!(
                    "\nFetching subjects for {} messages...\n",
                    message_list.len()
                );
                return self
                    .loop_messages(
                        &client,
                        &token,
                        message_list,
                        only_subject,
                        body_limit,
                    )
                    .await;
            }
            return gmail_err("Failed to parse response JSON");
        }
        gmail_err("Error: failed to fetch emails")
    }
}

pub struct GmailEmailSendTool {
    name: String,
    description: String,
    common: GmailToolCommon,
}

impl GmailEmailSendTool {
    pub fn new(config: GmailToolConfig) -> Self {
        let common = GmailToolCommon {
            config: config.clone(),
            secret_path: config.client_secret_path().to_string_lossy().to_string(),
            token_cache_path: config.token_cache_path().to_string_lossy().to_string(),
            scopes: vec!["https://www.googleapis.com/auth/gmail.send".to_string()],
        };
        Self {
            name: "gmail_email_send".to_string(),
            description: "Gmail Email Send Tool. Sends an automated email to a recipient email address.".to_string(),
            common,
        }
    }
}

#[async_trait]
impl Tool for GmailEmailSendTool {
    
    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "The email address of the recipient",
                },
                "subject": {
                    "type": "string",
                    "description": "The subject of the email",
                },
                "body": {
                    "type": "string",
                    "description": "The body of the email (plain text or HTML, depending on format)",
                },
                "format": {
                    "type": "string",
                    "description": "Email body format: plain (default) or html",
                    "enum": ["plain", "html"],
                }
            },
            "required": ["to", "subject", "body"],
        })
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        let to = params
            .get("to")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let subject = params
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let body = params
            .get("body")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let format = params
            .get("format")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("plain");
        let (Some(to), Some(subject), Some(body)) = (to, subject, body) else {
            return format!("Error: missing required parameters: to={}, subject={}, body={}", 
            to.unwrap_or("(none)"), subject.unwrap_or("(none)"), body.unwrap_or("(none)")).to_string();
        };
        let format = match parse_email_format(format) {
            Ok(format) => format,
            Err(e) => return e,
        };

        let token = match self.common.access_token().await {
            Ok(token) => token,
            Err(e) => return gmail_err(e),
        };

        let raw = encode_gmail_raw_message(&build_rfc2822_message(to, subject, body, format));

        log::info!("Successfully authenticated with Gmail API");
        let client = reqwest::Client::new();
        if let Ok(response) = client
            .post(format!("{}/users/me/messages/send", GMAIL_API))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "raw": raw }))
            .send()
            .await
        {
            if !response.status().is_success() {
                let status = response.status();
                if let Ok(response_body) = response.text().await {
                    return gmail_err(format!("Gmail API error {}: {}", status, response_body));
                } else {
                    return gmail_err(format!("Failed to read response body. Status: {}", status));
                }
            }
            let response_json: Result<serde_json::Value, reqwest::Error> = response.json().await;
            if let Ok(response_json) = response_json {
                if let Some(id) = response_json.get("id").and_then(|v| v.as_str()) {
                    return format!("Email sent (id: {id})");
                }
                return response_json.to_string();
            }
            return gmail_err("Failed to parse response JSON");
        }
        gmail_err("Error: failed to send email")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rfc2822_message_formats_plain_headers_and_body() {
        let mime = build_rfc2822_message(
            "a@example.com",
            "Hello",
            "Line one\nLine two",
            "plain",
        );
        assert!(mime.starts_with("To: a@example.com\r\n"));
        assert!(mime.contains("Subject: Hello\r\n"));
        assert!(mime.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(mime.ends_with("Line one\r\nLine two"));
    }

    #[test]
    fn build_rfc2822_message_uses_html_content_type() {
        let mime = build_rfc2822_message(
            "a@example.com",
            "Hello",
            "<p>Line one</p>",
            "html",
        );
        assert!(mime.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(mime.ends_with("<p>Line one</p>"));
    }

    #[test]
    fn build_rfc2822_message_strips_newlines_from_subject() {
        let mime = build_rfc2822_message("a@example.com", "Hello\r\nWorld", "body", "plain");
        assert!(mime.contains("Subject: Hello World\r\n"));
    }

    #[test]
    fn build_rfc2822_message_rfc2047_encodes_non_ascii_subject() {
        let mime = build_rfc2822_message(
            "a@example.com",
            "Rust-bot — Overview of Functionality",
            "body",
            "plain",
        );
        assert!(mime.contains("Subject: =?UTF-8?B?"));
        assert!(!mime.contains("Subject: Rust-bot —"));
    }

    #[test]
    fn parse_email_format_rejects_unknown_values() {
        assert_eq!(
            parse_email_format("markdown"),
            Err("Error: invalid format: markdown (expected plain or html)".to_string())
        );
    }

    #[test]
    fn encode_gmail_raw_message_is_url_safe_without_padding() {
        let raw = encode_gmail_raw_message("To: a@example.com\r\n\r\nHello");
        assert!(!raw.contains('+'));
        assert!(!raw.contains('/'));
        assert!(!raw.ends_with('='));
    }

    #[test]
    fn encode_gmail_raw_message_round_trips_with_decode_helper() {
        let mime = build_rfc2822_message("a@example.com", "Hi", "Body", "plain");
        let raw = encode_gmail_raw_message(&mime);
        assert_eq!(
            GmailEmailsTool::decode_gmail_body_data(&raw),
            Some(mime)
        );
    }

    #[test]
    fn limit_body_truncates_long_text() {
        assert_eq!(
            GmailEmailsTool::limit_body("0123456789", 5),
            "01234... (truncated)"
        );
    }

    #[test]
    fn limit_body_short_text_unchanged() {
        assert_eq!(GmailEmailsTool::limit_body("short", 10), "short");
    }

    #[test]
    fn limit_body_zero_returns_empty() {
        assert_eq!(GmailEmailsTool::limit_body("anything", 0), "");
    }

    #[test]
    fn limit_body_unicode_at_boundary_does_not_panic() {
        let text = "x".repeat(499) + "\u{2007}";
        assert_eq!(GmailEmailsTool::limit_body(&text, 500), text);
        assert_eq!(
            GmailEmailsTool::limit_body(&text, 499),
            format!("{}... (truncated)", "x".repeat(499))
        );
    }

    #[test]
    fn format_message_line_includes_body_by_default() {
        let line = GmailEmailsTool::format_message_line(
            "abc123",
            "Mon, 1 Jun 2026",
            "Hello",
            false,
            "body text",
        );
        assert_eq!(line, "abc123: Mon, 1 Jun 2026 | Hello\nbody text");
    }

    #[test]
    fn format_message_line_only_subject_omits_body() {
        let line = GmailEmailsTool::format_message_line(
            "abc123",
            "Mon, 1 Jun 2026",
            "Hello",
            true,
            "ignored",
        );
        assert_eq!(line, "abc123: Mon, 1 Jun 2026 | Hello");
    }

    #[test]
    fn normalize_gmail_date_accepts_iso_and_slash_formats() {
        assert_eq!(
            GmailEmailsTool::normalize_gmail_date("2026-06-01").unwrap(),
            "2026/06/01"
        );
        assert_eq!(
            GmailEmailsTool::normalize_gmail_date(" 2026-06-01 ").unwrap(),
            "2026/06/01"
        );
        assert_eq!(
            GmailEmailsTool::normalize_gmail_date("2026/06/01").unwrap(),
            "2026/06/01"
        );
    }

    #[test]
    fn normalize_gmail_date_rejects_invalid() {
        assert!(GmailEmailsTool::normalize_gmail_date("").is_err());
        assert!(GmailEmailsTool::normalize_gmail_date("2026").is_err());
        assert!(GmailEmailsTool::normalize_gmail_date("not-a-date").is_err());
    }

    #[test]
    fn build_gmail_query_inbox_only() {
        assert_eq!(
            GmailEmailsTool::build_gmail_query(None, None).unwrap(),
            "in:inbox"
        );
    }

    #[test]
    fn build_gmail_query_with_date_range() {
        assert_eq!(
            GmailEmailsTool::build_gmail_query(Some("2026-06-01"), Some("2026-07-01")).unwrap(),
            "in:inbox after:2026/06/01 before:2026/07/01"
        );
    }

    #[test]
    fn build_gmail_query_rejects_invalid_dates() {
        assert!(
            GmailEmailsTool::build_gmail_query(Some("bad-date"), None)
                .unwrap_err()
                .contains("invalid after date")
        );
        assert!(
            GmailEmailsTool::build_gmail_query(None, Some("bad-date"))
                .unwrap_err()
                .contains("invalid before date")
        );
    }

    #[test]
    fn extract_header_returns_value_or_default() {
        let payload = serde_json::json!({
            "headers": [
                { "name": "Subject", "value": "Test subject" },
                { "name": "Date", "value": "Mon, 1 Jun 2026 10:00:00 +0000" }
            ]
        });
        assert_eq!(
            GmailEmailsTool::extract_header(&payload, "Subject", "(no subject)"),
            "Test subject"
        );
        assert_eq!(
            GmailEmailsTool::extract_header(&payload, "From", "(no from)"),
            "(no from)"
        );
    }

    #[test]
    fn decode_gmail_body_data_decodes_url_safe_base64() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let encoded = URL_SAFE_NO_PAD.encode("Hello, Gmail!");
        assert_eq!(
            GmailEmailsTool::decode_gmail_body_data(&encoded),
            Some("Hello, Gmail!".to_string())
        );
    }

    #[test]
    fn extract_body_prefers_plain_text_over_html() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let plain = URL_SAFE_NO_PAD.encode("plain body");
        let html = URL_SAFE_NO_PAD.encode("<p>html body</p>");
        let payload = serde_json::json!({
            "parts": [
                {
                    "mimeType": "text/html",
                    "body": { "data": html }
                },
                {
                    "mimeType": "text/plain",
                    "body": { "data": plain }
                }
            ]
        });
        assert_eq!(
            GmailEmailsTool::extract_body(&payload),
            Some("plain body".to_string())
        );
    }

    #[test]
    fn extract_body_from_nested_multipart() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let encoded = URL_SAFE_NO_PAD.encode("nested text");
        let payload = serde_json::json!({
            "parts": [
                {
                    "mimeType": "multipart/alternative",
                    "parts": [
                        {
                            "mimeType": "text/plain",
                            "body": { "data": encoded }
                        }
                    ]
                }
            ]
        });
        assert_eq!(
            GmailEmailsTool::extract_body(&payload),
            Some("nested text".to_string())
        );
    }
}
