//! Generic Axum fallback that serves files from a [`rust_embed::RustEmbed`]
//! bundle, with SPA fallback to `index.html` (same shape as `ServeDir` +
//! `ServeFile` used for on-disk web roots).
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use rust_embed::{EmbeddedFile, RustEmbed};

/// Method router suitable as `Router::fallback_service(...)`.
pub fn fallback_router<E: RustEmbed + Send + Sync + 'static>() -> MethodRouter {
    get(serve::<E>)
}

pub(crate) async fn serve<E: RustEmbed>(uri: Uri) -> Response {
    match lookup::<E>(uri.path()) {
        Some(file) => file_response(file),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Resolve `request_path` against the bundle. `/` and unknown paths fall
/// back to `index.html`. Paths containing `..` are rejected (no fallback).
pub(crate) fn lookup<E: RustEmbed>(request_path: &str) -> Option<EmbeddedFile> {
    if request_path.contains("..") {
        return None;
    }
    let trimmed = request_path.trim_start_matches('/');
    let candidate = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    E::get(candidate).or_else(|| E::get("index.html"))
}

fn file_response(file: EmbeddedFile) -> Response {
    let mime = file.metadata.mimetype();
    let content_type = HeaderValue::from_str(mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    (
        [(header::CONTENT_TYPE, content_type)],
        file.data.into_owned(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use rust_embed::RustEmbed;

    #[derive(RustEmbed)]
    #[folder = "src/utils/testdata/embedded_static/"]
    struct Fixture;

    #[test]
    fn lookup_root_is_index() {
        let file = lookup::<Fixture>("/").expect("index.html");
        let body = std::str::from_utf8(&file.data).unwrap();
        assert!(body.contains("fixture-index"));
        assert_eq!(file.metadata.mimetype(), "text/html");
    }

    #[test]
    fn lookup_js_uses_javascript_mime() {
        let file = lookup::<Fixture>("/app.js").expect("app.js");
        let body = std::str::from_utf8(&file.data).unwrap();
        assert!(body.contains("fixture-js"));
        assert!(
            file.metadata.mimetype().contains("javascript"),
            "unexpected mime {}",
            file.metadata.mimetype()
        );
    }

    #[test]
    fn lookup_wasm_is_application_wasm() {
        let file = lookup::<Fixture>("/hello.wasm").expect("hello.wasm");
        assert_eq!(file.metadata.mimetype(), "application/wasm");
    }

    #[test]
    fn lookup_unknown_path_falls_back_to_index() {
        let file = lookup::<Fixture>("/not-a-real-route").expect("SPA fallback");
        let body = std::str::from_utf8(&file.data).unwrap();
        assert!(body.contains("fixture-index"));
    }

    #[test]
    fn lookup_dotdot_is_rejected() {
        assert!(lookup::<Fixture>("/../index.html").is_none());
    }

    #[tokio::test]
    async fn serve_root_returns_html() {
        let response = serve::<Fixture>(Uri::from_static("/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.contains("text/html"));
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("fixture-index")
        );
    }

    #[tokio::test]
    async fn serve_missing_file_is_not_found_when_index_absent_from_dotdot() {
        let response = serve::<Fixture>(Uri::from_static("/../secret")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
