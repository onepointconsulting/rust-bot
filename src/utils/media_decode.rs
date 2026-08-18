//! Shared helper for decoding `data:...;base64,...` URLs to disk. Port of
//! `nanobot/utils/media_decode.py`.
//!
//! nanobot shares this one primitive across its OpenAI-compatible API's
//! `image_url` ingestion, WebUI attachment ingestion, and voice-message
//! audio decode. rust-bot's own `api::media::materialize_data_url` predates
//! this port and is a narrower, image-only reimplementation of the same
//! idea for just the API path — not unified with this module here.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use uuid::Uuid;

use crate::utils::helpers::safe_filename;

pub const DEFAULT_MAX_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_FILE_SIZE: usize = DEFAULT_MAX_BYTES;

/// Mirrors nanobot's `re.compile(..., re.DOTALL)` — `(?s)` makes `.` match
/// newlines too, so a data URL with an embedded newline in its payload still
/// matches in one pass, same as Python. (One narrow divergence, not chased
/// here: Python's `$` can match just before a single trailing `\n`, letting
/// it fall outside the captured group; Rust's `$` without multi-line mode
/// only matches at the absolute end, so a trailing `\n` would be swept into
/// the capture instead. Irrelevant for values coming out of parsed JSON,
/// which is the only real caller shape today.)
static DATA_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^data:([^;,]+)(?:;[^,]*)*;base64,(.+)$").unwrap());

/// A small subset of `mimetypes.guess_extension`'s table — just enough for
/// the MIME types that actually reach this function through nanobot's three
/// call sites (API `image_url`, WebUI attachments, voice-message audio).
/// Deliberately not a general MIME-to-extension database — no new crate
/// dependency for that.
fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        // Mirrors nanobot's `_MIME_EXTENSION_OVERRIDES` verbatim.
        "application/ogg" | "audio/ogg" => ".ogg",
        "audio/mpga" => ".mpga",
        "audio/wav" | "audio/x-wav" | "audio/vnd.wave" => ".wav",
        "audio/webm" | "video/webm" => ".webm",
        "audio/x-m4a" => ".m4a",
        "application/json" => ".json",
        "application/pdf" => ".pdf",
        "application/toml" => ".toml",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => ".pptx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => ".xlsx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => ".docx",
        "application/x-yaml" | "application/yaml" | "text/yaml" => ".yaml",
        "application/xhtml+xml" | "text/html" => ".html",
        "application/xml" | "text/xml" => ".xml",
        "text/csv" => ".csv",
        "text/markdown" => ".md",
        "text/plain" => ".txt",
        // Not in nanobot's override table — Python resolves these via
        // `mimetypes.guess_extension` instead; hardcoded here for the same
        // reason as above (the image/video types the WebUI upload
        // allow-list accepts).
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        "video/mp4" => ".mp4",
        "video/quicktime" => ".mov",
        _ => ".bin",
    }
}

/// Raised when a decoded payload exceeds the caller's size limit. Mirrors
/// nanobot's `FileSizeExceededError`/`FileSizeExceeded`.
#[derive(Debug)]
pub struct FileSizeExceeded {
    pub limit_bytes: usize,
}

impl std::fmt::Display for FileSizeExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "File exceeds {}MB limit",
            self.limit_bytes / (1024 * 1024)
        )
    }
}

impl std::error::Error for FileSizeExceeded {}

/// Everything [`save_base64_data_url`] can fail with once the URL/base64
/// shape itself is already known to be valid (a malformed URL or payload
/// returns `Ok(None)` instead — see that function's docs).
#[derive(Debug)]
pub enum SaveDataUrlError {
    TooLarge(FileSizeExceeded),
    Io(std::io::Error),
}

impl std::fmt::Display for SaveDataUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(e) => e.fmt(f),
            Self::Io(e) => write!(f, "failed to write decoded media: {e}"),
        }
    }
}

impl std::error::Error for SaveDataUrlError {}

