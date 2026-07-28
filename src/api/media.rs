//! Materializes `image_url` references from chat completion requests into
//! local files so they can be passed through the existing media pipeline
//! (`InboundMessage.media` → [`crate::agent::context`] base64 inlining).

use std::sync::LazyLock;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use uuid::Uuid;

use crate::config::paths::get_media_dir;
use crate::security::network::{validate_resolved_url, validate_url_target};
use crate::utils::helpers::detect_image_mime;

use super::rest::ApiError;

pub(crate) const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const DOWNLOAD_TIMEOUT_SECS: u64 = 20;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Verify the decoded bytes are a supported image and within the size limit.
fn validate_image_bytes(raw: &[u8]) -> Result<&'static str, ApiError> {
    if raw.len() > MAX_IMAGE_BYTES {
        return Err(ApiError::bad_request(format!(
            "Image exceeds the maximum allowed size of {MAX_IMAGE_BYTES} bytes"
        )));
    }
    detect_image_mime(raw)
        .ok_or_else(|| ApiError::bad_request("image_url does not reference a supported image type (png, jpeg, gif, webp)"))
}

/// Write validated image bytes to the API media directory and return the path.
fn write_image_file(raw: &[u8], mime: &str) -> Result<String, ApiError> {
    let dir = get_media_dir(Some("api"));
    let filename = format!("{}.{}", Uuid::new_v4(), extension_for_mime(mime));
    let path = dir.join(filename);
    std::fs::write(&path, raw)
        .map_err(|e| ApiError::bad_request(format!("Failed to store uploaded image: {e}")))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Decode a `data:<mime>;base64,<payload>` URL into an image file.
fn materialize_data_url(url: &str) -> Result<String, ApiError> {
    let rest = url
        .strip_prefix("data:")
        .ok_or_else(|| ApiError::bad_request("Malformed data URL"))?;
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| ApiError::bad_request("Malformed data URL: missing ','"))?;
    if !meta.ends_with(";base64") {
        return Err(ApiError::bad_request(
            "Only base64-encoded data URLs are supported for image_url",
        ));
    }

    let raw = BASE64
        .decode(payload.as_bytes())
        .map_err(|e| ApiError::bad_request(format!("Invalid base64 in data URL: {e}")))?;

    let mime = validate_image_bytes(&raw)?;
    write_image_file(&raw, mime)
}

/// Download an `http(s)://` image URL, validating against SSRF targets, and
/// write it to an image file.
async fn materialize_http_url(url: &str) -> Result<String, ApiError> {
    let (ok, reason) = validate_url_target(url).await;
    if !ok {
        return Err(ApiError::bad_request(format!(
            "Refusing to fetch image_url '{url}': {reason}"
        )));
    }

    let response = HTTP_CLIENT
        .get(url)
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("Failed to fetch image_url '{url}': {e}")))?;

    if !response.status().is_success() {
        return Err(ApiError::bad_request(format!(
            "Failed to fetch image_url '{url}': HTTP {}",
            response.status()
        )));
    }

    let (ok, reason) = validate_resolved_url(response.url().as_str()).await;
    if !ok {
        return Err(ApiError::bad_request(format!(
            "Refusing to fetch image_url '{url}': {reason}"
        )));
    }

    if let Some(len) = response.content_length() {
        if len as usize > MAX_IMAGE_BYTES {
            return Err(ApiError::bad_request(format!(
                "Image at '{url}' exceeds the maximum allowed size of {MAX_IMAGE_BYTES} bytes"
            )));
        }
    }

    let raw = response
        .bytes()
        .await
        .map_err(|e| ApiError::bad_request(format!("Failed to read image_url '{url}': {e}")))?;

    let mime = validate_image_bytes(&raw)?;
    write_image_file(&raw, mime)
}

/// Materialize a single `image_url` reference (`data:` or `http(s)://`) into
/// a local file, returning its path.
async fn materialize_one(url: &str) -> Result<String, ApiError> {
    if url.starts_with("data:") {
        materialize_data_url(url)
    } else if url.starts_with("http://") || url.starts_with("https://") {
        materialize_http_url(url).await
    } else {
        Err(ApiError::bad_request(format!(
            "Unsupported image_url scheme: '{url}'"
        )))
    }
}

/// Materialize all `image_url` references into local files under the API
/// media directory. Fails the whole batch if any image cannot be
/// materialized, rather than silently dropping attachments.
pub(crate) async fn materialize_image_urls(urls: &[String]) -> Result<Vec<String>, ApiError> {
    let mut paths = Vec::with_capacity(urls.len());
    for url in urls {
        paths.push(materialize_one(url).await?);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[tokio::test]
    async fn data_url_round_trip_writes_readable_png() {
        let b64 = BASE64.encode(PNG_1X1);
        let url = format!("data:image/png;base64,{b64}");

        let paths = materialize_image_urls(&[url]).await.expect("materialize");
        assert_eq!(paths.len(), 1);

        let bytes = std::fs::read(&paths[0]).expect("read written file");
        assert_eq!(bytes, PNG_1X1);
        assert!(paths[0].ends_with(".png"));

        let _ = std::fs::remove_file(&paths[0]);
    }

    #[tokio::test]
    async fn data_url_oversized_rejected() {
        let big = vec![0u8; MAX_IMAGE_BYTES + 1024];
        let b64 = BASE64.encode(&big);
        let url = format!("data:image/png;base64,{b64}");

        let err = materialize_image_urls(&[url]).await.unwrap_err();
        assert!(err.message().contains("exceeds the maximum allowed size"), "{}", err.message());
    }

    #[tokio::test]
    async fn data_url_bad_mime_rejected() {
        let b64 = BASE64.encode(b"hello world, not an image");
        let url = format!("data:text/plain;base64,{b64}");

        let err = materialize_image_urls(&[url]).await.unwrap_err();
        assert!(err.message().contains("supported image type"), "{}", err.message());
    }

    #[tokio::test]
    async fn data_url_non_base64_rejected() {
        let url = "data:image/png,not-base64-encoded".to_string();
        let err = materialize_image_urls(&[url]).await.unwrap_err();
        assert!(err.message().contains("base64"), "{}", err.message());
    }

    #[tokio::test]
    async fn http_url_private_target_rejected() {
        let url = "http://127.0.0.1/image.png".to_string();
        let err = materialize_image_urls(&[url]).await.unwrap_err();
        assert!(err.message().contains("Refusing to fetch"), "{}", err.message());
    }

    #[tokio::test]
    async fn unsupported_scheme_rejected() {
        let url = "ftp://example.com/a.png".to_string();
        let err = materialize_image_urls(&[url]).await.unwrap_err();
        assert!(err.message().contains("Unsupported image_url scheme"), "{}", err.message());
    }
}
