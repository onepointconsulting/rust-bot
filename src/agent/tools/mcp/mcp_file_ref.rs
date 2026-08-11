//! File-reference expansion for MCP tool arguments.
//!
//! # Why this exists
//!
//! MCP tool arguments are produced by the model, so every byte of a binary
//! payload has to be emitted as tokens. For an image that is ruinous: a JSON
//! array of decimal bytes costs roughly 4 tokens per byte, so `maxTokens:
//! 16384` caps uploads at a handful of kilobytes. Base64 is denser but still
//! bounded by the same window, and the model cannot read local files as bytes
//! anyway.
//!
//! The fix is a sentinel. Instead of inlining the payload, the model writes a
//! reference to a local file and the harness substitutes the real content just
//! before the request goes out. The bytes never enter the token stream, so
//! upload size is limited only by the server.
//!
//! # Accepted forms
//!
//! Both a shorthand string and an explicit object are supported:
//!
//! ```json
//! { "content": "file://C:/images/banner.jpg" }
//! { "content": {"$file": "C:/images/banner.jpg", "encoding": "base64"} }
//! ```
//!
//! `encoding` selects the wire format and defaults to `base64`:
//!
//! * `base64`   → a base64 string (compact; what most servers want for `byte[]`)
//! * `bytes`    → a JSON array of numbers, for servers that insist on arrays
//! * `utf8`     → the file decoded as text, for text parameters
//!
//! Expansion is recursive, so a sentinel is picked up wherever it appears —
//! top-level argument, nested object, or inside an array.
//!
//! # Safety
//!
//! Paths are resolved through [`FsToolConfig`], the same sandbox the filesystem
//! tools use. When `restrictToWorkspace` is enabled, a sentinel pointing
//! outside the workspace is rejected rather than silently read, so this feature
//! cannot be used to exfiltrate arbitrary files through an MCP server. A size
//! ceiling guards against accidentally streaming a huge file into a request.

use base64::Engine;
use serde_json::{Map, Value};
use std::path::PathBuf;

use crate::agent::tools::filesystem::{FsToolConfig, ResolvePathError};

/// Key marking an explicit file-reference object.
const FILE_REF_KEY: &str = "$file";
/// Prefix marking the shorthand string form.
const FILE_URI_PREFIX: &str = "file://";

/// Default cap on an expanded file: 32 MiB.
///
/// Generous enough for images and documents, low enough that a mistaken
/// reference to something enormous fails fast instead of exhausting memory.
pub const DEFAULT_MAX_FILE_REF_BYTES: u64 = 32 * 1024 * 1024;

/// How the file content is encoded into the outgoing JSON argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRefEncoding {
    /// Base64 string. The default.
    Base64,
    /// JSON array of byte values.
    Bytes,
    /// UTF-8 text (lossy for invalid sequences).
    Utf8,
}

impl FileRefEncoding {
    fn parse(raw: &str) -> Result<Self, FileRefError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "base64" | "b64" => Ok(Self::Base64),
            "bytes" | "byte_array" | "array" => Ok(Self::Bytes),
            "utf8" | "utf-8" | "text" => Ok(Self::Utf8),
            other => Err(FileRefError::UnknownEncoding(other.to_string())),
        }
    }
}

/// Why a file reference could not be expanded.
#[derive(Debug)]
pub enum FileRefError {
    /// Path rejected by the workspace sandbox.
    Denied { path: String, reason: String },
    /// File missing or unreadable.
    Unreadable { path: PathBuf, reason: String },
    /// File exceeds the configured ceiling.
    TooLarge { path: PathBuf, size: u64, limit: u64 },
    /// `encoding` was not one of the supported values.
    UnknownEncoding(String),
    /// `$file` present but not a string.
    InvalidPathType,
}

impl std::fmt::Display for FileRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied { path, reason } => write!(
                f,
                "file reference '{path}' is outside the allowed directory ({reason}). \
                 Copy the file into the workspace or disable restrictToWorkspace."
            ),
            Self::Unreadable { path, reason } => {
                write!(f, "cannot read file reference '{}': {reason}", path.display())
            }
            Self::TooLarge { path, size, limit } => write!(
                f,
                "file reference '{}' is {size} bytes, over the {limit} byte limit",
                path.display()
            ),
            Self::UnknownEncoding(enc) => write!(
                f,
                "unknown file reference encoding '{enc}' (expected base64, bytes or utf8)"
            ),
            Self::InvalidPathType => {
                write!(f, "'{FILE_REF_KEY}' must be a string path")
            }
        }
    }
}

