//! Validation and persistence for inbound WebUI message attachments. Port of
//! `nanobot/webui/attachment_ingress.py`.

use std::path::Path;

use serde_json::Value;

use crate::security::ingress_policy::AttachmentIngressLimits;
use crate::utils::media_decode::{save_base64_data_url, SaveDataUrlError};

/// Rejection code returned by [`store_inbound_attachments`]. Mirrors
/// nanobot's `AttachmentRejection` (`Literal[...]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentRejection {
    Malformed,
    TooManyImages,
    TooManyVideos,
    TooManyAttachments,
    TotalSize,
    Mime,
    Size,
    Decode,
}

impl AttachmentRejection {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::TooManyImages => "too_many_images",
            Self::TooManyVideos => "too_many_videos",
            Self::TooManyAttachments => "too_many_attachments",
            Self::TotalSize => "total_size",
            Self::Mime => "mime",
            Self::Size => "size",
            Self::Decode => "decode",
        }
    }
}

/// Mirrors nanobot's `AttachmentIngressResult = tuple[list[str], AttachmentRejection | None]`.
/// Collapsed to a `Result` since Python's failure case always pairs an empty
/// list with the rejection reason — no information is lost.
pub type AttachmentIngressResult = Result<Vec<String>, AttachmentRejection>;

const MAX_VIDEOS_PER_MESSAGE: usize = 1;
const MAX_VIDEO_BYTES: usize = 20 * 1024 * 1024;

const IMAGE_MIME_ALLOWED: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];
const VIDEO_MIME_ALLOWED: &[&str] = &["video/mp4", "video/webm", "video/quicktime"];
const DOCUMENT_MIME_ALLOWED: &[&str] = &[
    "application/json",
    "application/pdf",
    "application/toml",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/x-yaml",
    "application/xhtml+xml",
    "application/xml",
    "application/yaml",
    "text/csv",
    "text/html",
    "text/markdown",
    "text/plain",
    "text/xml",
    "text/yaml",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MimeCategory {
    Image,
    Video,
    Document,
}

/// Mirrors the `mime in _VIDEO_MIME_ALLOWED` / `elif ... _IMAGE_MIME_ALLOWED`
/// / `elif ... _DOCUMENT_MIME_ALLOWED` chain — `None` here is exactly
/// equivalent to nanobot's separate `mime not in _UPLOAD_MIME_ALLOWED` check,
/// since `_UPLOAD_MIME_ALLOWED` is just the union of these three (disjoint)
/// sets.
fn classify_mime(mime: &str) -> Option<MimeCategory> {
    if VIDEO_MIME_ALLOWED.contains(&mime) {
        Some(MimeCategory::Video)
    } else if IMAGE_MIME_ALLOWED.contains(&mime) {
        Some(MimeCategory::Image)
    } else if DOCUMENT_MIME_ALLOWED.contains(&mime) {
        Some(MimeCategory::Document)
    } else {
        None
    }
}

static DATA_URL_MIME_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?s)^data:([^;,]+)(?:;[^,]*)*;base64,").unwrap());

/// Return the normalized MIME from a base64 data URL, else `None`.
///
/// Mirrors `extract_data_url_mime`'s `match.group(1).strip().lower() or
/// None` — note the `or None`: an empty string after stripping (e.g.
/// `"data: ;base64,..."`) is normalized to `None`, not `Some("")`.
fn extract_data_url_mime(url: &str) -> Option<String> {
    let mime = DATA_URL_MIME_RE
        .captures(url)?
        .get(1)?
        .as_str()
        .trim()
        .to_lowercase();
    (!mime.is_empty()).then_some(mime)
}

/// Remove any files already written before this batch was found invalid,
/// then return the rejection. Mirrors nanobot's nested `abort(reason)`.
fn abort(paths: &[String], reason: AttachmentRejection) -> AttachmentIngressResult {
    for path in paths {
        if let Err(e) = std::fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("failed to unlink partial media '{path}': {e}");
        }
    }
    Err(reason)
}

/// Extract the `data_url` string field from one media item, if present.
fn attachment_data_url(item: &Value) -> Option<&str> {
    item.as_object()?.get("data_url")?.as_str()
}

