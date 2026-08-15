//! Shared REST client pieces reused by both chat frontends.
//!
//! `login()` posts to `/v1/login`, a REST endpoint exposed identically by
//! both the `rust-bot api` server and the WebSocket gateway. Requests are
//! relative so the call works unmodified whether the frontend is served
//! same-origin or proxied during `trunk serve`.

use gloo_net::http::Request;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub message: String,
}

impl ApiError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Debug, Serialize)]
struct LoginRequest<'a> {
    email: &'a str,
    password: &'a str,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    token: String,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ErrorDetail {
    message: String,
}

/// Build an [`ApiError`] from a non-2xx response, preferring the server's
/// structured `{"error": {"message": ...}}` body when present.
pub async fn error_from_response(resp: gloo_net::http::Response) -> ApiError {
    let status = resp.status();
    match resp.json::<ErrorBody>().await {
        Ok(body) => ApiError::new(body.error.message),
        Err(_) => ApiError::new(format!("Request failed with status {status}")),
    }
}

/// Authenticate with email/password and return a freshly minted JWT.
pub async fn login(email: &str, password: &str) -> Result<String, ApiError> {
    let resp = Request::post("/v1/login")
        .json(&LoginRequest { email, password })
        .map_err(|e| ApiError::new(e.to_string()))?
        .send()
        .await
        .map_err(|e| ApiError::new(e.to_string()))?;

    if !resp.ok() {
        return Err(error_from_response(resp).await);
    }

    resp.json::<LoginResponse>()
        .await
        .map(|body| body.token)
        .map_err(|e| ApiError::new(e.to_string()))
}