impl std::error::Error for FileRefError {}

/// Sandbox and limits applied when expanding file references.
#[derive(Debug, Clone)]
pub struct FileRefResolver {
    fs: FsToolConfig,
    max_bytes: u64,
}

impl FileRefResolver {
    pub fn new(fs: FsToolConfig, max_bytes: u64) -> Self {
        Self { fs, max_bytes }
    }

    /// Resolver honouring the workspace sandbox, with the default size cap.
    pub fn with_scope(
        workspace: Option<PathBuf>,
        allowed_dir: Option<PathBuf>,
        extra_allowed_dirs: Vec<PathBuf>,
    ) -> Self {
        Self::new(
            FsToolConfig::new(workspace, allowed_dir, Some(extra_allowed_dirs)),
            DEFAULT_MAX_FILE_REF_BYTES,
        )
    }

    /// Walk `params` and replace every file reference with its content.
    ///
    /// Returns the rewritten arguments plus a human-readable note for each
    /// expansion, so the caller can log what was substituted. Values that are
    /// not sentinels are passed through untouched.
    pub fn expand(&self, params: &Value) -> Result<(Value, Vec<String>), FileRefError> {
        let mut notes = Vec::new();
        let expanded = self.expand_value(params, &mut notes)?;
        Ok((expanded, notes))
    }

