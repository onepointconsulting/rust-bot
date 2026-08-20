//! Shared `/v1/login` handler, reused by both the REST API server
//! (`api::rest::create_api_server`) and the combined login+WebSocket
//! gateway server (`cli::commands::run_gateway`) so the credential check and
//! token-minting logic exist exactly once.

use std::sync::{Arc, Mutex};

use axum::{Json, extract::State};

use crate::api::rest::ApiError;
use crate::api::types::{ChatLoginRequest, ChatLoginResponse};
use crate::api::user_registry::{User, UserRegistry, verify_password};
use crate::config::schema::JwtConfig;
use crate::security::jwt::{DEFAULT_EXPIRES_IN_MONTHS, JwtValidationOpts, generate_jwt_token};

/// Validated JWT signing/validation material derived from a `JwtConfig` at
/// server-start time. Holds the **public** key bytes (for validating
/// incoming bearer tokens) plus the private key **path** and
/// `JwtValidationOpts` (for minting new ones via `login`).
#[derive(Clone)]
pub(crate) struct JwtAuthState {
    pub(crate) public_key_pem: Arc<Vec<u8>>,
    pub(crate) private_key_path: String,
    pub(crate) opts: JwtValidationOpts,
}

/// State backing the `/v1/login` route, independent of whatever larger
/// app state (if any) the hosting server otherwise uses — mounted as its
/// own fully-`with_state`'d sub-router and `.merge()`d in, so `login` never
/// needs to know about `AppState`/`WsShared`/anything else.
#[derive(Clone)]
pub(crate) struct LoginState {
    /// `None` disables minting entirely (`login` returns 500).
    pub(crate) jwt_auth: Option<JwtAuthState>,
    pub(crate) user_registry: Arc<Mutex<dyn UserRegistry + Send>>,
    /// Purpose claim stamped onto every token this state's `login` mints —
    /// `""` for the general REST API (`api::rest`), `"webui"` for the
    /// combined gateway server, whose only client is the WebSocket-based
    /// chat UI (see `security::jwt::Claims::purpose`).
    pub(crate) token_purpose: String,
}

/// Build the `JwtAuthState` a server needs to validate/mint tokens from its
/// own `JwtConfig`. Shared by `api::rest::AppState::from` (validation) and
/// callers building a `LoginState` (minting) so both read the same
/// keypair-loading logic exactly once, regardless of which `JwtConfig`
/// section (the REST API's own, or the WebSocket channel's) they're built
/// from.
pub(crate) fn jwt_auth_state_from_config(jwt: &JwtConfig) -> Option<JwtAuthState> {
    if !jwt.enabled {
        return None;
    }
    let public_key_pem = std::fs::read(&jwt.public_key_path).unwrap_or_else(|e| {
        panic!(
            "JWT enabled but failed to read public key '{}': {e}",
            jwt.public_key_path
        );
    });
    if jwt.aud.trim().is_empty() {
        panic!("JWT enabled but jwt.aud is empty");
    }
    if jwt.private_key_path.trim().is_empty() {
        panic!("JWT enabled but jwt.private_key_path is empty");
    }
    Some(JwtAuthState {
        public_key_pem: Arc::new(public_key_pem),
        private_key_path: jwt.private_key_path.clone(),
        opts: JwtValidationOpts {
            iss: jwt.iss.clone(),
            aud: jwt.aud.clone(),
        },
    })
}

