use std::path::PathBuf;
use std::sync::LazyLock;

use async_trait::async_trait;
use rdocx::Document;
use regex::Regex;
use serde_json::json;

use crate::agent::tools::{
    base::Tool,
    filesystem::{FsToolConfig, ResolvePathError},
};

static OUTPUT_EXT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.(pdf|md|html)$").unwrap());

pub struct DocxConversionTool {
    fs: FsToolConfig,
}

impl DocxConversionTool {
    pub fn new(fs: FsToolConfig) -> Self {
        Self { fs }
    }

    fn resolve_allowed_path(&self, path: &str) -> Result<PathBuf, String> {
        self.fs.resolve(path).map_err(|err| match err {
            ResolvePathError::HomeDirUnavailable => {
                "Error: home directory unavailable for path expansion".to_string()
            }
            ResolvePathError::NotUnderAllowedDir { path, allowed } => format!(
                "Error: path {} is outside allowed directory {}",
                path.display(),
                allowed.display()
            ),
            ResolvePathError::NotUnderAnyAllowedDir { path } => format!(
                "Error: path {} is outside allowed directories",
                path.display()
            ),
        })
    }

    fn output_extension(output_path: &str) -> Result<&'static str, String> {
        let caps = OUTPUT_EXT_RE.captures(output_path).ok_or_else(|| {
            format!("Error: output path must end with .pdf, .md or .html, got: {output_path}")
        })?;
        match caps.get(1).unwrap().as_str().to_ascii_lowercase().as_str() {
            "pdf" => Ok("pdf"),
            "md" => Ok("md"),
            "html" => Ok("html"),
            _ => unreachable!("regex only matches pdf, md, html"),
        }
    }

    fn ensure_output_parent(path: &PathBuf) -> Result<(), String> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        if parent.as_os_str().is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Error: failed to create output directory: {e}"))
    }
}

#[async_trait]
impl Tool for DocxConversionTool {
    fn name(&self) -> String {
        "docx".to_string()
    }

    fn description(&self) -> String {
        "Convert a DOCX file to PDF, Markdown, or HTML".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "docx_path": {
                    "type": "string",
                    "description": "Path to a .docx file to convert",
                },
                "output_path": {
                    "type": "string",
                    "description": "Destination path; must end with .pdf, .md, or .html",
                },
            },
            "required": ["docx_path", "output_path"],
        })
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        let docx_path = params
            .get("docx_path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(docx_path) = docx_path else {
            return "Error: missing required parameter 'docx_path'".to_string();
        };
        let output_path = params
            .get("output_path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(output_path) = output_path else {
            return "Error: missing required parameter 'output_path'".to_string();
        };

        let resolved_docx_path = match self.resolve_allowed_path(docx_path) {
            Ok(path) => path,
            Err(message) => return message,
        };
        if !resolved_docx_path.is_file() {
            return format!(
                "Error: DOCX file does not exist or is not a file: {}",
                resolved_docx_path.display()
            );
        }

        let resolved_output_path = match self.resolve_allowed_path(output_path) {
            Ok(path) => path,
            Err(message) => return message,
        };

        let extension = match Self::output_extension(output_path) {
            Ok(ext) => ext,
            Err(message) => return message,
        };

        if let Err(message) = Self::ensure_output_parent(&resolved_output_path) {
            return message;
        }

        let doc = match Document::open(&resolved_docx_path) {
            Ok(doc) => doc,
            Err(e) => return format!("Error: failed to open DOCX file: {e}"),
        };

        match extension {
            "pdf" => match doc.to_pdf() {
                Ok(pdf) => match std::fs::write(&resolved_output_path, &pdf) {
                    Ok(_) => "Successfully converted DOCX file to PDF.".to_string(),
                    Err(e) => format!("Error: failed to write PDF file: {e}"),
                },
                Err(e) => format!("Error: failed to convert DOCX to PDF: {e}"),
            },
            "md" => match std::fs::write(&resolved_output_path, doc.to_markdown()) {
                Ok(_) => "Successfully converted DOCX file to Markdown.".to_string(),
                Err(e) => format!("Error: failed to write Markdown file: {e}"),
            },
            "html" => match std::fs::write(&resolved_output_path, doc.to_html()) {
                Ok(_) => "Successfully converted DOCX file to HTML.".to_string(),
                Err(e) => format!("Error: failed to write HTML file: {e}"),
            },
            _ => format!("Error: unsupported extension: {extension}"),
        }
    }
}
