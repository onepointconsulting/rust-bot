//! Partial port of nanobot's `webui/transcript.py`. Currently covers only
//! what `handle_envelope_message` needs to stamp a client-supplied `turn_id`
//! onto inbound message metadata (`client_turn_metadata`). The rest of
//! `WebUiTranscriptRecorder` (`append_user_message`, `prepare_and_append`,
//! its `_turn_sequences` ordering state, ...) isn't ported yet — when it is,
//! this file is the natural home for the struct that will own that state.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use uuid::Uuid;

/// Metadata key carrying the WebUI-tracked turn id. Mirrors nanobot's
/// `WEBUI_TURN_METADATA_KEY` (`webui/metadata.py:3`).
pub const WEBUI_TURN_METADATA_KEY: &str = "webui_turn_id";

/// Mirrors nanobot's `_WEBUI_TURN_ID_RE` (`webui/transcript.py:37`).
static WEBUI_TURN_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._:-]{1,128}$").unwrap());

/// Accept a client-supplied turn id only if it's a validly-shaped string;
/// otherwise mint a fresh one. Mirrors nanobot's `normalize_webui_turn_id`
/// (`webui/transcript.py:651-656`).
///
/// Takes the raw envelope value (as opposed to an already-extracted `&str`)
/// so it can apply the same `isinstance(value, str)` gate nanobot's
/// dynamically-typed `Any` parameter does: a non-string JSON value (a
/// number, an object, `null`, ...) is treated the same as a missing one.
pub fn normalize_webui_turn_id(value: Option<&serde_json::Value>) -> String {
    if let Some(candidate) = value.and_then(|v| v.as_str()) {
        let candidate = candidate.trim();
        if WEBUI_TURN_ID_RE.is_match(candidate) {
            return candidate.to_string();
        }
    }
    Uuid::new_v4().to_string()
}

/// Build the metadata patch nanobot merges via
/// `metadata.update(self._transcripts.client_turn_metadata(...))`.
/// Mirrors `WebUiTranscriptRecorder.client_turn_metadata`
/// (`webui/transcript.py:681-682`).
pub fn client_turn_metadata(turn_id: Option<&serde_json::Value>) -> HashMap<String, serde_json::Value> {
    HashMap::from([(
        WEBUI_TURN_METADATA_KEY.to_string(),
        serde_json::Value::String(normalize_webui_turn_id(turn_id)),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_webui_turn_id_keeps_a_validly_shaped_string() {
        let value = serde_json::json!("client-turn-123");
        assert_eq!(normalize_webui_turn_id(Some(&value)), "client-turn-123");
    }

    #[test]
    fn normalize_webui_turn_id_trims_whitespace() {
        let value = serde_json::json!("  client-turn-123  ");
        assert_eq!(normalize_webui_turn_id(Some(&value)), "client-turn-123");
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_when_missing() {
        let id = normalize_webui_turn_id(None);
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_for_empty_string() {
        let value = serde_json::json!("");
        let id = normalize_webui_turn_id(Some(&value));
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_for_non_string_value() {
        let value = serde_json::json!(12345);
        let id = normalize_webui_turn_id(Some(&value));
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_for_overlong_string() {
        let value = serde_json::json!("x".repeat(129));
        let id = normalize_webui_turn_id(Some(&value));
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn normalize_webui_turn_id_generates_a_uuid_for_disallowed_characters() {
        let value = serde_json::json!("has space");
        let id = normalize_webui_turn_id(Some(&value));
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn client_turn_metadata_carries_the_normalized_id_under_the_expected_key() {
        let value = serde_json::json!("client-turn-123");
        let metadata = client_turn_metadata(Some(&value));
        assert_eq!(
            metadata.get(WEBUI_TURN_METADATA_KEY),
            Some(&serde_json::Value::String("client-turn-123".to_string()))
        );
    }
}
