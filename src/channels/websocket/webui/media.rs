//! Authenticated `GET /v1/media/{*key}` endpoint that serves WebUI media
//! back to the browser — inbound user uploads and outbound `message`-tool
//! attachments alike.
//!
//! Files live on disk under
//! [`get_media_dir`](crate::config::paths::get_media_dir). User uploads are
//! written there by `security::attachment_ingress::store_inbound_attachments`;
//! agent-attached files are copied in by [`confine_outbound_media`]. The
//! absolute path is recorded on the transcript (`media_paths`). A freshly
//! sent *user* message still shows its thumbnail from an in-memory `data:`
//! URL the browser already has — this endpoint matters once that message
//! comes back through `attached.history`, and for every outbound attachment
//! (live `message` event and restored history), which never had a `data:`
//! payload to show.
//!
//! Pieces that live in this module:
//! - [`media_url_from_stored_path`]: a pure(ish) mapping from a stored
//!   absolute file path to a browser-relative `/v1/media/...` URL, used by
//!   `channels::websocket::runtime::resolve_history_media` to rewrite
//!   `attached.history` rows and by outbound `send()` for live `media`.
//! - [`confine_outbound_media`]: copy agent-supplied paths under `media_root`
//!   so the HTTP handler never serves outside it.
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
use uuid::Uuid;

use crate::channels::websocket::runtime::WEBUI_JWT_PURPOSE;
use crate::channels::websocket::types::WsShared;
use crate::security::ingress_policy::AttachmentIngressLimits;
use crate::security::jwt::{JwtValidationOpts, validate_jwt_token};
use crate::utils::helpers::{detect_image_mime, safe_filename};

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

/// Copy agent-attached files into `media_root` so [`serve_media`] can hand
/// them out without ever reading outside the media directory.
///
/// Files already confined to `media_root` are reused in place. Missing,
/// unreadable, or oversized files (above inbound
/// [`AttachmentIngressLimits::DEFAULT.max_file_bytes`]) are logged and
/// dropped rather than failing the whole send.
pub fn confine_outbound_media(paths: &[String], media_root: &Path) -> Vec<String> {
    confine_outbound_media_with_limit(
        paths,
        media_root,
        AttachmentIngressLimits::DEFAULT.max_file_bytes,
    )
}

fn confine_outbound_media_with_limit(
    paths: &[String],
    media_root: &Path,
    max_file_bytes: usize,
) -> Vec<String> {
    if paths.is_empty() {
        return Vec::new();
    }
    let dest_dir = media_root.join("websocket");
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        log::warn!(
            "outbound media: failed to create {}: {e}",
            dest_dir.display()
        );
        return Vec::new();
    }
    let canonical_root = match media_root.canonicalize() {
        Ok(root) => root,
        Err(e) => {
            log::warn!(
                "outbound media: failed to canonicalize {}: {e}",
                media_root.display()
            );
            return Vec::new();
        }
    };

    let mut confined = Vec::new();
    for path in paths {
        match confine_one(path, &dest_dir, &canonical_root, max_file_bytes) {
            Some(saved) => confined.push(saved),
            None => {}
        }
    }
    confined
}

fn confine_one(
    path: &str,
    dest_dir: &Path,
    canonical_root: &Path,
    max_file_bytes: usize,
) -> Option<String> {
    let source = Path::new(path);
    if !source.is_file() {
        log::warn!("outbound media: skipping missing or non-file path {path}");
        return None;
    }
    let size = match std::fs::metadata(source) {
        Ok(meta) => meta.len() as usize,
        Err(e) => {
            log::warn!("outbound media: failed to stat {path}: {e}");
            return None;
        }
    };
    if size > max_file_bytes {
        log::warn!(
            "outbound media: skipping oversized file {path} ({size} bytes, limit {max_file_bytes})"
        );
        return None;
    }
    if let Ok(canonical) = source.canonicalize()
        && canonical.is_file()
        && canonical.starts_with(canonical_root)
    {
        return Some(canonical.to_string_lossy().into_owned());
    }

    let original = source
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("attachment");
    let dest_name = format!("{}_{}", Uuid::new_v4(), safe_filename(original));
    let dest = dest_dir.join(dest_name);
    if let Err(e) = std::fs::copy(source, &dest) {
        log::warn!(
            "outbound media: failed to copy {path} -> {}: {e}",
            dest.display()
        );
        return None;
    }
    Some(
        dest.canonicalize()
            .unwrap_or(dest)
            .to_string_lossy()
            .into_owned(),
    )
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

/// MIME from a file extension when magic bytes aren't conclusive. Covers
/// the image/video/document types the WebUI already accepts inbound, plus
/// the common outbound attachments the `message` tool delivers. Fallback
/// at the call site is `application/octet-stream`.
fn guess_mime_from_extension(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "pdf" => Some("application/pdf"),
        "txt" | "log" => Some("text/plain; charset=utf-8"),
        "md" => Some("text/markdown; charset=utf-8"),
        "csv" => Some("text/csv; charset=utf-8"),
        "html" | "htm" => Some("text/html; charset=utf-8"),
        "json" => Some("application/json"),
        "xml" => Some("application/xml"),
        "toml" => Some("application/toml"),
        "yaml" | "yml" => Some("application/yaml"),
        "zip" => Some("application/zip"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "ogg" => Some("audio/ogg"),
        "m4a" => Some("audio/mp4"),
        "mp4" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mov" => Some("video/quicktime"),
        "doc" => Some("application/msword"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "xls" => Some("application/vnd.ms-excel"),
        "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "ppt" => Some("application/vnd.ms-powerpoint"),
        "pptx" => Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        _ => None,
    }
}

fn mime_for_file(path: &Path, bytes: &[u8]) -> &'static str {
    detect_image_mime(bytes)
        .or_else(|| guess_mime_from_extension(path))
        .unwrap_or("application/octet-stream")
}

fn is_inline_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Prefer the original filename when outbound copies are stored as
/// `{uuid}_{original}`; otherwise the on-disk name.
fn download_filename(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("attachment");
    if let Some((maybe_uuid, rest)) = name.split_once('_')
        && Uuid::parse_str(maybe_uuid).is_ok()
        && !rest.is_empty()
    {
        return rest.to_string();
    }
    name.to_string()
}

fn content_disposition_header(inline: bool, filename: &str) -> header::HeaderValue {
    let kind = if inline { "inline" } else { "attachment" };
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = percent_encode_segment(filename);
    let value = format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}");
    header::HeaderValue::from_str(&value).unwrap_or_else(|_| header::HeaderValue::from_static(kind))
}

