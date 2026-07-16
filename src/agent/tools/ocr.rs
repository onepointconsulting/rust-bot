use std::path::{Path, PathBuf};

use async_trait::async_trait;
use base64::Engine;
use serde_json::json;

use crate::{
    agent::tools::{
        base::Tool,
        filesystem::{FsToolConfig, ResolvePathError},
    },
    config::schema::{OcrProvider, OcrToolConfig},
    providers::{
        anthropic_provider::AnthropicProvider,
        base::LLMProvider,
    },
};

const OCR_PROMPT: &str = "Extract all readable text from the provided document or image. \
Preserve reading order, paragraph breaks, and table structure where possible. \
Return only the extracted text with no commentary or preamble.";

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const OCR_MAX_TOKENS: usize = 8192 * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OcrContentKind {
    Document,
    Image,
}

struct OcrMediaType {
    kind: OcrContentKind,
    media_type: &'static str,
}

fn ocr_err(msg: impl Into<String>) -> String {
    let msg = msg.into();
    log::error!("{}", msg);
    msg
}

fn media_type_for_path(path: &Path) -> Result<OcrMediaType, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .ok_or_else(|| "Error: file has no extension; supported: pdf, png, jpg, jpeg, gif, webp".to_string())?;

    match ext.as_str() {
        "pdf" => Ok(OcrMediaType {
            kind: OcrContentKind::Document,
            media_type: "application/pdf",
        }),
        "png" => Ok(OcrMediaType {
            kind: OcrContentKind::Image,
            media_type: "image/png",
        }),
        "jpg" | "jpeg" => Ok(OcrMediaType {
            kind: OcrContentKind::Image,
            media_type: "image/jpeg",
        }),
        "gif" => Ok(OcrMediaType {
            kind: OcrContentKind::Image,
            media_type: "image/gif",
        }),
        "webp" => Ok(OcrMediaType {
            kind: OcrContentKind::Image,
            media_type: "image/webp",
        }),
        other => Err(format!(
            "Error: unsupported file type '.{other}'; supported: pdf, png, jpg, jpeg, gif, webp"
        )),
    }
}

fn build_content_block(media: &OcrMediaType, data_b64: &str) -> serde_json::Value {
    let source = json!({
        "type": "base64",
        "media_type": media.media_type,
        "data": data_b64,
    });
    match media.kind {
        OcrContentKind::Document => json!({ "type": "document", "source": source }),
        OcrContentKind::Image => json!({ "type": "image", "source": source }),
    }
}

fn format_resolve_error(err: ResolvePathError) -> String {
    match err {
        ResolvePathError::HomeDirUnavailable => {
            ocr_err("Error: home directory unavailable for path expansion")
        }
        ResolvePathError::NotUnderAllowedDir { path, allowed } => ocr_err(format!(
            "Error: path {} is outside allowed directory {}",
            path.display(),
            allowed.display()
        )),
        ResolvePathError::NotUnderAnyAllowedDir { path } => ocr_err(format!(
            "Error: path {} is outside allowed directories",
            path.display()
        )),
    }
}

pub struct OcrTool {
    name: String,
    description: String,
    config: OcrToolConfig,
    fs: FsToolConfig,
    provider: AnthropicProvider,
}

impl OcrTool {
    pub fn new(
        config: OcrToolConfig,
        workspace: PathBuf,
        allowed_dir: Option<PathBuf>,
        extra_read: Vec<PathBuf>,
    ) -> Self {
        let provider = match config.provider {
            OcrProvider::Anthropic => AnthropicProvider::new(
                Some(config.api_key.clone()).filter(|k| !k.is_empty()),
                Some(config.base_url.clone()),
                Some(config.model.clone()),
                None,
                None,
            ),
        };
        Self {
            name: "ocr".to_string(),
            description: "Extract text from a PDF document or image file (png, jpg, gif, webp)."
                .to_string(),
            config,
            fs: FsToolConfig::new(Some(workspace), allowed_dir, Some(extra_read)),
            provider,
        }
    }
}

