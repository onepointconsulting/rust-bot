//! Helpers for rendering chat attachments: image vs file, filename from a
//! media URL, and the `?download=1` query the gateway's `/v1/media` endpoint
//! uses to force `Content-Disposition: attachment`.

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// True when `url` should render as a thumbnail (`<img>`) rather than a
/// download chip: a `data:image/...` URL, or a path whose extension is a
/// displayable raster type. Query strings (JWT `?token=`) are ignored.
pub fn is_image_attachment_url(url: &str) -> bool {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    if without_query.starts_with("data:image/") {
        return true;
    }
    extension_of(without_query).is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// Last path segment of `url`, percent-decoded, with query/hash stripped.
/// `None` when the URL has no usable filename.
pub fn filename_from_media_url(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let segment = without_query.rsplit('/').next().unwrap_or("");
    if segment.is_empty() {
        return None;
    }
    let decoded = percent_decode(segment);
    let name = strip_uuid_prefix(&decoded);
    (!name.is_empty()).then_some(name)
}

/// Append `download=1` to `url` without disturbing an existing query
/// (typically `?token=...` on a gateway media URL).
pub fn with_download_query(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    if url.contains("download=") {
        return url.to_string();
    }
    if url.contains('?') {
        format!("{url}&download=1")
    } else {
        format!("{url}?download=1")
    }
}

fn extension_of(path: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.')?.1;
    if ext.is_empty() || ext.contains('/') {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

fn strip_uuid_prefix(name: &str) -> String {
    if let Some((maybe_uuid, rest)) = name.split_once('_') {
        if is_uuid(maybe_uuid) && !rest.is_empty() {
            return rest.to_string();
        }
    }
    name.to_string()
}

fn is_uuid(value: &str) -> bool {
    let mut parts = value.split('-');
    matches!(
        (
            parts.next().map(|p| p.len() == 8 && is_hex(p)),
            parts.next().map(|p| p.len() == 4 && is_hex(p)),
            parts.next().map(|p| p.len() == 4 && is_hex(p)),
            parts.next().map(|p| p.len() == 4 && is_hex(p)),
            parts.next().map(|p| p.len() == 12 && is_hex(p)),
            parts.next(),
        ),
        (
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            None
        )
    )
}

fn is_hex(value: &str) -> bool {
    value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_image_attachment_url_detects_rasters_and_data_urls() {
        assert!(is_image_attachment_url("/v1/media/websocket/pic.png"));
        assert!(is_image_attachment_url(
            "/v1/media/websocket/pic.PNG?token=abc"
        ));
        assert!(is_image_attachment_url("data:image/png;base64,AAAA"));
        assert!(!is_image_attachment_url("/v1/media/websocket/report.pdf"));
        assert!(!is_image_attachment_url(
            "/v1/media/websocket/notes.txt?token=abc"
        ));
        assert!(!is_image_attachment_url("data:application/pdf;base64,AAAA"));
    }

    #[test]
    fn filename_from_media_url_strips_query_and_uuid_prefix() {
        assert_eq!(
            filename_from_media_url("/v1/media/websocket/report.pdf"),
            Some("report.pdf".to_string())
        );
        assert_eq!(
            filename_from_media_url(
                "/v1/media/websocket/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee_report.pdf?token=x"
            ),
            Some("report.pdf".to_string())
        );
        assert_eq!(
            filename_from_media_url("/v1/media/a%20file%231.png"),
            Some("a file#1.png".to_string())
        );
        assert_eq!(filename_from_media_url("/v1/media/"), None);
    }

    #[test]
    fn with_download_query_appends_without_clobbering_token() {
        assert_eq!(
            with_download_query("/v1/media/websocket/a.png"),
            "/v1/media/websocket/a.png?download=1"
        );
        assert_eq!(
            with_download_query("/v1/media/websocket/a.png?token=tok%20en"),
            "/v1/media/websocket/a.png?token=tok%20en&download=1"
        );
        assert_eq!(
            with_download_query("/v1/media/websocket/a.png?token=x&download=1"),
            "/v1/media/websocket/a.png?token=x&download=1"
        );
    }
}
