use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::multipart;

pub enum PathLike {
    Str(String),
    Path(PathBuf),
}

#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    async fn transcribe(&self, file_path: PathLike) -> String;
}

pub struct OpenAITranscriptionProvider {
    api_key: String,
    api_url: String,
    model: String,
}

impl OpenAITranscriptionProvider {
    pub fn new(api_url: impl Into<String>, api_key: Option<String>, model: Option<String>) -> Self {
        Self {
            api_key: api_key.unwrap_or_else(|| {
                std::env::var("OPENAI_TRANSCRIPTION_API_KEY")
                    .expect("OPENAI_TRANSCRIPTION_API_KEY is not set for OpenAI transcription provider")
            }),
            api_url: api_url.into(),
            model: model.unwrap_or_else(|| "whisper-1".to_string()),
        }
    }
}

#[async_trait]
impl TranscriptionProvider for OpenAITranscriptionProvider {
    async fn transcribe(&self, file_path: PathLike) -> String {
        let path = match file_path {
            PathLike::Str(path) => PathBuf::from(path),
            PathLike::Path(path) => path,
        };
        if path.as_os_str().is_empty() {
            return "Empty file path to audio file to transcribe".to_string();
        }

        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(e) => return format!("Error opening audio file: {e}"),
        };
        let mut buffer = Vec::new();
        if let Err(e) = file.read_to_end(&mut buffer) {
            return format!("Error reading audio file: {e}");
        }

        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio")
            .to_string();

        let form = multipart::Form::new()
            .part(
                "file",
                multipart::Part::bytes(buffer).file_name(file_name),
            )
            .text("model", self.model.clone());

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
        {
            Ok(client) => client,
            Err(e) => return format!("Error creating HTTP client: {e}"),
        };

        let response_result = client
            .post(&self.api_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .multipart(form)
            .send()
            .await;

        match response_result {
            Ok(response) => {
                if let Err(e) = response.error_for_status_ref() {
                    return format!("Error transcribing audio file: {e}");
                }
                match response.json::<serde_json::Value>().await {
                    Ok(response_value) => response_value
                        .get("text")
                        .and_then(|text| text.as_str())
                        .unwrap_or("")
                        .to_string(),
                    Err(e) => format!("Error parsing transcription response: {e}"),
                }
            }
            Err(e) => format!("Error transcribing audio file: {e}"),
        }
    }
}
