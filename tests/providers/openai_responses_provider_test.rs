//! Live checks of `OpenAIResponsesProvider` against api.openai.com.
//!
//! They run only when `OPENAI_RESPONSES_API_KEY` holds an api.openai.com key (in the
//! environment or `.env`); otherwise they print a note and pass. A dedicated variable
//! is used because `OPENAI_API_KEY` in this repository's `.env` is an OpenRouter key.
//! `OPENAI_RESPONSES_MODEL` picks the model (default `gpt-6-astra`).

use rust_bot::providers::base::{LLMProvider, LLMResponse};
use rust_bot::providers::openai_responses_provider::OpenAIResponsesProvider;
use rust_bot::utils::helpers::build_assistant_message;
use serde_json::{Value, json};

const DEFAULT_MODEL: &str = "gpt-6-astra";
const REASONING_EFFORT: &str = "medium";
const MAX_TOKENS: usize = 4096;

/// The provider and model to test, or `None` when no key is configured.
fn live_provider() -> Option<(OpenAIResponsesProvider, String)> {
    dotenv::dotenv().ok();
    let api_key = std::env::var("OPENAI_RESPONSES_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty());
    let Some(api_key) = api_key else {
        eprintln!("OPENAI_RESPONSES_API_KEY is not set; skipping the live Responses API test");
        return None;
    };
    let model = std::env::var("OPENAI_RESPONSES_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let provider =
        OpenAIResponsesProvider::new(Some(api_key), None, Some(model.clone()), None, None);
    Some((provider, model))
}

fn weather_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get the current weather for a city",
            "parameters": {
                "type": "object",
                "properties": {
                    "location": { "type": "string", "description": "City and country" }
                },
                "required": ["location"]
            }
        }
    })
}

/// The assistant message the agent runner stores for `response`.
fn stored_assistant_message(response: &LLMResponse) -> Value {
    let tool_calls: Vec<Value> = response
        .tool_calls
        .iter()
        .map(|call| call.to_openai_tool_call())
        .collect();
    let thinking_blocks = response.thinking_blocks.as_ref().map(|blocks| {
        blocks
            .iter()
            .map(|block| Value::Object(block.clone().into_iter().collect()))
            .collect()
    });
    build_assistant_message(
        response.content.as_deref(),
        (!tool_calls.is_empty()).then_some(tool_calls),
        response.reasoning_content.as_deref(),
        thinking_blocks,
    )
}

async fn send(
    provider: &OpenAIResponsesProvider,
    messages: &[Value],
    tools: Option<Vec<Value>>,
) -> LLMResponse {
    let response = provider
        .chat(
            messages.to_vec(),
            tools,
            None,
            MAX_TOKENS,
            Some(0.3),
            Some(REASONING_EFFORT.to_string()),
            None,
        )
        .await;
    assert_ne!(response.finish_reason, "error", "{:?}", response.content);
    response
}

/// Tool call with reasoning, the tool result, then a follow-up user turn that replays
/// the reasoning of both earlier replies.
#[tokio::test]
async fn a_tool_round_trip_with_reasoning_and_a_follow_up_turn() {
    let Some((provider, model)) = live_provider() else {
        return;
    };
    let mut messages = vec![
        json!({ "role": "system", "content": "You are a terse assistant. Use tools when asked." }),
        json!({ "role": "user", "content": "Use the get_weather tool for Lisbon, Portugal, then tell me the result." }),
    ];

    let tool_turn = send(&provider, &messages, Some(vec![weather_tool()])).await;
    assert_eq!(
        tool_turn.finish_reason, "tool_calls",
        "{model}: {tool_turn:?}"
    );
    assert_eq!(tool_turn.tool_calls[0].name, "get_weather");
    assert!(
        tool_turn.thinking_blocks.is_some(),
        "no replay log: {tool_turn:?}"
    );
    messages.push(stored_assistant_message(&tool_turn));
    for call in &tool_turn.tool_calls {
        messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "name": call.name,
            "content": "Sunny, 24 °C, light breeze."
        }));
    }

    let answer = send(&provider, &messages, Some(vec![weather_tool()])).await;
    assert_eq!(answer.finish_reason, "stop", "{answer:?}");
    let answer_text = answer.content.clone().unwrap_or_default();
    assert!(answer_text.contains("24"), "{answer_text}");
    messages.push(stored_assistant_message(&answer));

    messages.push(json!({ "role": "user", "content": "And in Fahrenheit? One line." }));
    let follow_up = send(&provider, &messages, Some(vec![weather_tool()])).await;
    assert!(
        follow_up.content.is_some() || follow_up.has_tool_calls(),
        "{follow_up:?}"
    );
}

/// History written by another model is sent without its reasoning and item ids.
#[tokio::test]
async fn history_from_another_model_is_accepted() {
    let Some((provider, _model)) = live_provider() else {
        return;
    };
    let foreign_assistant = json!({
        "role": "assistant",
        "content": "Let me check.",
        "tool_calls": [{
            "id": "call_foreign_1",
            "type": "function",
            "function": { "name": "get_weather", "arguments": "{\"location\":\"Porto, Portugal\"}" }
        }],
        "thinking_blocks": [{
            "type": "openai_responses_item",
            "model": "some-other-model",
            "item": { "type": "reasoning", "id": "rs_foreign", "summary": [], "encrypted_content": "not-valid" }
        }]
    });
    let messages = vec![
        json!({ "role": "user", "content": "What's the weather in Porto?" }),
        foreign_assistant,
        json!({ "role": "tool", "tool_call_id": "call_foreign_1", "content": "Rain, 15 °C." }),
    ];
    let answer = send(&provider, &messages, Some(vec![weather_tool()])).await;
    assert!(answer.content.is_some(), "{answer:?}");
}
