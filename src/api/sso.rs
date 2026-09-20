//! `POST /v1/sso/strapi` — exchange a Strapi admin JWT for a rust-bot WebUI
//! JWT. Mounted on the combined gateway next to `/v1/login` (same
//! [`LoginState`], CORS layer, and websocket `purpose=webui` signer).

use std::time::Duration;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::login::LoginState;
use crate::api::rest::ApiError;
use crate::api::types::ChatLoginResponse;
use crate::api::user_registry::User;
use crate::security::jwt::{DEFAULT_EXPIRES_IN_MONTHS, generate_jwt_token};

const USERS_ME_TIMEOUT: Duration = Duration::from_secs(10);

/// Body accepted by `POST /v1/sso/strapi`. The Strapi origin is never taken
/// from the client — rust-bot calls its configured `gateway.strapiSso.strapiUrl`.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StrapiSsoRequest {
    pub strapi_jwt: String,
}

/// Authenticate a Strapi admin JWT by calling `{strapiUrl}/admin/users/me`,
/// map-or-create the rust-bot user by email, and mint a WebUI JWT.
#[utoipa::path(
    post,
    path = "/v1/sso/strapi",
    request_body = StrapiSsoRequest,
    responses(
        (status = 200, description = "Freshly minted rust-bot JWT for the Strapi admin", body = ChatLoginResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "security"
)]
pub(crate) async fn sso_strapi(
    State(state): State<std::sync::Arc<LoginState>>,
    Json(request): Json<StrapiSsoRequest>,
) -> Result<Json<ChatLoginResponse>, ApiError> {
    let strapi_url = state.strapi_url.as_deref().ok_or_else(|| {
        ApiError::internal("Strapi SSO is not configured (gateway.strapiSso.strapiUrl is empty)")
    })?;
    let strapi_jwt = request.strapi_jwt.trim();
    if strapi_jwt.is_empty() {
        return Err(ApiError::unauthorized("Missing Strapi admin JWT"));
    }

    let jwt = state
        .jwt_auth
        .as_ref()
        .ok_or_else(|| ApiError::internal("JWT is not enabled; cannot mint SSO tokens"))?;

    let email = fetch_strapi_admin_email(strapi_url, strapi_jwt).await?;

    let private_key_path = jwt.private_key_path.clone();
    let iss = jwt.opts.iss.clone();
    let aud = jwt.opts.aud.clone();
    let purpose = state.token_purpose.clone();
    let sub = Some(email.clone());
    let minted = tokio::task::spawn_blocking(move || {
        generate_jwt_token(
            private_key_path,
            iss,
            aud,
            purpose,
            DEFAULT_EXPIRES_IN_MONTHS,
            sub,
        )
    })
    .await
    .map_err(|_| ApiError::internal("Token minting task failed"))?
    .map_err(|err| ApiError::internal(format!("Failed to mint JWT: {err}")))?;

    persist_sso_user(&state, &email, &minted.token);

    Ok(Json(ChatLoginResponse {
        token: minted.token,
    }))
}

fn persist_sso_user(state: &LoginState, email: &str, token: &str) {
    let mut registry = state
        .user_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let existing = registry.get_user_by_email(email).ok();
    let user = User {
        email: email.to_string(),
        password_hash: existing.as_ref().and_then(|u| u.password_hash.clone()),
        token: token.to_string(),
    };
    let result = if existing.is_some() {
        registry.update_user(email, &user)
    } else {
        registry.register_user(&user)
    };
    if let Err(err) = result {
        log::warn!("Failed to persist SSO user {email}: {err}");
    }
}

