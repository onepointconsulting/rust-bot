use std::path::PathBuf;

use rust_bot::providers::transcription::{GROQ_DEFAULT_MODEL, GroqTranscriptionProvider, OpenAITranscriptionProvider, PathLike, TranscriptionProvider};

use crate::config::helpers::read_env;


#[tokio::test]
async fn test_openai_transcription_provider() {
    read_env();
    let provider = OpenAITranscriptionProvider::new(
        "https://api.openai.com/v1/audio/transcriptions", 
        None, 
        Some("whisper-1".to_string())
    );
    let response = provider.transcribe(PathLike::Path(PathBuf::from("media/world_models.m4a"))).await;
    assert!(!response.is_empty());
    assert!(
        !response.starts_with("Error "),
        "expected transcription text, got: {response}"
    );
    println!("response: {response}");
}


#[tokio::test]
async fn test_grok_transcription_provider() {
    read_env();
    let provider = GroqTranscriptionProvider::new(
        "https://api.groq.com/openai/v1/audio/transcriptions", 
        None, 
        Some(GROQ_DEFAULT_MODEL.to_string())
    );
    let response = provider.transcribe(PathLike::Path(PathBuf::from("media/world_models.m4a"))).await;
    assert!(!response.is_empty());
    assert!(
        !response.starts_with("Error "),
        "expected transcription text, got: {response}"
    );
    println!("response: {response}");
}