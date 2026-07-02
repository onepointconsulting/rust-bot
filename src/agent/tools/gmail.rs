use crate::{agent::tools::base::Tool, config::schema::GmailToolConfig};
use async_trait::async_trait;
use base64::Engine;
use std::path::{Path, PathBuf};
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

const DEFAULT_LIMIT: u32 = 20;
const DEFAULT_BODY_LIMIT: usize = 500;

const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1";
const EMAILS_DIR: &str = "emails";
const ATTACHMENTS_DIR: &str = "attachments";

struct GmailEmailDownloadResult {
    subject: String,
    date: String,
    body: String,
    email_dir: Option<PathBuf>,
    body_file: Option<String>,
    saved_attachments: Vec<String>,
    attachment_errors: Vec<String>,
}

struct AttachmentPart {
    filename: String,
    attachment_id: Option<String>,
    inline_data: Option<String>,
}

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
    body.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
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
        other => Err(format!(
            "Error: invalid format: {other} (expected plain or html)"
        )),
    }
}

fn encode_gmail_raw_message(mime: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.encode(mime.as_bytes())
}

fn parse_filename_from_disposition(disposition: &str) -> Option<String> {
    for segment in disposition.split(';') {
        let segment = segment.trim();
        if let Some(rest) = segment.strip_prefix("filename*=") {
            // RFC 5987: filename*=UTF-8''encoded-name
            let name = rest.split_once("''").map(|(_, n)| n).unwrap_or(rest);
            return Some(name.trim_matches('"').to_string());
        }
        if let Some(rest) = segment.strip_prefix("filename=") {
            return Some(rest.trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

fn sanitize_attachment_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let sanitized = sanitized.trim().trim_start_matches('.');
    if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized.to_string()
    }
}

fn unique_attachment_path(dir: &Path, filename: &str) -> PathBuf {
    let sanitized = sanitize_attachment_filename(filename);
    let candidate = dir.join(&sanitized);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(&sanitized);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment");
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 2.. {
        let name = if let Some(ext) = ext {
            format!("{stem}-{n}.{ext}")
        } else {
            format!("{stem}-{n}")
        };
        let candidate = dir.join(&name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("exhausted attachment filename suffixes")
}

async fn fetch_attachment(
    client: &reqwest::Client,
    access_token: &str,
    msg_id: &str,
    attachment_id: &str,
) -> Result<Vec<u8>, String> {
    let response = client
        .get(format!(
            "{GMAIL_API}/users/me/messages/{msg_id}/attachments/{attachment_id}"
        ))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| format!("Failed to fetch attachment {attachment_id}"))?;
    if !response.status().is_success() {
        return Err(gmail_err(format!(
            "Gmail API error {} fetching attachment {attachment_id}",
            response.status()
        )));
    }
    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|_| format!("Failed to parse attachment response for {attachment_id}"))?;
    let data = json["data"]
        .as_str()
        .ok_or_else(|| format!("No data in attachment response for {attachment_id}"))?;
    GmailEmailsTool::decode_gmail_body_data_bytes(data)
        .ok_or_else(|| format!("Failed to decode attachment {attachment_id}"))
}

async fn save_email_to_disk(
    client: &reqwest::Client,
    access_token: &str,
    msg_id: &str,
    payload: &serde_json::Value,
    save_root: &Path,
) -> Result<(PathBuf, String, Vec<String>, Vec<String>), String> {
    let email_dir = save_root.join(msg_id);
    let attachments_dir = email_dir.join(ATTACHMENTS_DIR);
    std::fs::create_dir_all(&attachments_dir).map_err(|e| {
        gmail_err(format!(
            "Failed to create email directory {}: {e}",
            email_dir.display()
        ))
    })?;

    let (body_content, body_ext) = GmailEmailsTool::extract_body_for_save(payload);
    let body_filename = if body_ext == "html" {
        "body.html"
    } else {
        "body.txt"
    };
    let body_content = if body_content.is_empty() {
        "(no body)".to_string()
    } else {
        body_content
    };
    std::fs::write(email_dir.join(body_filename), &body_content).map_err(|e| {
        gmail_err(format!(
            "Failed to write body file for message {msg_id}: {e}"
        ))
    })?;

    let attachment_parts = GmailEmailsTool::collect_attachment_parts(payload);
    let mut saved_attachments = Vec::new();
    let mut attachment_errors = Vec::new();

    for part in attachment_parts {
        let bytes = if let Some(ref attachment_id) = part.attachment_id {
            match fetch_attachment(client, access_token, msg_id, attachment_id).await {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    attachment_errors.push(format!("{}: {e}", part.filename));
                    None
                }
            }
        } else if let Some(ref data) = part.inline_data {
            GmailEmailsTool::decode_gmail_body_data_bytes(data)
        } else {
            attachment_errors.push(format!("{}: no attachment data", part.filename));
            None
        };

        if let Some(bytes) = bytes {
            let file_path = unique_attachment_path(&attachments_dir, &part.filename);
            let relative = format!(
                "{ATTACHMENTS_DIR}/{}",
                file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("attachment")
            );
            match std::fs::write(&file_path, &bytes) {
                Ok(()) => saved_attachments.push(relative),
                Err(e) => attachment_errors.push(format!("{}: {e}", part.filename)),
            }
        }
    }

    Ok((email_dir, body_filename.to_string(), saved_attachments, attachment_errors))
}

fn format_download_summary(result: &GmailEmailDownloadResult) -> String {
    let email_dir = result
        .email_dir
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut lines = vec![
        format!("Email downloaded to: {email_dir}"),
        format!("Subject: {}", result.subject),
        format!("Date: {}", result.date),
    ];
    if let Some(ref body_file) = result.body_file {
        lines.push(format!("Body file: {body_file}"));
    }
    if result.saved_attachments.is_empty() && result.attachment_errors.is_empty() {
        lines.push("Attachments (0)".to_string());
    } else {
        lines.push(format!("Attachments ({}):", result.saved_attachments.len()));
        for path in &result.saved_attachments {
            lines.push(format!("  - {path}"));
        }
        for err in &result.attachment_errors {
            lines.push(format!("  - Error: {err}"));
        }
    }
    lines.join("\n")
}

async fn download_email(
    client: &reqwest::Client,
    access_token: &str,
    msg_id: &str,
    only_subject: bool,
    body_limit: usize,
    save_root: Option<&Path>,
) -> Result<GmailEmailDownloadResult, String> {
    let format = if only_subject { "metadata" } else { "full" };
    let detail = client
        .get(format!("{GMAIL_API}/users/me/messages/{msg_id}"))
        .bearer_auth(access_token)
        .query(&[("format", format)])
        .send()
        .await;
    if let Ok(detail) = detail {
        if !detail.status().is_success() {
            return Err(gmail_err(format!(
                "Gmail API error {} fetching message {msg_id}",
                detail.status()
            )));
        }
        if let Ok(detail_json) = detail.json::<serde_json::Value>().await {
            let payload = &detail_json["payload"];
            let subject = GmailEmailsTool::extract_header(payload, "Subject", "(no subject)");
            let date = GmailEmailsTool::extract_header(payload, "Date", "(no date)");
            if only_subject {
                Ok(GmailEmailDownloadResult {
                    subject,
                    date,
                    body: String::new(),
                    email_dir: None,
                    body_file: None,
                    saved_attachments: Vec::new(),
                    attachment_errors: Vec::new(),
                })
            } else if let Some(save_root) = save_root {
                let (email_dir, body_file, saved_attachments, attachment_errors) =
                    save_email_to_disk(client, access_token, msg_id, payload, save_root).await?;
                let (body, _) = GmailEmailsTool::extract_body_for_save(payload);
                Ok(GmailEmailDownloadResult {
                    subject,
                    date,
                    body,
                    email_dir: Some(email_dir),
                    body_file: Some(body_file),
                    saved_attachments,
                    attachment_errors,
                })
            } else {
                let body = GmailEmailsTool::limit_body(
                    &GmailEmailsTool::extract_body(payload).unwrap_or("(no body)".to_string()),
                    body_limit,
                );
                Ok(GmailEmailDownloadResult {
                    subject,
                    date,
                    body,
                    email_dir: None,
                    body_file: None,
                    saved_attachments: Vec::new(),
                    attachment_errors: Vec::new(),
                })
            }
        } else {
            Err(gmail_err(format!(
                "Failed to parse message details for ID: {msg_id}"
            )))
        }
    } else {
        Err(gmail_err(format!(
            "Failed to fetch message details for ID: {msg_id}"
        )))
    }
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
    let error_message = format!(
        "ERROR: {} not found. Make sure this path exists.",
        path.display()
    );
    log::error!("{}", error_message);
    eprintln!("{}", error_message);
    std::process::exit(4); // EXIT_CONFIG_ERROR
}

fn validate_secret_and_token_cache_paths(secret_path: &PathBuf, token_cache_path: &PathBuf) {
    if !Path::new(&secret_path).exists() {
        show_error_and_exit(&secret_path); // EXIT_CONFIG_ERROR
    }
    if !Path::new(&token_cache_path).exists() {
        show_error_and_exit(&token_cache_path); // EXIT_CONFIG_ERROR
    }
}

impl GmailEmailsTool {
    pub fn new(config: GmailToolConfig) -> Self {
        let secret_path = config.client_secret_path();
        let token_cache_path = config.token_cache_path();
        validate_secret_and_token_cache_paths(&secret_path, &token_cache_path);
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
        for msg in message_list {
            let msg_id = msg["id"].as_str().unwrap_or_default();
            let result = download_email(
                client,
                access_token,
                msg_id,
                only_subject,
                body_limit,
                None,
            ).await;
            match result {
                Ok(result) => {
                    final_output.push(GmailEmailsTool::format_message_line(
                        msg_id,
                        &result.date,
                        &result.subject,
                        only_subject,
                        &result.body,
                    ));
                }
                Err(e) => {
                    final_output.push(gmail_err(e));
                }
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
        Self::decode_gmail_body_data_bytes(data)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    fn decode_gmail_body_data_bytes(data: &str) -> Option<Vec<u8>> {
        use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
        URL_SAFE_NO_PAD
            .decode(data)
            .or_else(|_| URL_SAFE.decode(data))
            .ok()
    }

    fn extract_body_by_mime(payload: &serde_json::Value, target_mime: &str) -> Option<String> {
        if payload["mimeType"].as_str() == Some(target_mime) {
            if let Some(data) = payload["body"]["data"].as_str() {
                return Self::decode_gmail_body_data(data);
            }
        }
        if let Some(parts) = payload["parts"].as_array() {
            for part in parts {
                if let Some(text) = Self::extract_body_by_mime(part, target_mime) {
                    return Some(text);
                }
            }
        }
        None
    }

    fn extract_body_for_save(payload: &serde_json::Value) -> (String, &'static str) {
        if let Some(html) = Self::extract_body_by_mime(payload, "text/html") {
            return (html, "html");
        }
        if let Some(plain) = Self::extract_body_by_mime(payload, "text/plain") {
            return (plain, "txt");
        }
        (String::new(), "txt")
    }

    fn extract_filename_from_part(part: &serde_json::Value) -> Option<String> {
        let disposition = Self::extract_header(part, "Content-Disposition", "");
        if !disposition.is_empty() {
            if let Some(name) = parse_filename_from_disposition(&disposition) {
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
        None
    }

    fn is_attachment_part(part: &serde_json::Value) -> bool {
        let mime = part["mimeType"].as_str().unwrap_or("");
        if mime.starts_with("multipart/") {
            return false;
        }
        let disposition = Self::extract_header(part, "Content-Disposition", "").to_lowercase();
        if disposition.contains("attachment") {
            return true;
        }
        if mime == "text/plain" || mime == "text/html" {
            return false;
        }
        part["body"]["attachmentId"].as_str().is_some()
    }

    fn collect_attachment_parts(payload: &serde_json::Value) -> Vec<AttachmentPart> {
        let mut parts = Vec::new();
        Self::collect_attachment_parts_recursive(payload, &mut parts, &mut 0);
        parts
    }

    fn collect_attachment_parts_recursive(
        payload: &serde_json::Value,
        parts: &mut Vec<AttachmentPart>,
        index: &mut usize,
    ) {
        if let Some(subparts) = payload["parts"].as_array() {
            for part in subparts {
                if part["parts"].is_array() {
                    Self::collect_attachment_parts_recursive(part, parts, index);
                } else if Self::is_attachment_part(part) {
                    *index += 1;
                    let filename = Self::extract_filename_from_part(part)
                        .unwrap_or_else(|| format!("attachment-{index}"));
                    let attachment_id = part["body"]["attachmentId"]
                        .as_str()
                        .map(str::to_string);
                    let inline_data = if attachment_id.is_none() {
                        part["body"]["data"].as_str().map(str::to_string)
                    } else {
                        None
                    };
                    if attachment_id.is_some() || inline_data.is_some() {
                        parts.push(AttachmentPart {
                            filename,
                            attachment_id,
                            inline_data,
                        });
                    }
                }
            }
        }
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
            .query(&[("q", &query), ("maxResults", &limit.to_string())])
            .send()
            .await
        {
            if !response.status().is_success() {
                let status = response.status();
                if let Ok(body) = response.text().await {
                    return gmail_err(format!("Gmail API error {}: {}", status, body));
                } else {
                    return gmail_err(format!("Failed to read response body. Status: {}", status));
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
                    .loop_messages(&client, &token, message_list, only_subject, body_limit)
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
        let secret_path = config.client_secret_path();
        let token_cache_path = config.token_cache_path();
        validate_secret_and_token_cache_paths(&secret_path, &token_cache_path);
        let common = GmailToolCommon {
            config: config.clone(),
            secret_path: secret_path.to_string_lossy().to_string(),
            token_cache_path: token_cache_path.to_string_lossy().to_string(),
            scopes: vec!["https://www.googleapis.com/auth/gmail.send".to_string()],
        };
        Self {
            name: "gmail_email_send".to_string(),
            description:
                "Gmail Email Send Tool. Sends an automated email to a recipient email address."
                    .to_string(),
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
            return format!(
                "Error: missing required parameters: to={}, subject={}, body={}",
                to.unwrap_or("(none)"),
                subject.unwrap_or("(none)"),
                body.unwrap_or("(none)")
            )
            .to_string();
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

pub struct GmailEmailDownloadTool {
    name: String,
    description: String,
    common: GmailToolCommon,
    workspace: PathBuf,
}

impl GmailEmailDownloadTool {
    pub fn new(config: GmailToolConfig, workspace: PathBuf) -> Self {
        let secret_path = config.client_secret_path();
        let token_cache_path = config.token_cache_path();
        validate_secret_and_token_cache_paths(&secret_path, &token_cache_path);
        let common = GmailToolCommon {
            config: config.clone(),
            secret_path: secret_path.to_string_lossy().to_string(),
            token_cache_path: token_cache_path.to_string_lossy().to_string(),
            scopes: vec!["https://www.googleapis.com/auth/gmail.readonly".to_string()],
        };
        Self {
            name: "gmail_email_download".to_string(),
            description: "Gmail Email Download Tool. Downloads an email from the user's inbox with all attachments to the workspace emails folder.".to_string(),
            common,
            workspace,
        }
    }
}

#[async_trait]
impl Tool for GmailEmailDownloadTool {
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
                "email_id": {
                    "type": "string",
                    "description": "The ID of the email to download",
                },
            },
            "required": ["email_id"],
        })
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        let email_id = params
            .get("email_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(email_id) = email_id else {
            return "Error: email_id is required".to_string();
        };

        let client = reqwest::Client::new();
        let token = match self.common.access_token().await {
            Ok(token) => token,
            Err(e) => return gmail_err(e),
        };

        log::info!("Successfully authenticated with Gmail API");
        let emails_root = self.workspace.join(EMAILS_DIR);
        match download_email(
            &client,
            &token,
            email_id,
            false,
            usize::MAX,
            Some(&emails_root),
        )
        .await
        {
            Ok(result) => format_download_summary(&result),
            Err(e) => gmail_err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rfc2822_message_formats_plain_headers_and_body() {
        let mime = build_rfc2822_message("a@example.com", "Hello", "Line one\nLine two", "plain");
        assert!(mime.starts_with("To: a@example.com\r\n"));
        assert!(mime.contains("Subject: Hello\r\n"));
        assert!(mime.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(mime.ends_with("Line one\r\nLine two"));
    }

    #[test]
    fn build_rfc2822_message_uses_html_content_type() {
        let mime = build_rfc2822_message("a@example.com", "Hello", "<p>Line one</p>", "html");
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
        assert_eq!(GmailEmailsTool::decode_gmail_body_data(&raw), Some(mime));
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

    #[test]
    fn extract_body_for_save_prefers_html_over_plain() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let plain = URL_SAFE_NO_PAD.encode("plain body");
        let html = URL_SAFE_NO_PAD.encode("<p>html body</p>");
        let payload = serde_json::json!({
            "parts": [
                {
                    "mimeType": "text/plain",
                    "body": { "data": plain }
                },
                {
                    "mimeType": "text/html",
                    "body": { "data": html }
                }
            ]
        });
        let (body, ext) = GmailEmailsTool::extract_body_for_save(&payload);
        assert_eq!(body, "<p>html body</p>");
        assert_eq!(ext, "html");
    }

    #[test]
    fn extract_body_for_save_falls_back_to_plain() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let plain = URL_SAFE_NO_PAD.encode("plain only");
        let payload = serde_json::json!({
            "mimeType": "text/plain",
            "body": { "data": plain }
        });
        let (body, ext) = GmailEmailsTool::extract_body_for_save(&payload);
        assert_eq!(body, "plain only");
        assert_eq!(ext, "txt");
    }

    #[test]
    fn decode_gmail_body_data_bytes_round_trips_binary() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let bytes = vec![0u8, 127, 255, 1, 2, 3];
        let encoded = URL_SAFE_NO_PAD.encode(&bytes);
        assert_eq!(
            GmailEmailsTool::decode_gmail_body_data_bytes(&encoded),
            Some(bytes)
        );
    }

    #[test]
    fn sanitize_attachment_filename_strips_invalid_chars() {
        assert_eq!(
            sanitize_attachment_filename("report<>:\"|?*.pdf"),
            "report_______.pdf"
        );
        assert_eq!(sanitize_attachment_filename("   "), "attachment");
    }

    #[test]
    fn unique_attachment_path_deduplicates_collisions() {
        let dir = std::env::temp_dir().join(format!(
            "gmail-attachment-test-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("report.pdf"), b"1").unwrap();
        let second = unique_attachment_path(&dir, "report.pdf");
        assert_eq!(second.file_name().unwrap().to_str().unwrap(), "report-2.pdf");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_attachment_parts_finds_nested_attachments() {
        let payload = serde_json::json!({
            "parts": [
                {
                    "mimeType": "multipart/mixed",
                    "parts": [
                        {
                            "mimeType": "text/plain",
                            "body": { "data": "dGV4dA==" }
                        },
                        {
                            "mimeType": "application/pdf",
                            "filename": "report.pdf",
                            "headers": [
                                {
                                    "name": "Content-Disposition",
                                    "value": "attachment; filename=\"report.pdf\""
                                }
                            ],
                            "body": { "attachmentId": "ANGjdJ8x" }
                        }
                    ]
                }
            ]
        });
        let parts = GmailEmailsTool::collect_attachment_parts(&payload);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].filename, "report.pdf");
        assert_eq!(parts[0].attachment_id.as_deref(), Some("ANGjdJ8x"));
    }

    #[test]
    fn collect_attachment_parts_skips_text_body_parts() {
        let payload = serde_json::json!({
            "parts": [
                {
                    "mimeType": "text/html",
                    "body": { "data": "PGh0bWw+" }
                }
            ]
        });
        assert!(GmailEmailsTool::collect_attachment_parts(&payload).is_empty());
    }

    #[test]
    fn parse_filename_from_disposition_extracts_quoted_name() {
        assert_eq!(
            parse_filename_from_disposition("attachment; filename=\"report.pdf\""),
            Some("report.pdf".to_string())
        );
    }

    #[test]
    fn format_download_summary_includes_paths_and_errors() {
        let result = GmailEmailDownloadResult {
            subject: "Hello".to_string(),
            date: "Mon, 1 Jun 2026".to_string(),
            body: "body".to_string(),
            email_dir: Some(PathBuf::from("/workspace/emails/abc123")),
            body_file: Some("body.html".to_string()),
            saved_attachments: vec!["attachments/report.pdf".to_string()],
            attachment_errors: vec!["bad.bin: fetch failed".to_string()],
        };
        let summary = format_download_summary(&result);
        assert!(summary.contains("Email downloaded to: /workspace/emails/abc123"));
        assert!(summary.contains("Body file: body.html"));
        assert!(summary.contains("attachments/report.pdf"));
        assert!(summary.contains("Error: bad.bin: fetch failed"));
    }
}
