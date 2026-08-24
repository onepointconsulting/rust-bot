//! Authenticated `GET /v1/media/{*key}` endpoint that serves previously
//! uploaded WebUI attachments back to the browser.
//!
//! Uploaded images are written to disk under
//! [`get_media_dir`](crate::config::paths::get_media_dir) (see
//! `security::attachment_ingress::store_inbound_attachments`) and the file's
//! absolute path is recorded on the transcript (`media_paths`) / session
//! (`_meta.path`). A freshly-sent message still shows its thumbnail from an
//! in-memory `data:` URL the browser already has — this endpoint only
//! matters once that message comes back through `attached.history` (attach,
//! fork, reconnect), which is text-only and has no `data:` payload to show.
//!
//! Two halves live in this module:
//! - [`media_url_from_stored_path`]: a pure(ish) mapping from a stored
//!   absolute file path to a browser-relative `/v1/media/...` URL, used by
//!   `channels::websocket::runtime::resolve_history_media` to rewrite
//!   `attached.history` rows.
//! - [`serve_media`]: the axum handler those URLs resolve to, mounted on
//!   [`WebSocketChannel::router`](super::super::runtime::WebSocketChannel::router)
//!   so it shares origin and JWT config with the WebSocket upgrade route.

use std::path::{Path, PathBuf};

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::channels::websocket::runtime::WEBUI_JWT_PURPOSE;
use crate::channels::websocket::types::WsShared;
use crate::security::jwt::{JwtValidationOpts, validate_jwt_token};
use crate::utils::helpers::detect_image_mime;

/// Convert a stored absolute media file path (a transcript's `media_paths`
/// entry, or a session's recovered `[image: <path>]` placeholder) into a
/// browser-relative `/v1/media/...` URL, or `None` when the file no longer
/// exists or isn't confined to `media_root` (a foreign/legacy path — never
/// put an unconfined path on the wire).
///
/// Requires the file to exist: `canonicalize()` fails otherwise, which
/// doubles as "the upload was since deleted, omit it" — the caller
/// (`resolve_history_media`) drops a `None` rather than surfacing an error,
/// so a missing file degrades to plain text instead of a broken thumbnail.
///
/// Each path segment is percent-encoded since stored filenames can contain
/// characters (spaces, non-ASCII) that aren't valid raw in a URL; encoding
/// byte-by-byte is correct even for multi-byte UTF-8 segments.
pub fn media_url_from_stored_path(path: &str, media_root: &Path) -> Option<String> {
    let stored = Path::new(path);
    if !stored.is_file() {
        return None;
    }
    let canonical_root = media_root.canonicalize().ok()?;
    let canonical_stored = stored.canonicalize().ok()?;
    let rel = canonical_stored.strip_prefix(&canonical_root).ok()?;

    let segments: Vec<String> = rel
        .components()
        .map(|c| percent_encode_segment(&c.as_os_str().to_string_lossy()))
        .collect();
    if segments.is_empty() {
        return None;
    }
    Some(format!("/v1/media/{}", segments.join("/")))
}