    fn expand_value(
        &self,
        value: &Value,
        notes: &mut Vec<String>,
    ) -> Result<Value, FileRefError> {
        match value {
            Value::String(text) => {
                if let Some(path) = text.strip_prefix(FILE_URI_PREFIX) {
                    return self.load(path, FileRefEncoding::Base64, notes);
                }
                // A parameter whose advertised schema is a string (Java
                // `byte[]` is rendered as array-of-string, for one) makes the
                // object form arrive flattened into its own JSON text. Recover
                // it here instead of forwarding a literal `{"$file": ...}`
                // string to the server, which can only reject it.
                if let Some(map) = parse_embedded_ref(text) {
                    return self.load_ref_object(&map, notes);
                }
                Ok(value.clone())
            }
            Value::Object(map) => {
                if map.contains_key(FILE_REF_KEY) {
                    return self.load_ref_object(map, notes);
                }
                let mut out = Map::with_capacity(map.len());
                for (key, item) in map {
                    out.insert(key.clone(), self.expand_value(item, notes)?);
                }
                Ok(Value::Object(out))
            }
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.expand_value(item, notes)?);
                }
                Ok(Value::Array(out))
            }
            _ => Ok(value.clone()),
        }
    }

    /// Expand an explicit `{"$file": ..., "encoding": ...}` reference.
    ///
    /// Shared by the real object form and the string-flattened form so both
    /// honour `encoding` identically.
    fn load_ref_object(
        &self,
        map: &Map<String, Value>,
        notes: &mut Vec<String>,
    ) -> Result<Value, FileRefError> {
        let path = map
            .get(FILE_REF_KEY)
            .and_then(Value::as_str)
            .ok_or(FileRefError::InvalidPathType)?;
        let encoding = match map.get("encoding") {
            Some(Value::String(enc)) => FileRefEncoding::parse(enc)?,
            Some(Value::Null) | None => FileRefEncoding::Base64,
            Some(other) => return Err(FileRefError::UnknownEncoding(other.to_string())),
        };
        self.load(path, encoding, notes)
    }

    /// Read one referenced file and encode it for the wire.
    fn load(
        &self,
        raw_path: &str,
        encoding: FileRefEncoding,
        notes: &mut Vec<String>,
    ) -> Result<Value, FileRefError> {
        // Strip the leftover slash of `file:///C:/...` style URIs on Windows,
        // where the absolute path already starts with a drive letter.
        let cleaned = normalize_uri_path(raw_path);

        let path = self.fs.resolve(&cleaned).map_err(|e| FileRefError::Denied {
            path: cleaned.clone(),
            reason: describe_resolve_error(&e),
        })?;

        let metadata = std::fs::metadata(&path).map_err(|e| FileRefError::Unreadable {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        if metadata.is_dir() {
            return Err(FileRefError::Unreadable {
                path: path.clone(),
                reason: "path is a directory".to_string(),
            });
        }
        let size = metadata.len();
        if size > self.max_bytes {
            return Err(FileRefError::TooLarge {
                path: path.clone(),
                size,
                limit: self.max_bytes,
            });
        }

        let bytes = std::fs::read(&path).map_err(|e| FileRefError::Unreadable {
            path: path.clone(),
            reason: e.to_string(),
        })?;

        let (value, described) = match encoding {
            FileRefEncoding::Base64 => (
                Value::String(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                "base64",
            ),
            FileRefEncoding::Bytes => (
                Value::Array(bytes.iter().map(|b| Value::from(*b)).collect()),
                "byte array",
            ),
            FileRefEncoding::Utf8 => (
                Value::String(String::from_utf8_lossy(&bytes).into_owned()),
                "utf8 text",
            ),
        };

        notes.push(format!(
            "{} ({} bytes) as {described}",
            path.display(),
            bytes.len()
        ));
        Ok(value)
    }
}

/// Render a sandbox rejection in plain language.
///
/// `ResolvePathError` has no `Display` impl (the filesystem tools print it with
/// `{:?}`), so keep the wording here rather than leaking Rust debug syntax into
/// a message the model has to act on.
fn describe_resolve_error(err: &ResolvePathError) -> String {
    match err {
        ResolvePathError::HomeDirUnavailable => "home directory unavailable".to_string(),
        ResolvePathError::NotUnderAllowedDir { allowed, .. } => {
            format!("allowed directory is {}", allowed.display())
        }
        ResolvePathError::NotUnderAnyAllowedDir { .. } => {
            "not under any allowed directory".to_string()
        }
    }
}

/// Turn a `file://` URI body into a plain filesystem path.
///
/// Handles the `file:///C:/dir/x.jpg` form, whose leading slash must go before
/// Windows can resolve the drive letter, and decodes `%20` style escapes so
/// paths with spaces survive the round trip.
fn normalize_uri_path(raw: &str) -> String {
    let trimmed = raw.trim();
    // `file:///C:/x` arrives here as `/C:/x`.
    let stripped = match trimmed.strip_prefix('/') {
        Some(rest) if looks_like_windows_abs(rest) => rest,
        _ => trimmed,
    };
    percent_decode(stripped)
}

/// True for `C:/...` or `C:\...`.
fn looks_like_windows_abs(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Minimal percent-decoding, enough for paths with escaped spaces.
fn percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(decoded) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(decoded);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Recover a reference object that was flattened into a JSON string.
///
/// Returns the parsed map only when it really is a file reference, so ordinary
/// string arguments that merely happen to be valid JSON are left alone. The
/// cheap `$file` substring check keeps this off the hot path for the vast
/// majority of string arguments, which are not JSON at all.
fn parse_embedded_ref(text: &str) -> Option<Map<String, Value>> {
    let trimmed = text.trim();
    if !trimmed.starts_with('{') || !trimmed.contains(FILE_REF_KEY) {
        return None;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(map)) if map.contains_key(FILE_REF_KEY) => Some(map),
        _ => None,
    }
}

/// True if `params` contains at least one file reference.
///
/// Lets callers skip logging and cloning when there is nothing to expand.
/// Mirrors [`FileRefResolver::expand_value`] exactly: if this returns false for
/// something expansion would have rewritten, the sentinel is forwarded to the
/// server verbatim and the feature silently does nothing.
pub fn has_file_reference(params: &Value) -> bool {
    match params {
        Value::String(text) => {
            text.starts_with(FILE_URI_PREFIX) || parse_embedded_ref(text).is_some()
        }
        Value::Object(map) => {
            map.contains_key(FILE_REF_KEY) || map.values().any(has_file_reference)
        }
        Value::Array(items) => items.iter().any(has_file_reference),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    /// Resolver with no sandbox, for tests that use absolute temp paths.
    fn open_resolver() -> FileRefResolver {
        FileRefResolver::new(
            FsToolConfig::new(None, None, None),
            DEFAULT_MAX_FILE_REF_BYTES,
        )
    }

    fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("rust_bot_fileref_{name}"));
        let mut file = std::fs::File::create(&path).expect("create temp file");
        file.write_all(bytes).expect("write temp file");
        path
    }

    fn path_arg(path: &PathBuf) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn expands_file_uri_shorthand_to_base64() {
        let path = write_temp("shorthand.bin", &[0xFF, 0xD8, 0xFF, 0xD9]);
        let params = json!({"content": format!("file://{}", path_arg(&path))});

        let (out, notes) = open_resolver().expand(&params).expect("expand");

        assert_eq!(out["content"], json!("/9j/2Q=="));
        assert_eq!(notes.len(), 1, "expansion should be reported");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expands_explicit_object_to_byte_array() {
        let path = write_temp("bytes.bin", &[1, 2, 250]);
        let params = json!({
            "content": {"$file": path_arg(&path), "encoding": "bytes"}
        });

        let (out, _) = open_resolver().expand(&params).expect("expand");

        assert_eq!(out["content"], json!([1, 2, 250]));
        // Bytes must stay JSON numbers, not strings.
        assert!(out["content"][0].is_number());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expands_utf8_encoding_to_text() {
        let path = write_temp("text.txt", b"hello mcp");
        let params = json!({"body": {"$file": path_arg(&path), "encoding": "utf8"}});

        let (out, _) = open_resolver().expand(&params).expect("expand");

        assert_eq!(out["body"], json!("hello mcp"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn defaults_to_base64_when_encoding_omitted() {
        let path = write_temp("default.bin", &[0x68, 0x69]);
        let params = json!({"content": {"$file": path_arg(&path)}});

        let (out, _) = open_resolver().expand(&params).expect("expand");

        assert_eq!(out["content"], json!("aGk="));
        let _ = std::fs::remove_file(&path);
    }

    /// Regression: a tool whose `content` schema is a string (Java `byte[]` is
    /// advertised as array-of-string) receives the object form flattened into
    /// JSON text. Observed live against EMS `saveEventImageForEventId`, which
    /// answered "Cannot deserialize value of type `byte[]` from Object value"
    /// because the sentinel was forwarded verbatim instead of expanded.
    #[test]
    fn expands_reference_object_flattened_into_a_string() {
        let path = write_temp("stringified.bin", &[0x68, 0x69]);
        let flattened = json!({"$file": path_arg(&path), "encoding": "base64"}).to_string();
        let params = json!({"content": flattened});
        // Precondition: the argument really is a string, as it arrives on the wire.
        assert!(params["content"].is_string());

        let (out, notes) = open_resolver().expand(&params).expect("expand");

        assert_eq!(out["content"], json!("aGk="));
        assert_eq!(notes.len(), 1, "expansion should be reported");
        let _ = std::fs::remove_file(&path);
    }

    /// `encoding` inside a flattened reference must still be honoured,
    /// otherwise the recovery path silently downgrades to base64.
    #[test]
    fn honours_encoding_inside_flattened_reference() {
        let path = write_temp("stringified_bytes.bin", &[1, 2, 250]);
        let flattened = json!({"$file": path_arg(&path), "encoding": "bytes"}).to_string();

        let (out, _) = open_resolver()
            .expand(&json!({"content": flattened}))
            .expect("expand");

        assert_eq!(out["content"], json!([1, 2, 250]));
        let _ = std::fs::remove_file(&path);
    }

    /// `has_file_reference` gates expansion in `MCPToolWrapper::execute`. If it
    /// disagrees with `expand_value`, the sentinel reaches the server raw.
    #[test]
    fn detects_reference_flattened_into_a_string() {
        let flattened = json!({"$file": "C:/x.jpg", "encoding": "base64"}).to_string();
        assert!(has_file_reference(&json!({"content": flattened})));
    }

    /// Ordinary string arguments must not be reinterpreted as references just
    /// because they contain JSON, or unrelated tool calls would break.
    #[test]
    fn leaves_json_strings_without_a_reference_untouched() {
        for text in [
            r#"{"name": "banner.png", "encoding": "base64"}"#,
            "not json at all",
            "{ unbalanced",
            r#"{"nested": {"deep": 1}}"#,
        ] {
            let params = json!({"content": text});
            assert!(
                !has_file_reference(&params),
                "should not treat {text:?} as a file reference"
            );
            let (out, notes) = open_resolver().expand(&params).expect("expand");
            assert_eq!(out, params, "{text:?} must pass through unchanged");
            assert!(notes.is_empty());
        }
    }

    /// A string mentioning `$file` that is not a well-formed object must fall
    /// through as plain text rather than erroring the whole tool call.
    #[test]
    fn ignores_malformed_flattened_reference() {
        let params = json!({"content": "{\"$file\": truncated"});

        let (out, notes) = open_resolver().expand(&params).expect("expand");

        assert_eq!(out, params);
        assert!(notes.is_empty());
    }

    #[test]
    fn expands_references_nested_in_arrays_and_objects() {
        let path = write_temp("nested.bin", &[0x41]);
        let params = json!({
            "outer": {"list": [{"$file": path_arg(&path), "encoding": "utf8"}]}
        });

        let (out, notes) = open_resolver().expand(&params).expect("expand");

        assert_eq!(out["outer"]["list"][0], json!("A"));
        assert_eq!(notes.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn leaves_ordinary_values_untouched() {
        let params = json!({
            "eventId": 7255314,
            "name": "banner.jpg",
            "flag": true,
            "nothing": null,
            "list": [1, 2, 3]
        });

        let (out, notes) = open_resolver().expand(&params).expect("expand");

        assert_eq!(out, params, "non-sentinel arguments must pass through");
        assert!(notes.is_empty());
    }

    #[test]
    fn rejects_path_outside_workspace_when_restricted() {
        let workspace = std::env::temp_dir().join("rust_bot_fileref_ws");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let outside = write_temp("secret.bin", b"secret");

        let resolver = FileRefResolver::new(
            FsToolConfig::new(
                Some(workspace.clone()),
                Some(workspace.clone()),
                Some(vec![]),
            ),
            DEFAULT_MAX_FILE_REF_BYTES,
        );
        let params = json!({"content": format!("file://{}", path_arg(&outside))});

        let err = resolver.expand(&params).expect_err("must be denied");

        assert!(
            matches!(err, FileRefError::Denied { .. }),
            "expected sandbox denial, got {err:?}"
        );
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn enforces_size_limit() {
        let path = write_temp("big.bin", &[0u8; 512]);
        let resolver =
            FileRefResolver::new(FsToolConfig::new(None, None, None), 16);
        let params = json!({"content": format!("file://{}", path_arg(&path))});

        let err = resolver.expand(&params).expect_err("must exceed limit");

        assert!(
            matches!(err, FileRefError::TooLarge { .. }),
            "expected size rejection, got {err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reports_missing_file() {
        let missing = std::env::temp_dir().join("rust_bot_fileref_absent.bin");
        let _ = std::fs::remove_file(&missing);
        let params = json!({"content": format!("file://{}", path_arg(&missing))});

        let err = open_resolver().expand(&params).expect_err("must fail");

        assert!(matches!(err, FileRefError::Unreadable { .. }));
    }

    #[test]
    fn rejects_unknown_encoding() {
        let path = write_temp("enc.bin", b"x");
        let params = json!({"content": {"$file": path_arg(&path), "encoding": "rot13"}});

        let err = open_resolver().expand(&params).expect_err("must fail");

        assert!(matches!(err, FileRefError::UnknownEncoding(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_non_string_path() {
        let params = json!({"content": {"$file": 42}});

        let err = open_resolver().expand(&params).expect_err("must fail");

        assert!(matches!(err, FileRefError::InvalidPathType));
    }

    #[test]
    fn detects_presence_of_references() {
        assert!(has_file_reference(&json!("file://C:/x.jpg")));
        assert!(has_file_reference(&json!({"a": {"$file": "x"}})));
        assert!(has_file_reference(&json!([{"b": "file://x"}])));
        assert!(!has_file_reference(&json!({"a": "plain", "b": [1, 2]})));
    }

    #[test]
    fn normalizes_windows_uri_paths() {
        // `file:///C:/dir/x.jpg` reaches the resolver as `/C:/dir/x.jpg`.
        assert_eq!(normalize_uri_path("/C:/dir/x.jpg"), "C:/dir/x.jpg");
        // A genuine POSIX absolute path keeps its leading slash.
        assert_eq!(normalize_uri_path("/home/u/x.jpg"), "/home/u/x.jpg");
        // Escaped spaces are decoded.
        assert_eq!(normalize_uri_path("/C:/my%20dir/x.jpg"), "C:/my dir/x.jpg");
    }
}
