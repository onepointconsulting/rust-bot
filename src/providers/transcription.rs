use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::multipart;

pub const GROQ_DEFAULT_MODEL: &'static str = "whisper-large-v3";
pub const OPENAI_DEFAULT_MODEL: &'static str = "whisper-1";
pub const GROQ_DEFAULT_API_URL: &'static str =
    "https://api.groq.com/openai/v1/audio/transcriptions";
pub const OPENAI_DEFAULT_API_URL: &'static str = "https://api.openai.com/v1/audio/transcriptions";

fn convert_path(file_path: PathLike) -> PathBuf {
    let path: PathBuf = match file_path {
        PathLike::Str(path) => PathBuf::from(path),
        PathLike::Path(path) => path,
    };
    path
}

pub enum PathLike {
    Str(String),
    Path(PathBuf),
}

#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    fn new(api_url: impl Into<String>, api_key: Option<String>, model: Option<String>) -> Self
    where
        Self: Sized;

    fn get_api_key(&self) -> String;

    fn get_api_url(&self) -> String;

    fn get_model(&self) -> String;

    async fn transcribe(&self, file_path: PathLike) -> String {
        let path = convert_path(file_path);
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
            .part("file", multipart::Part::bytes(buffer).file_name(file_name))
            .text("model", self.get_model());

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
        {
            Ok(client) => client,
            Err(e) => return format!("Error creating HTTP client: {e}"),
        };

        let response_result = client
            .post(&self.get_api_url())
            .header("Authorization", format!("Bearer {}", self.get_api_key()))
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

pub struct OpenAITranscriptionProvider {
    api_key: String,
    api_url: String,
    model: String,
}

#[async_trait]
impl TranscriptionProvider for OpenAITranscriptionProvider {
    fn new(api_url: impl Into<String>, api_key: Option<String>, model: Option<String>) -> Self {
        Self {
            api_key: api_key.unwrap_or_else(|| {
                std::env::var("OPENAI_TRANSCRIPTION_API_KEY").expect(
                    "OPENAI_TRANSCRIPTION_API_KEY is not set for OpenAI transcription provider",
                )
            }),
            api_url: api_url.into(),
            model: model.unwrap_or_else(|| OPENAI_DEFAULT_MODEL.to_string()),
        }
    }

    fn get_api_key(&self) -> String {
        self.api_key.clone()
    }

    fn get_api_url(&self) -> String {
        self.api_url.clone()
    }

    fn get_model(&self) -> String {
        self.model.clone()
    }
}

/// Voice transcription provider using Groq's Whisper API.
/// Groq offers extremely fast transcription with a generous free tier.
pub struct GroqTranscriptionProvider {
    api_key: String,
    api_url: String,
    model: String,
}

#[async_trait]
impl TranscriptionProvider for GroqTranscriptionProvider {
    fn new(api_url: impl Into<String>, api_key: Option<String>, model: Option<String>) -> Self {
        let api_key = api_key.unwrap_or_else(|| {
            std::env::var("GROQ_TRANSCRIPTION_API_KEY")
                .expect("GROQ_TRANSCRIPTION_API_KEY is not set for OpenAI transcription provider")
        });
        Self {
            api_key,
            api_url: api_url.into(),
            model: model.unwrap_or_else(|| GROQ_DEFAULT_MODEL.to_string()),
        }
    }

    fn get_api_key(&self) -> String {
        self.api_key.clone()
    }

    fn get_api_url(&self) -> String {
        self.api_url.clone()
    }

    fn get_model(&self) -> String {
        self.model.clone()
    }
}
