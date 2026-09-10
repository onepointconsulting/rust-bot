use std::path::PathBuf;

use async_trait::async_trait;
use base64::Engine;

use crate::{agent::tools::base::Tool, config::schema::ImageGenerationToolConfig};

fn image_gen_err(msg: impl Into<String>) -> String {
    let msg = msg.into();
    log::error!("{}", msg);
    msg
}

pub struct ImageGenerationTool {
    config: ImageGenerationToolConfig,
    image_folder: PathBuf,
    http_client: reqwest::Client,
}

impl ImageGenerationTool {
    pub fn new(config: ImageGenerationToolConfig, workspace: PathBuf) -> Self {
        let api_key = Some(config.api_key.clone())
            .filter(|k| !k.is_empty())
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
            .unwrap_or_default();

        let mut header_map = reqwest::header::HeaderMap::new();
        use reqwest::header::{AUTHORIZATION, HeaderValue};
        if let Ok(val) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
            header_map.insert(AUTHORIZATION, val);
        };
        let http_client = reqwest::Client::builder()
            .default_headers(header_map)
            .build()
            .unwrap_or_default();
        let image_folder = workspace.join("images_generation");
        Self {
            config,
            image_folder,
            http_client,
        }
    }
}

/// OpenRouter accepts HTTP(S) URLs or base64 `data:image/...` URLs as references.
fn is_supported_reference_url(url: &str) -> bool {
    url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("data:image/")
}