#[async_trait]
impl Tool for OcrTool {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn read_only(&self) -> bool {
        true
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to a PDF or image file to extract text from",
                },
            },
            "required": ["file_path"],
        })
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        let file_path = params
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(file_path) = file_path else {
            return "Error: missing required parameter 'file_path'".to_string();
        };

        let resolved = match self.fs.resolve(file_path) {
            Ok(path) => path,
            Err(err) => return format_resolve_error(err),
        };

        if !resolved.exists() {
            return ocr_err(format!("Error: file not found: {}", resolved.display()));
        }
        if !resolved.is_file() {
            return ocr_err(format!("Error: not a file: {}", resolved.display()));
        }

        let media = match media_type_for_path(&resolved) {
            Ok(media) => media,
            Err(e) => return e,
        };

        let metadata = match std::fs::metadata(&resolved) {
            Ok(m) => m,
            Err(e) => {
                return ocr_err(format!(
                    "Error: failed to read file metadata for {}: {e}",
                    resolved.display()
                ));
            }
        };
        if metadata.len() > MAX_FILE_BYTES {
            return ocr_err(format!(
                "Error: file exceeds maximum size of {} bytes: {}",
                MAX_FILE_BYTES,
                resolved.display()
            ));
        }

        let bytes = match std::fs::read(&resolved) {
            Ok(b) => b,
            Err(e) => {
                return ocr_err(format!(
                    "Error: failed to read file {}: {e}",
                    resolved.display()
                ));
            }
        };

        use base64::engine::general_purpose::STANDARD;
        let data_b64 = STANDARD.encode(&bytes);
        let content_block = build_content_block(&media, &data_b64);

        let messages = vec![json!({
            "role": "user",
            "content": [
                { "type": "text", "text": OCR_PROMPT },
                content_block,
            ],
        })];

        let response = self
            .provider
            .chat(
                messages,
                None,
                Some(self.config.model.clone()),
                OCR_MAX_TOKENS,
                0.0,
                None,
                None,
            )
            .await;

        if response.finish_reason == "error" {
            return ocr_err(
                response
                    .content
                    .unwrap_or_else(|| "Unknown Anthropic API error".to_string()),
            );
        }

        match response.content {
            Some(text) if !text.trim().is_empty() => text,
            _ => ocr_err("Error: OCR returned empty text"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_for_path_pdf() {
        let media = media_type_for_path(Path::new("report.pdf")).unwrap();
        assert_eq!(media.kind, OcrContentKind::Document);
        assert_eq!(media.media_type, "application/pdf");
    }

    #[test]
    fn media_type_for_path_png() {
        let media = media_type_for_path(Path::new("scan.PNG")).unwrap();
        assert_eq!(media.kind, OcrContentKind::Image);
        assert_eq!(media.media_type, "image/png");
    }

    #[test]
    fn media_type_for_path_jpeg_aliases() {
        let jpg = media_type_for_path(Path::new("photo.jpg")).unwrap();
        let jpeg = media_type_for_path(Path::new("photo.jpeg")).unwrap();
        assert_eq!(jpg.media_type, "image/jpeg");
        assert_eq!(jpeg.media_type, "image/jpeg");
    }

    #[test]
    fn media_type_for_path_rejects_unknown() {
        assert!(media_type_for_path(Path::new("file.txt")).is_err());
        assert!(media_type_for_path(Path::new("file")).is_err());
    }

    #[test]
    fn build_content_block_document_shape() {
        let media = OcrMediaType {
            kind: OcrContentKind::Document,
            media_type: "application/pdf",
        };
        let block = build_content_block(&media, "abc123");
        assert_eq!(block["type"], "document");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], "application/pdf");
        assert_eq!(block["source"]["data"], "abc123");
    }

    #[test]
    fn build_content_block_image_shape() {
        let media = OcrMediaType {
            kind: OcrContentKind::Image,
            media_type: "image/png",
        };
        let block = build_content_block(&media, "xyz");
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["media_type"], "image/png");
    }
}