async fn fetch_strapi_admin_email(strapi_url: &str, strapi_jwt: &str) -> Result<String, ApiError> {
    let url = format!("{}/admin/users/me", strapi_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(USERS_ME_TIMEOUT)
        .build()
        .map_err(|err| ApiError::internal(format!("Failed to build HTTP client: {err}")))?;
    let response = client
        .get(&url)
        .bearer_auth(strapi_jwt)
        .send()
        .await
        .map_err(|err| ApiError::unauthorized(format!("Failed to reach Strapi at {url}: {err}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| ApiError::unauthorized(format!("Failed to read Strapi response: {err}")))?;
    if !status.is_success() {
        return Err(ApiError::unauthorized(format!(
            "Strapi /admin/users/me returned {status}"
        )));
    }
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|err| {
        ApiError::unauthorized(format!(
            "Strapi /admin/users/me returned invalid JSON: {err}"
        ))
    })?;
    parse_strapi_admin_email(&value)
        .ok_or_else(|| ApiError::unauthorized("Strapi user has no email"))
}

/// Pull the admin email out of a `/admin/users/me` body. Strapi 5 typically
/// nests it under `data`; older shapes use `user` or a top-level `email`.
pub(crate) fn parse_strapi_admin_email(value: &serde_json::Value) -> Option<String> {
    fn from_obj(obj: &serde_json::Value) -> Option<String> {
        obj.get("email")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
    from_obj(value)
        .or_else(|| value.get("data").and_then(from_obj))
        .or_else(|| value.get("user").and_then(from_obj))
        .or_else(|| value.pointer("/data/user").and_then(from_obj))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::login::{JwtAuthState, LoginState};
    use crate::api::user_registry::JsonUserRegistry;
    use crate::security::jwt::{
        JwtValidationOpts, generate_jwt_keypair, validate_jwt_token_from_path,
    };
    use axum::extract::State;
    use axum::routing::get;
    use axum::{Json as AxumJson, Router};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    #[test]
    fn parse_email_from_strapi5_data_envelope() {
        let value = json!({"data": {"id": 1, "email": "admin@example.com"}});
        assert_eq!(
            parse_strapi_admin_email(&value).as_deref(),
            Some("admin@example.com")
        );
    }

    #[test]
    fn parse_email_from_user_envelope() {
        let value = json!({"user": {"email": " editor@example.com "}});
        assert_eq!(
            parse_strapi_admin_email(&value).as_deref(),
            Some("editor@example.com")
        );
    }

    #[test]
    fn parse_email_from_top_level() {
        let value = json!({"email": "root@example.com"});
        assert_eq!(
            parse_strapi_admin_email(&value).as_deref(),
            Some("root@example.com")
        );
    }

    #[test]
    fn parse_email_missing_is_none() {
        let value = json!({"data": {"id": 1}});
        assert!(parse_strapi_admin_email(&value).is_none());
    }

    async fn spawn_users_me(body: serde_json::Value, status: axum::http::StatusCode) -> String {
        async fn ok(
            axum::extract::State((body, status)): axum::extract::State<(
                serde_json::Value,
                axum::http::StatusCode,
            )>,
        ) -> (axum::http::StatusCode, AxumJson<serde_json::Value>) {
            (status, AxumJson(body))
        }
        let app = Router::new()
            .route("/admin/users/me", get(ok))
            .with_state((body, status));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn sso_state(jwt_auth: Option<JwtAuthState>, strapi_url: Option<String>) -> Arc<LoginState> {
        Arc::new(LoginState {
            jwt_auth,
            user_registry: Arc::new(Mutex::new(JsonUserRegistry::empty())),
            token_purpose: "webui".to_string(),
            strapi_url,
        })
    }

    #[tokio::test]
    async fn sso_fails_when_unconfigured() {
        let err = sso_strapi(
            State(sso_state(None, None)),
            Json(StrapiSsoRequest {
                strapi_jwt: "anything".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("not configured"));
    }

    #[tokio::test]
    async fn sso_rejects_empty_jwt() {
        let err = sso_strapi(
            State(sso_state(None, Some("http://127.0.0.1:1337".into()))),
            Json(StrapiSsoRequest {
                strapi_jwt: "  ".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.message(), "Missing Strapi admin JWT");
    }

    #[tokio::test]
    async fn sso_rejects_strapi_unauthorized() {
        let url = spawn_users_me(
            json!({"error": "nope"}),
            axum::http::StatusCode::UNAUTHORIZED,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let jwt_auth = Some(JwtAuthState {
            public_key_pem: Arc::new(std::fs::read(&keys.public_key_path).unwrap()),
            private_key_path: keys.private_key_path.display().to_string(),
            opts: JwtValidationOpts {
                iss: "rust-bot".to_string(),
                aud: "/ws".to_string(),
            },
        });
        let err = sso_strapi(
            State(sso_state(jwt_auth, Some(url))),
            Json(StrapiSsoRequest {
                strapi_jwt: "bad".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("401"));
    }

    #[tokio::test]
    async fn sso_mints_webui_token_and_creates_user() {
        let url = spawn_users_me(
            json!({"data": {"id": 7, "email": "admin@example.com"}}),
            axum::http::StatusCode::OK,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let registry = JsonUserRegistry::open(dir.path().join("users.json")).unwrap();
        let jwt_auth = Some(JwtAuthState {
            public_key_pem: Arc::new(std::fs::read(&keys.public_key_path).unwrap()),
            private_key_path: keys.private_key_path.display().to_string(),
            opts: JwtValidationOpts {
                iss: "rust-bot".to_string(),
                aud: "/ws".to_string(),
            },
        });
        let state = Arc::new(LoginState {
            jwt_auth,
            user_registry: Arc::new(Mutex::new(registry)),
            token_purpose: "webui".to_string(),
            strapi_url: Some(url),
        });

        let response = sso_strapi(
            State(state.clone()),
            Json(StrapiSsoRequest {
                strapi_jwt: "strapi-admin-jwt".to_string(),
            }),
        )
        .await
        .unwrap();

        let claims = validate_jwt_token_from_path(
            &response.token,
            &keys.public_key_path,
            &JwtValidationOpts {
                iss: "rust-bot".to_string(),
                aud: "/ws".to_string(),
            },
        )
        .unwrap();
        assert_eq!(claims.sub, "admin@example.com");
        assert_eq!(claims.purpose.as_deref(), Some("webui"));
        assert_eq!(claims.aud.as_deref(), Some("/ws"));

        let stored = state
            .user_registry
            .lock()
            .unwrap()
            .get_user_by_email("admin@example.com")
            .unwrap();
        assert!(stored.password_hash.is_none());
        assert_eq!(stored.token, response.token);
    }
}
