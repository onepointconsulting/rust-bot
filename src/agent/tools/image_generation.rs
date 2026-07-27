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

#[async_trait]
impl Tool for ImageGenerationTool {
    fn name(&self) -> String {
        "image_generation".to_string()
    }

    fn description(&self) -> String {
        "Generate an image based on a text description. \
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