/// Percent-encode one path segment for use in a URL, keeping only the
/// RFC 3986 "unreserved" ASCII set raw.
fn percent_encode_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A small subset of `mimetypes.guess_extension`'s table, just enough to
/// classify the image types the WebUI upload allow-list accepts when magic
/// bytes alone (`detect_image_mime`) aren't conclusive (never observed in
/// practice for files this endpoint serves, but kept as a defensive
/// fallback rather than a hard 404 on ambiguous bytes). Deliberately
/// duplicated from `agent::context`'s private, near-identical helper rather
/// than shared — same "narrower reimplementation" precedent as
/// `api::media`'s relationship to `utils::media_decode`.
fn guess_image_mime_from_extension(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Confine a request's `key` (already percent-decoded by axum's `Path`
/// extractor) to `media_root`: canonicalize both sides so `..`, absolute
/// components, and symlink escapes can't reach outside the media directory.
/// Requires the resolved path to be an existing file, which also naturally
/// 404s a missing upload instead of merely rejecting traversal attempts.
fn resolve_media_request_path(media_root: &Path, key: &str) -> Option<PathBuf> {
    if key.is_empty() {
        return None;
    }
    let candidate = media_root.join(key);
    let canonical_root = media_root.canonicalize().ok()?;
    let canonical_candidate = candidate.canonicalize().ok()?;
    if canonical_candidate.is_file() && canonical_candidate.starts_with(&canonical_root) {
        Some(canonical_candidate)
    } else {
        None
    }
}

/// Query params accepted on the media request, mirroring the WebSocket
/// upgrade's own `?token=...` convention (`WsUpgradeQuery`) — needed because
/// a plain `<img src>` cannot set an `Authorization` header.
#[derive(Debug, Deserialize)]
pub(crate) struct MediaQuery {
    #[serde(default)]
    pub token: Option<String>,
}

/// Bearer-or-query-token guard for the media endpoint, mirroring the
/// WebSocket upgrade's own `authorize`
/// (`channels::websocket::runtime::authorize`): when JWT is enabled,
/// requires a valid `purpose=webui` token from either the `Authorization:
/// Bearer` header (fetch/XHR callers) or a `?token=` query param (a plain
/// `<img src>`, which can't set headers). No-op when JWT is disabled — same
/// policy as the WS upgrade path.
fn authorize_media_request(
    shared: &WsShared,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<(), StatusCode> {
    let Some(public_key_pem) = shared.jwt_public_key_pem.as_ref() else {
        return Ok(());
    };
    let header_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")));
    let token = header_token
        .or(query_token)
        .filter(|t| !t.trim().is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let opts = JwtValidationOpts {
        iss: shared.jwt.iss.clone(),
        aud: shared.jwt.aud.clone(),
    };
    let claims = validate_jwt_token(token, public_key_pem.as_slice(), &opts).map_err(|e| {
        log::warn!("media endpoint: rejected request with invalid JWT: {e}");
        StatusCode::UNAUTHORIZED
    })?;
    if claims.purpose.as_deref() == Some(WEBUI_JWT_PURPOSE) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Axum handler for `GET /v1/media/{*key}`: authorize, confine `key` to the
/// media root, and stream the file back with an image `Content-Type` —
/// 404 for anything missing, escaping the media root, or not an image (this
/// endpoint only ever serves images; non-image attachments aren't shown as
/// bubble thumbnails).
pub(crate) async fn serve_media(
    State(shared): State<WsShared>,
    AxumPath(key): AxumPath<String>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(status) = authorize_media_request(&shared, &headers, query.token.as_deref()) {
        return status.into_response();
    }

    let Some(resolved) = resolve_media_request_path(&shared.media_root, &key) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let bytes = match tokio::fs::read(&resolved).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(mime) =
        detect_image_mime(&bytes).or_else(|| guess_image_mime_from_extension(&resolved))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(mime),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, max-age=86400"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::queue::MessageBus;
    use crate::channels::gateway_services::GatewayServices;
    use crate::channels::websocket::registry::ConnectionRegistry;
    use crate::config::schema::{ChannelsConfig, JwtConfig};
    use crate::security::workspace_requests::WorkspaceRequestHandler;
    use crate::session::manager::SessionManager;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::sync::Mutex as AsyncMutex;

    // ── media_url_from_stored_path ───────────────────────────────────────────

    #[test]
    fn media_url_from_stored_path_maps_relative_url() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("abc123.png");
        std::fs::write(&file, b"fake-png").unwrap();

        let url = media_url_from_stored_path(file.to_str().unwrap(), dir.path()).unwrap();
        assert_eq!(url, "/v1/media/websocket/abc123.png");
    }

    #[test]
    fn media_url_from_stored_path_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("websocket").join("gone.png");
        assert!(media_url_from_stored_path(missing.to_str().unwrap(), dir.path()).is_none());
    }

    #[test]
    fn media_url_from_stored_path_none_when_outside_media_root() {
        let media_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("foreign.png");
        std::fs::write(&outside_file, b"fake-png").unwrap();

        assert!(
            media_url_from_stored_path(outside_file.to_str().unwrap(), media_dir.path())
                .is_none()
        );
    }

    #[test]
    fn media_url_from_stored_path_percent_encodes_unsafe_characters() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a file#1.png");
        std::fs::write(&file, b"fake-png").unwrap();

        let url = media_url_from_stored_path(file.to_str().unwrap(), dir.path()).unwrap();
        assert_eq!(url, "/v1/media/a%20file%231.png");
    }

    // ── resolve_media_request_path ───────────────────────────────────────────

    #[test]
    fn resolve_media_request_path_finds_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pic.png"), b"data").unwrap();

        let resolved = resolve_media_request_path(dir.path(), "pic.png").unwrap();
        assert_eq!(std::fs::read(resolved).unwrap(), b"data");
    }

    #[test]
    fn resolve_media_request_path_rejects_traversal() {
        let root = tempfile::tempdir().unwrap();
        let media_dir = root.path().join("media");
        std::fs::create_dir_all(&media_dir).unwrap();
        std::fs::write(root.path().join("secret.txt"), b"nope").unwrap();

        assert!(resolve_media_request_path(&media_dir, "../secret.txt").is_none());
    }

    #[test]
    fn resolve_media_request_path_rejects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_media_request_path(dir.path(), "nope.png").is_none());
    }

    #[test]
    fn resolve_media_request_path_rejects_empty_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_media_request_path(dir.path(), "").is_none());
    }

    // ── authorize_media_request / serve_media ────────────────────────────────

    fn test_shared() -> WsShared {
        let dir = tempfile::tempdir().unwrap();
        WsShared {
            name: "websocket",
            bus: Arc::new(MessageBus::new()),
            channels_config: ChannelsConfig::default(),
            jwt: JwtConfig::default(),
            jwt_public_key_pem: None,
            connections: Arc::new(AsyncMutex::new(ConnectionRegistry::default())),
            supports_streaming: false,
            session_manager: Arc::new(StdMutex::new(SessionManager::new(dir.keep()))),
            workspace_request_handler: WorkspaceRequestHandler::new(
                tempfile::tempdir().unwrap().keep(),
                true,
            ),
            runtime_surface: "browser".to_string(),
            gateway_services: Arc::new(GatewayServices::new(tempfile::tempdir().unwrap().keep())),
            media_root: tempfile::tempdir().unwrap().keep(),
            runtime_resolver: crate::agent::model_runtime::ModelRuntimeResolver::for_tests(),
        }
    }

    fn shared_with_jwt_enabled() -> (WsShared, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let keys = crate::security::jwt::generate_jwt_keypair(dir.keep(), false).unwrap();
        let mut shared = test_shared();
        shared.jwt = JwtConfig {
            enabled: true,
            iss: "rust-bot".to_string(),
            aud: String::new(),
            ..JwtConfig::default()
        };
        shared.jwt_public_key_pem = Some(Arc::new(std::fs::read(&keys.public_key_path).unwrap()));
        (shared, keys.private_key_path)
    }

    fn mint_token_with_purpose(private_key_path: &Path, purpose: Option<&str>) -> String {
        let private_pem = std::fs::read(private_key_path).unwrap();
        let now = chrono::Utc::now().timestamp();
        let claims = crate::security::jwt::Claims {
            iss: "rust-bot".to_string(),
            sub: uuid::Uuid::new_v4().to_string(),
            aud: None,
            exp: now + 3600,
            iat: now,
            purpose: purpose.map(str::to_string),
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        let encoding_key = jsonwebtoken::EncodingKey::from_ed_pem(&private_pem).unwrap();
        jsonwebtoken::encode(&header, &claims, &encoding_key).unwrap()
    }

    #[test]
    fn authorize_media_request_ok_when_jwt_disabled() {
        let shared = test_shared();
        assert!(authorize_media_request(&shared, &HeaderMap::new(), None).is_ok());
    }

    #[test]
    fn authorize_media_request_rejects_missing_token_when_jwt_enabled() {
        let (shared, _key) = shared_with_jwt_enabled();
        assert_eq!(
            authorize_media_request(&shared, &HeaderMap::new(), None),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn authorize_media_request_accepts_query_token() {
        let (shared, key) = shared_with_jwt_enabled();
        let token = mint_token_with_purpose(&key, Some(WEBUI_JWT_PURPOSE));
        assert!(authorize_media_request(&shared, &HeaderMap::new(), Some(&token)).is_ok());
    }

    #[test]
    fn authorize_media_request_accepts_bearer_header() {
        let (shared, key) = shared_with_jwt_enabled();
        let token = mint_token_with_purpose(&key, Some(WEBUI_JWT_PURPOSE));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        assert!(authorize_media_request(&shared, &headers, None).is_ok());
    }

    #[test]
    fn authorize_media_request_rejects_wrong_purpose() {
        let (shared, key) = shared_with_jwt_enabled();
        let token = mint_token_with_purpose(&key, Some("client"));
        assert_eq!(
            authorize_media_request(&shared, &HeaderMap::new(), Some(&token)),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[tokio::test]
    async fn serve_media_returns_image_bytes_when_jwt_disabled() {
        let shared = test_shared();
        let sub = shared.media_root.join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        // Minimal valid PNG magic-byte header, enough for `detect_image_mime`.
        let png_bytes: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-file";
        std::fs::write(sub.join("pic.png"), png_bytes).unwrap();

        let response = serve_media(
            State(shared),
            AxumPath("websocket/pic.png".to_string()),
            Query(MediaQuery { token: None }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
    }

    #[tokio::test]
    async fn serve_media_404_for_traversal_key() {
        let shared = test_shared();
        let response = serve_media(
            State(shared),
            AxumPath("../../etc/passwd".to_string()),
            Query(MediaQuery { token: None }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_media_404_for_non_image_file() {
        let shared = test_shared();
        let sub = shared.media_root.join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("notes.txt"), b"plain text, not an image").unwrap();

        let response = serve_media(
            State(shared),
            AxumPath("websocket/notes.txt".to_string()),
            Query(MediaQuery { token: None }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_media_401_without_token_when_jwt_enabled() {
        let (shared, _key) = shared_with_jwt_enabled();
        let response = serve_media(
            State(shared),
            AxumPath("websocket/pic.png".to_string()),
            Query(MediaQuery { token: None }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