/// Authenticate with email/password, mint a fresh JWT, best-effort persist it
/// in the user registry, and return it. Persistence failures (e.g. read-only
/// FS) are logged and do not fail the login.
#[utoipa::path(
    post,
    path = "/v1/login",
    request_body = ChatLoginRequest,
    responses(
        (status = 200, description = "Freshly minted JWT for the user", body = ChatLoginResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "security"
)]
pub(crate) async fn login(
    State(state): State<Arc<LoginState>>,
    Json(request): Json<ChatLoginRequest>,
) -> Result<Json<ChatLoginResponse>, ApiError> {
    let unauthorized = || ApiError::unauthorized("Invalid email or password");
    let jwt = state
        .jwt_auth
        .as_ref()
        .ok_or_else(|| ApiError::internal("JWT is not enabled; cannot mint login tokens"))?;

    // Copy credentials out so Argon2 does not hold the registry lock.
    let password_hash = {
        let registry = state
            .user_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let user = registry
            .get_user_by_email(&request.email)
            .map_err(|_| unauthorized())?;
        user.password_hash.ok_or_else(unauthorized)?
    };

    let password = request.password.clone();
    let password_hash_for_verify = password_hash.clone();
    let valid = tokio::task::spawn_blocking(move || {
        verify_password(&password, &password_hash_for_verify).unwrap_or(false)
    })
    .await
    .map_err(|_| ApiError::internal("Password verification task failed"))?;

    if !valid {
        return Err(unauthorized());
    }

    let private_key_path = jwt.private_key_path.clone();
    let iss = jwt.opts.iss.clone();
    let aud = jwt.opts.aud.clone();
    let purpose = state.token_purpose.clone();
    let minted = tokio::task::spawn_blocking(move || {
        generate_jwt_token(
            private_key_path,
            iss,
            aud,
            purpose,
            DEFAULT_EXPIRES_IN_MONTHS,
        )
    })
    .await
    .map_err(|_| ApiError::internal("Token minting task failed"))?
    .map_err(|err| ApiError::internal(format!("Failed to mint JWT: {err}")))?;

    {
        let mut registry = state
            .user_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Err(err) = registry.update_user(
            &request.email,
            &User {
                email: request.email.clone(),
                password_hash: Some(password_hash),
                token: minted.token.clone(),
            },
        ) {
            // Registry persistence is bookkeeping only; auth does not consult it.
            // Read-only mounts (common in containers) should not fail login.
            log::warn!("Failed to persist login token for {}: {err}", request.email);
        }
    }

    Ok(Json(ChatLoginResponse {
        token: minted.token,
    }))
}

/// Minimal OpenAPI document for the combined gateway server, which exposes
/// only `/v1/login` (not the full `api::rest::ApiDoc` chat surface). No
/// `bearerAuth` security scheme/modifier is needed — `login`'s own
/// `#[utoipa::path]` doesn't declare `security(...)`, so nothing in this
/// document needs it either.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(login),
    components(schemas(ChatLoginRequest, ChatLoginResponse)),
    tags((name = "security", description = "Authentication and token issuance")),
)]
pub(crate) struct GatewayApiDoc;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::user_registry::JsonUserRegistry;
    use crate::security::jwt::generate_jwt_keypair;

    fn login_state(jwt_auth: Option<JwtAuthState>) -> Arc<LoginState> {
        Arc::new(LoginState {
            jwt_auth,
            user_registry: Arc::new(Mutex::new(JsonUserRegistry::empty())),
            token_purpose: "webui".to_string(),
        })
    }

    fn request(email: &str, password: &str) -> ChatLoginRequest {
        ChatLoginRequest {
            email: email.to_string(),
            password: password.to_string(),
        }
    }

    #[tokio::test]
    async fn login_fails_with_500_when_jwt_disabled() {
        let state = login_state(None);
        let err = login(State(state), Json(request("a@b.com", "pw")))
            .await
            .unwrap_err();
        assert_eq!(
            err.message(),
            "JWT is not enabled; cannot mint login tokens"
        );
    }

    #[tokio::test]
    async fn login_fails_for_unknown_user() {
        let dir = tempfile::tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let jwt_auth = Some(JwtAuthState {
            public_key_pem: Arc::new(std::fs::read(&keys.public_key_path).unwrap()),
            private_key_path: keys.private_key_path.display().to_string(),
            opts: JwtValidationOpts {
                iss: "rust-bot".to_string(),
                aud: String::new(),
            },
        });
        let state = login_state(jwt_auth);
        let err = login(State(state), Json(request("nobody@example.com", "pw")))
            .await
            .unwrap_err();
        assert_eq!(err.message(), "Invalid email or password");
    }

    #[tokio::test]
    async fn login_mints_token_with_configured_purpose_on_success() {
        use crate::api::user_registry::{User, hash_password};

        let dir = tempfile::tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let mut registry = JsonUserRegistry::open(dir.path().join("users.json")).unwrap();
        registry
            .register_user(&User {
                email: "a@b.com".to_string(),
                password_hash: Some(hash_password("correct horse".to_string()).unwrap()),
                token: String::new(),
            })
            .unwrap();
        let state = Arc::new(LoginState {
            jwt_auth: Some(JwtAuthState {
                public_key_pem: Arc::new(std::fs::read(&keys.public_key_path).unwrap()),
                private_key_path: keys.private_key_path.display().to_string(),
                opts: JwtValidationOpts {
                    iss: "rust-bot".to_string(),
                    aud: String::new(),
                },
            }),
            user_registry: Arc::new(Mutex::new(registry)),
            token_purpose: "webui".to_string(),
        });

        let response = login(State(state), Json(request("a@b.com", "correct horse")))
            .await
            .unwrap();

        let parts: Vec<_> = response.token.split('.').collect();
        assert_eq!(parts.len(), 3, "expected a 3-part JWT");
    }
}