fn is_download_query(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1") | Some("true") | Some("yes"))
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
/// a plain `<img src>` cannot set an `Authorization` header. `download=1`
/// forces `Content-Disposition: attachment` so a click-to-save link can
/// download an image that would otherwise be served `inline` for `<img>`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MediaQuery {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub download: Option<String>,
}

/// Bearer-or-query-token guard for the media endpoint, mirroring the
/// WebSocket upgrade's own `authorize`
/// (`channels::websocket::runtime::authorize`): when JWT is enabled,
/// requires a valid `purpose=webui` token from either the `Authorization:
/// Bearer` header (fetch/XHR callers) or a `?token=` query param (a plain
/// `<img src>`, which can't set headers). No-op when JWT is disabled — same
/// policy as the WS upgrade path. When JWT is enabled but
/// `WsShared::require_auth` is `false` (an instance that allows guest use),
/// a missing token is also allowed — but an invalid one still isn't.
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
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        });
    let token = header_token
        .or(query_token)
        .filter(|t| !t.trim().is_empty());
    let Some(token) = token else {
        return if shared.require_auth {
            Err(StatusCode::UNAUTHORIZED)
        } else {
            Ok(())
        };
    };
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
/// media root, and stream the file back. Images are served `inline` so an
/// `<img src>` can render them; non-images, or any file requested with
/// `?download=1`, get `Content-Disposition: attachment`. 404 for anything
/// missing or escaping the media root.
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

    let mime = mime_for_file(&resolved, &bytes);
    let force_download = is_download_query(query.download.as_deref());
    let inline = is_inline_image_mime(mime) && !force_download;
    let filename = download_filename(&resolved);

    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static(mime));
    headers.insert(
        header::CONTENT_DISPOSITION,
        content_disposition_header(inline, &filename),
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
            media_url_from_stored_path(outside_file.to_str().unwrap(), media_dir.path()).is_none()
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

    // ── confine_outbound_media ───────────────────────────────────────────────

    #[test]
    fn confine_outbound_media_reuses_file_already_under_media_root() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("already.png");
        std::fs::write(&file, b"png-bytes").unwrap();

        let confined = confine_outbound_media(&[file.to_str().unwrap().to_string()], dir.path());
        assert_eq!(confined.len(), 1);
        assert_eq!(
            Path::new(&confined[0]).canonicalize().unwrap(),
            file.canonicalize().unwrap()
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"png-bytes");
    }

    #[test]
    fn confine_outbound_media_copies_file_from_outside_media_root() {
        let media_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let source = outside_dir.path().join("report.pdf");
        std::fs::write(&source, b"%PDF-fake").unwrap();

        let confined =
            confine_outbound_media(&[source.to_str().unwrap().to_string()], media_dir.path());
        assert_eq!(confined.len(), 1);
        let dest = Path::new(&confined[0]);
        assert!(
            dest.starts_with(media_dir.path())
                || dest
                    .canonicalize()
                    .unwrap()
                    .starts_with(media_dir.path().canonicalize().unwrap())
        );
        assert_eq!(std::fs::read(dest).unwrap(), b"%PDF-fake");
        assert!(
            std::fs::read(&source).is_ok(),
            "source must be left in place"
        );
        let name = dest.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with("_report.pdf"), "{name}");
        assert_eq!(download_filename(dest), "report.pdf");
    }

    #[test]
    fn confine_outbound_media_skips_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.pdf");
        assert!(
            confine_outbound_media(&[missing.to_string_lossy().into_owned()], dir.path())
                .is_empty()
        );
    }

    #[test]
    fn confine_outbound_media_skips_oversized_file() {
        let media_dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let source = outside_dir.path().join("big.bin");
        std::fs::write(&source, vec![0u8; 32]).unwrap();

        let confined = confine_outbound_media_with_limit(
            &[source.to_str().unwrap().to_string()],
            media_dir.path(),
            16,
        );
        assert!(confined.is_empty());
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
            require_auth: true,
            connections: Arc::new(AsyncMutex::new(ConnectionRegistry::default())),
            supports_streaming: false,
            session_manager: Arc::new(StdMutex::new(
                SessionManager::with_default_eviction_threshold(dir.keep()),
            )),
            workspace_request_handler: WorkspaceRequestHandler::new(
                tempfile::tempdir().unwrap().keep(),
                true,
            ),
            runtime_surface: "browser".to_string(),
            gateway_services: Arc::new(GatewayServices::new(tempfile::tempdir().unwrap().keep())),
            media_root: tempfile::tempdir().unwrap().keep(),
            runtime_resolver: crate::agent::model_runtime::ModelRuntimeResolver::for_tests(),
            default_agent_mode: crate::agent::modes::AgentMode::Standard,
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

    fn shared_with_jwt_enabled_and_optional_auth() -> (WsShared, std::path::PathBuf) {
        let (mut shared, key) = shared_with_jwt_enabled();
        shared.require_auth = false;
        (shared, key)
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

    #[test]
    fn authorize_media_request_allows_missing_token_when_auth_not_required() {
        let (shared, _key) = shared_with_jwt_enabled_and_optional_auth();
        assert!(authorize_media_request(&shared, &HeaderMap::new(), None).is_ok());
    }

    #[test]
    fn authorize_media_request_still_rejects_invalid_token_when_auth_not_required() {
        let (shared, _key) = shared_with_jwt_enabled_and_optional_auth();
        assert_eq!(
            authorize_media_request(&shared, &HeaderMap::new(), Some("not-a-real-token")),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn authorize_media_request_still_accepts_valid_token_when_auth_not_required() {
        let (shared, key) = shared_with_jwt_enabled_and_optional_auth();
        let token = mint_token_with_purpose(&key, Some(WEBUI_JWT_PURPOSE));
        assert!(authorize_media_request(&shared, &HeaderMap::new(), Some(&token)).is_ok());
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
            Query(MediaQuery::default()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(disposition.starts_with("inline;"), "{disposition}");
        assert!(
            disposition.contains("filename=\"pic.png\""),
            "{disposition}"
        );
    }

    #[tokio::test]
    async fn serve_media_404_for_traversal_key() {
        let shared = test_shared();
        let response = serve_media(
            State(shared),
            AxumPath("../../etc/passwd".to_string()),
            Query(MediaQuery::default()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_media_returns_non_image_as_attachment() {
        let shared = test_shared();
        let sub = shared.media_root.join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("notes.txt"), b"plain text, not an image").unwrap();

        let response = serve_media(
            State(shared),
            AxumPath("websocket/notes.txt".to_string()),
            Query(MediaQuery::default()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(disposition.starts_with("attachment;"), "{disposition}");
        assert!(
            disposition.contains("filename=\"notes.txt\""),
            "{disposition}"
        );
    }

    #[tokio::test]
    async fn serve_media_pdf_is_attachment() {
        let shared = test_shared();
        let sub = shared.media_root.join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("report.pdf"), b"%PDF-1.4").unwrap();

        let response = serve_media(
            State(shared),
            AxumPath("websocket/report.pdf".to_string()),
            Query(MediaQuery::default()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/pdf"
        );
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(disposition.starts_with("attachment;"), "{disposition}");
    }

    #[tokio::test]
    async fn serve_media_download_query_forces_attachment_on_image() {
        let shared = test_shared();
        let sub = shared.media_root.join("websocket");
        std::fs::create_dir_all(&sub).unwrap();
        let png_bytes: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-file";
        std::fs::write(sub.join("pic.png"), png_bytes).unwrap();

        let response = serve_media(
            State(shared),
            AxumPath("websocket/pic.png".to_string()),
            Query(MediaQuery {
                token: None,
                download: Some("1".to_string()),
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(disposition.starts_with("attachment;"), "{disposition}");
    }

    #[tokio::test]
    async fn serve_media_401_without_token_when_jwt_enabled() {
        let (shared, _key) = shared_with_jwt_enabled();
        let response = serve_media(
            State(shared),
            AxumPath("websocket/pic.png".to_string()),
            Query(MediaQuery::default()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