/// Rebuild one OpenRouter `input_references` item:
/// `{ "type": "image_url", "image_url": { "url": "..." } }`.
fn normalize_input_reference(
    item: &serde_json::Value,
    index: usize,
) -> Result<serde_json::Value, String> {
    let Some(obj) = item.as_object() else {
        return Err(format!(
            "Error: input_references[{index}] must be an object with 'type' and 'image_url'"
        ));
    };

    match obj.get("type").and_then(|v| v.as_str()) {
        Some("image_url") => {}
        Some(other) => {
            return Err(format!(
                "Error: input_references[{index}].type must be 'image_url', got '{other}'"
            ));
        }
        None => {
            return Err(format!(
                "Error: input_references[{index}] is missing required property 'type'"
            ));
        }
    }

    let url = obj
        .get("image_url")
        .and_then(|v| v.get("url"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(url) = url else {
        return Err(format!(
            "Error: input_references[{index}].image_url.url is required and must be a non-empty string"
        ));
    };

    if !is_supported_reference_url(url) {
        return Err(format!(
            "Error: input_references[{index}].image_url.url must be an HTTP(S) URL or a data:image... base64 URL"
        ));
    }

    Ok(serde_json::json!({
        "type": "image_url",
        "image_url": { "url": url }
    }))
}

/// Parse optional OpenRouter `input_references`. Missing or empty is omitted
/// from the request; present-but-invalid values are errors.
fn extract_input_references(
    params: &serde_json::Value,
) -> Result<Option<Vec<serde_json::Value>>, String> {
    let Some(raw) = params.get("input_references") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let Some(items) = raw.as_array() else {
        return Err(
            "Error: 'input_references' must be an array of { type, image_url } objects".to_string(),
        );
    };
    if items.is_empty() {
        return Ok(None);
    }

    let mut references = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        references.push(normalize_input_reference(item, index)?);
    }
    Ok(Some(references))
}

#[async_trait]
impl Tool for ImageGenerationTool {
    fn name(&self) -> String {
        "image_generation".to_string()
    }

    fn description(&self) -> String {
        "Generate an image based on a text description, optionally guided by \
reference images via input_references (OpenRouter image-to-image). \
This tool returns the path to a local file that contains the generated image or a plain text error message."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The text description of the image to generate.",
                },
                "size": {
                    "type": "string",
                    "description": "Optional image size, either a tier (e.g. '1K', '2K') or explicit pixel dimensions (e.g. '1024x1024'). Defaults to the configured size.",
                },
                "quality": {
                    "type": "string",
                    "description": "Optional rendering quality: 'auto', 'low', 'medium', or 'high'. Defaults to the configured quality.",
                },
                "input_references": {
                    "type": "array",
                    "description": "Optional OpenRouter reference images for image-to-image generation. Each item must be { \"type\": \"image_url\", \"image_url\": { \"url\": \"...\" } }. The url may be an HTTP(S) URL or a base64 data URL (data:image/png;base64,...).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "type": {
                                "type": "string",
                                "enum": ["image_url"],
                                "description": "Must be 'image_url'.",
                            },
                            "image_url": {
                                "type": "object",
                                "description": "Reference image payload.",
                                "properties": {
                                    "url": {
                                        "type": "string",
                                        "description": "HTTP(S) image URL or a data:image/...;base64,... data URL.",
                                    },
                                },
                                "required": ["url"],
                            }
                        },
                        "required": ["type", "image_url"],
                    },
                }
            },
            "required": ["prompt"],
        })
    }

    async fn execute(&self, params: &serde_json::Value) -> String {
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(prompt) = prompt else {
            return "Error: missing required parameter 'prompt'".to_string();
        };

        let size = params
            .get("size")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| Some(self.config.size.clone()).filter(|s| !s.is_empty()));

        let quality = params
            .get("quality")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| Some(self.config.quality.clone()).filter(|s| !s.is_empty()));

        let input_references = match extract_input_references(params) {
            Ok(refs) => refs,
            Err(e) => return e,
        };

        let mut body = serde_json::json!({
            "model": self.config.model,
            "prompt": prompt,
        });
        let body_obj = body.as_object_mut().expect("body is always an object");
        if let Some(size) = size {
            body_obj.insert("size".to_string(), serde_json::Value::String(size));
        }
        if let Some(quality) = quality {
            body_obj.insert("quality".to_string(), serde_json::Value::String(quality));
        }
        if let Some(input_references) = input_references {
            body_obj.insert(
                "input_references".to_string(),
                serde_json::Value::Array(input_references),
            );
        }

        let response = match self
            .http_client
            .post(&self.config.base_url)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => return image_gen_err(format!("Error: image generation request failed: {e}")),
        };

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return image_gen_err(format!(
                "Error: image generation API returned {status}: {text}"
            ));
        }

        let payload: serde_json::Value = match response.json().await {
            Ok(v) => v,
            Err(e) => {
                return image_gen_err(format!(
                    "Error: failed to parse image generation response: {e}"
                ));
            }
        };

        let data_array = match payload.get("data").and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => arr,
            _ => return image_gen_err("Error: image generation API returned no data"),
        };

        let b64_json = data_array
            .first()
            .and_then(|entry| entry.get("b64_json"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(b64_json) = b64_json else {
            return image_gen_err(
                "Error: image generation API response is missing 'b64_json' data",
            );
        };

        use base64::engine::general_purpose::STANDARD;
        let image_bytes = match STANDARD.decode(b64_json) {
            Ok(bytes) => bytes,
            Err(e) => {
                return image_gen_err(format!("Error: failed to decode image data: {e}"));
            }
        };

        if let Err(e) = std::fs::create_dir_all(&self.image_folder) {
            return image_gen_err(format!(
                "Error: failed to create image folder {}: {e}",
                self.image_folder.display()
            ));
        }

        let file_path = self
            .image_folder
            .join(format!("{}.png", uuid::Uuid::new_v4()));
        if let Err(e) = std::fs::write(&file_path, &image_bytes) {
            return image_gen_err(format!(
                "Error: failed to write image file {}: {e}",
                file_path.display()
            ));
        }

        file_path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_omits_missing_and_empty_references() {
        assert_eq!(extract_input_references(&json!({})).unwrap(), None);
        assert_eq!(
            extract_input_references(&json!({ "input_references": null })).unwrap(),
            None
        );
        assert_eq!(
            extract_input_references(&json!({ "input_references": [] })).unwrap(),
            None
        );
    }

    #[test]
    fn extract_normalizes_openrouter_http_and_data_urls() {
        let params = json!({
            "input_references": [
                { "type": "image_url", "image_url": { "url": "https://example.com/photo.jpg" } },
                { "type": "image_url", "image_url": { "url": "  data:image/png;base64,iVBORw0KGgoAAAANS...  " } },
            ]
        });
        let refs = extract_input_references(&params).unwrap().unwrap();
        assert_eq!(
            refs,
            vec![
                json!({ "type": "image_url", "image_url": { "url": "https://example.com/photo.jpg" } }),
                json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KGgoAAAANS..." } }),
            ]
        );
    }

    #[test]
    fn extract_rejects_malformed_references() {
        assert!(extract_input_references(&json!({ "input_references": "https://x" })).is_err());
        assert!(
            extract_input_references(&json!({
                "input_references": [{ "type": "file", "image_url": { "url": "https://x" } }]
            }))
            .is_err()
        );
        assert!(
            extract_input_references(&json!({
                "input_references": [{ "type": "image_url" }]
            }))
            .is_err()
        );
        assert!(
            extract_input_references(&json!({
                "input_references": [{ "type": "image_url", "image_url": { "url": "/tmp/local.png" } }]
            }))
            .is_err()
        );
    }
}