/// Decode a `data:<mime>;base64,<payload>` URL and persist it under
/// `media_dir`.
///
/// Returns `Ok(None)` when the URL shape or the base64 payload itself is
/// malformed — mirrors Python's `return None`, a normal outcome callers are
/// expected to branch on, not an error. Returns
/// `Err(SaveDataUrlError::TooLarge(_))` when the decoded payload exceeds
/// `max_bytes` (default 10 MB) — mirrors the raised `FileSizeExceeded`.
/// Returns `Err(SaveDataUrlError::Io(_))` if writing the file fails —
/// mirrors the unhandled `OSError` Python would propagate out of
/// `write_bytes`.
pub fn save_base64_data_url(
    data_url: &str,
    media_dir: &Path,
    max_bytes: Option<usize>,
    filename: Option<&str>,
) -> Result<Option<String>, SaveDataUrlError> {
    let Some(captures) = DATA_URL_RE.captures(data_url) else {
        return Ok(None);
    };
    let mime_type = captures[1].trim().to_lowercase();
    let b64_payload = &captures[2];

    let Ok(raw) = STANDARD.decode(b64_payload.as_bytes()) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }

    let limit = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    if raw.len() > limit {
        return Err(SaveDataUrlError::TooLarge(FileSizeExceeded {
            limit_bytes: limit,
        }));
    }

    let ext = extension_for_mime(&mime_type);
    let base = safe_filename(filename.unwrap_or(""));
    let stem = if base.is_empty() {
        String::new()
    } else {
        // `.chars().take(80)`, not byte slicing: Python's `stem[:80]` slices
        // by codepoint, and `base` is user-controlled (may contain
        // multi-byte UTF-8) — byte-index slicing here could panic or cut
        // mid-character.
        Path::new(&base)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>()
    };

    // Hex chars are single-byte ASCII, so byte-slicing the uuid is safe
    // (unlike `stem` above).
    let uuid_hex = Uuid::new_v4().simple().to_string();
    let short_id = &uuid_hex[..12];
    let saved_name = if stem.is_empty() {
        format!("{short_id}{ext}")
    } else {
        format!("{short_id}_{stem}{ext}")
    };

    // Sanitized again here (on top of `base`'s own sanitization above),
    // matching nanobot's own belt-and-suspenders `safe_filename(saved_name)`
    // right before the write.
    let dest: PathBuf = media_dir.join(safe_filename(&saved_name));
    std::fs::write(&dest, &raw).map_err(SaveDataUrlError::Io)?;
    Ok(Some(dest.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_url(mime: &str, bytes: &[u8]) -> String {
        format!("data:{mime};base64,{}", STANDARD.encode(bytes))
    }

    #[test]
    fn round_trips_a_valid_data_url() {
        let dir = tempfile::tempdir().unwrap();
        let saved = save_base64_data_url(
            &data_url("image/png", b"fake-png-bytes"),
            dir.path(),
            None,
            None,
        )
        .unwrap()
        .expect("valid data URL should save");

        assert!(saved.ends_with(".png"));
        assert_eq!(std::fs::read(&saved).unwrap(), b"fake-png-bytes");
    }

    #[test]
    fn uses_filename_stem_when_provided() {
        let dir = tempfile::tempdir().unwrap();
        let saved = save_base64_data_url(
            &data_url("text/plain", b"hello"),
            dir.path(),
            None,
            Some("notes.txt"),
        )
        .unwrap()
        .unwrap();

        let name = Path::new(&saved).file_name().unwrap().to_str().unwrap();
        assert!(name.contains("notes"), "expected stem in {name}");
        assert!(name.ends_with(".txt"));
    }

    #[test]
    fn truncates_long_multibyte_stem_by_codepoint_not_byte() {
        let dir = tempfile::tempdir().unwrap();
        // Each "你" is 3 UTF-8 bytes; byte-slicing at 80 would panic
        // mid-character or silently corrupt the name. 100 chars > the 80
        // char cap, so this also exercises truncation actually happening.
        let long_name = format!("{}.txt", "你".repeat(100));
        let saved = save_base64_data_url(
            &data_url("text/plain", b"hi"),
            dir.path(),
            None,
            Some(&long_name),
        )
        .unwrap()
        .unwrap();

        let stem = Path::new(&saved).file_stem().unwrap().to_str().unwrap();
        // "<12-hex>_" prefix + up to 80 "你" chars.
        assert_eq!(stem.chars().filter(|&c| c == '你').count(), 80);
    }

    #[test]
    fn malformed_url_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            save_base64_data_url("not a data url", dir.path(), None, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_base64_payload_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            save_base64_data_url(
                "data:text/plain;base64,not-valid-base64!!",
                dir.path(),
                None,
                None
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = save_base64_data_url(
            &data_url("text/plain", &[0u8; 100]),
            dir.path(),
            Some(50),
            None,
        );
        assert!(matches!(result, Err(SaveDataUrlError::TooLarge(_))));
    }

    #[test]
    fn extension_overrides_match_nanobot_table() {
        assert_eq!(extension_for_mime("audio/ogg"), ".ogg");
        assert_eq!(extension_for_mime("application/pdf"), ".pdf");
        assert_eq!(
            extension_for_mime(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            ),
            ".docx"
        );
        assert_eq!(extension_for_mime("image/png"), ".png");
        assert_eq!(extension_for_mime("application/octet-stream"), ".bin");
    }
}