/// Validate and atomically persist one WebUI message's attachments.
///
/// The caller owns transport-level error mapping. This function owns the
/// WebUI upload policy and removes files already written when a later item
/// makes the batch invalid.
pub fn store_inbound_attachments(
    media: &[Value],
    media_dir: &Path,
    limits: AttachmentIngressLimits,
) -> AttachmentIngressResult {
    let mut image_count = 0usize;
    let mut video_count = 0usize;
    let mut document_count = 0usize;
    for item in media {
        let category = attachment_data_url(item)
            .and_then(extract_data_url_mime)
            .as_deref()
            .and_then(classify_mime);
        match category {
            Some(MimeCategory::Video) => video_count += 1,
            Some(MimeCategory::Image) => image_count += 1,
            Some(MimeCategory::Document) => document_count += 1,
            None => {}
        }
    }
    if image_count > limits.max_count {
        return Err(AttachmentRejection::TooManyImages);
    }
    if video_count > MAX_VIDEOS_PER_MESSAGE {
        return Err(AttachmentRejection::TooManyVideos);
    }
    if image_count + document_count > limits.max_count {
        return Err(AttachmentRejection::TooManyAttachments);
    }

    let mut paths: Vec<String> = Vec::new();
    let mut total_attachment_bytes: usize = 0;

    for item in media {
        let Some(attachment) = item.as_object() else {
            return abort(&paths, AttachmentRejection::Malformed);
        };
        let Some(data_url) = attachment.get("data_url").and_then(Value::as_str).filter(|s| !s.is_empty())
        else {
            return abort(&paths, AttachmentRejection::Malformed);
        };
        let Some(mime) = extract_data_url_mime(data_url) else {
            return abort(&paths, AttachmentRejection::Decode);
        };
        let Some(category) = classify_mime(&mime) else {
            return abort(&paths, AttachmentRejection::Mime);
        };
        let is_video = category == MimeCategory::Video;
        let is_document = category == MimeCategory::Document;
        let max_bytes = if is_video { MAX_VIDEO_BYTES } else { limits.max_file_bytes };
        let name = if is_document {
            attachment.get("name").and_then(Value::as_str)
        } else {
            None
        };

        let saved = match save_base64_data_url(data_url, media_dir, Some(max_bytes), name) {
            Ok(Some(path)) => path,
            Ok(None) => return abort(&paths, AttachmentRejection::Decode),
            Err(SaveDataUrlError::TooLarge(_)) => return abort(&paths, AttachmentRejection::Size),
            Err(SaveDataUrlError::Io(e)) => {
                log::warn!("media decode failed: {e}");
                return abort(&paths, AttachmentRejection::Decode);
            }
        };
        paths.push(saved);

        if !is_video {
            let saved_path = paths.last().expect("just pushed");
            let size = match std::fs::metadata(saved_path) {
                Ok(meta) => meta.len() as usize,
                Err(e) => {
                    log::warn!("failed to stat inbound attachment '{saved_path}': {e}");
                    return abort(&paths, AttachmentRejection::Decode);
                }
            };
            total_attachment_bytes += size;
            if total_attachment_bytes > limits.max_total_bytes {
                return abort(&paths, AttachmentRejection::TotalSize);
            }
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    fn data_url(mime: &str, bytes: &[u8]) -> Value {
        serde_json::json!({
            "data_url": format!("data:{mime};base64,{}", STANDARD.encode(bytes)),
        })
    }

    fn limits() -> AttachmentIngressLimits {
        AttachmentIngressLimits::DEFAULT
    }

    #[test]
    fn extract_data_url_mime_normalizes_case_and_whitespace() {
        assert_eq!(
            extract_data_url_mime("data: IMAGE/PNG ;base64,AAAA"),
            Some("image/png".to_string())
        );
        assert_eq!(extract_data_url_mime("not a data url"), None);
        assert_eq!(extract_data_url_mime("data: ;base64,AAAA"), None);
    }

    #[test]
    fn empty_media_list_succeeds_with_no_paths() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(store_inbound_attachments(&[], dir.path(), limits()), Ok(vec![]));
    }

    #[test]
    fn stores_a_valid_image() {
        let dir = tempfile::tempdir().unwrap();
        let media = vec![data_url("image/png", b"fake-png")];
        let result = store_inbound_attachments(&media, dir.path(), limits()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(std::fs::read(&result[0]).unwrap(), b"fake-png");
    }

    #[test]
    fn non_object_item_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let media = vec![serde_json::json!("not-an-object")];
        assert_eq!(
            store_inbound_attachments(&media, dir.path(), limits()),
            Err(AttachmentRejection::Malformed)
        );
    }

    #[test]
    fn missing_data_url_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let media = vec![serde_json::json!({"name": "foo.txt"})];
        assert_eq!(
            store_inbound_attachments(&media, dir.path(), limits()),
            Err(AttachmentRejection::Malformed)
        );
    }

    #[test]
    fn undecodable_data_url_is_decode_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let media = vec![serde_json::json!({"data_url": "data:not-a-valid-prefix"})];
        assert_eq!(
            store_inbound_attachments(&media, dir.path(), limits()),
            Err(AttachmentRejection::Decode)
        );
    }

    #[test]
    fn disallowed_mime_is_mime_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let media = vec![data_url("application/x-executable", b"nope")];
        assert_eq!(
            store_inbound_attachments(&media, dir.path(), limits()),
            Err(AttachmentRejection::Mime)
        );
    }

    #[test]
    fn too_many_images_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let media: Vec<Value> = (0..limits().max_count + 1)
            .map(|_| data_url("image/png", b"x"))
            .collect();
        assert_eq!(
            store_inbound_attachments(&media, dir.path(), limits()),
            Err(AttachmentRejection::TooManyImages)
        );
    }

    #[test]
    fn second_video_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let media = vec![data_url("video/mp4", b"a"), data_url("video/mp4", b"b")];
        assert_eq!(
            store_inbound_attachments(&media, dir.path(), limits()),
            Err(AttachmentRejection::TooManyVideos)
        );
    }

    #[test]
    fn oversized_file_is_rejected_and_cleans_up_earlier_saved_files() {
        let dir = tempfile::tempdir().unwrap();
        let small_limits = AttachmentIngressLimits {
            max_file_bytes: 10,
            ..AttachmentIngressLimits::DEFAULT
        };
        let media = vec![
            data_url("image/png", b"ok"),
            data_url("image/png", &[0u8; 100]),
        ];
        let result = store_inbound_attachments(&media, dir.path(), small_limits);
        assert_eq!(result, Err(AttachmentRejection::Size));

        // The first (valid) attachment must have been unlinked, not left behind.
        let remaining: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(remaining.is_empty(), "expected cleanup, found {remaining:?}");
    }

    #[test]
    fn total_size_across_attachments_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let tight_limits = AttachmentIngressLimits {
            max_total_bytes: 15,
            ..AttachmentIngressLimits::DEFAULT
        };
        let media = vec![
            data_url("text/plain", b"0123456789"),
            data_url("text/plain", b"0123456789"),
        ];
        assert_eq!(
            store_inbound_attachments(&media, dir.path(), tight_limits),
            Err(AttachmentRejection::TotalSize)
        );
    }

    #[test]
    fn document_name_is_preserved_but_image_name_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut doc = data_url("text/plain", b"hi");
        doc["name"] = serde_json::json!("report.txt");
        let saved = store_inbound_attachments(&[doc], dir.path(), limits()).unwrap();
        let name = Path::new(&saved[0]).file_name().unwrap().to_str().unwrap();
        assert!(name.contains("report"), "expected stem in {name}");

        let mut img = data_url("image/png", b"hi");
        img["name"] = serde_json::json!("should-be-ignored.png");
        let saved = store_inbound_attachments(&[img], dir.path(), limits()).unwrap();
        let name = Path::new(&saved[0]).file_name().unwrap().to_str().unwrap();
        assert!(!name.contains("should-be-ignored"), "unexpected stem in {name}");
    }
}
