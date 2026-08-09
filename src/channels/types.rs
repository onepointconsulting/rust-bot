use std::collections::HashMap;

pub struct MessageBytes {
    pub uid: u32,
    pub bytes: Vec<u8>,
}

impl MessageBytes {
    pub fn new(uid: u32, bytes: Vec<u8>) -> Self {
        Self { uid, bytes }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeType {
    NewChat,
    ForkChat,
    Attach,
    SetWorkspaceScope,
    TranscribeAudio,
    Message,
    /// An envelope whose `type` didn't match any known variant. Carries the
    /// raw type string so the dispatcher can reply with nanobot's
    /// `f"unknown type: {t!r}"` (`runtime.py:850`) — by the time an envelope
    /// reaches dispatch, `_parse_envelope` has already guaranteed `type` is
    /// a string (not missing, not some other JSON value), so `String` here
    /// (not `Option<String>` or `serde_json::Value`) is the right shape.
    Unrecognized(String),
}

impl From<&str> for EnvelopeType {
    /// Maps a raw envelope `type` string (e.g. `"new_chat"`) to its variant.
    /// Infallible by design — anything that isn't one of the six known
    /// values becomes `Unrecognized`, mirroring nanobot's `_dispatch_envelope`
    /// fallthrough (`runtime.py:850`) rather than failing to parse.
    fn from(value: &str) -> Self {
        match value {
            "new_chat" => Self::NewChat,
            "fork_chat" => Self::ForkChat,
            "attach" => Self::Attach,
            "set_workspace_scope" => Self::SetWorkspaceScope,
            "transcribe_audio" => Self::TranscribeAudio,
            "message" => Self::Message,
            other => Self::Unrecognized(other.to_string()),
        }
    }
}

pub type Envelope = HashMap<String, serde_json::Value>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_maps_every_known_type() {
        assert_eq!(EnvelopeType::from("new_chat"), EnvelopeType::NewChat);
        assert_eq!(EnvelopeType::from("fork_chat"), EnvelopeType::ForkChat);
        assert_eq!(EnvelopeType::from("attach"), EnvelopeType::Attach);
        assert_eq!(EnvelopeType::from("set_workspace_scope"), EnvelopeType::SetWorkspaceScope);
        assert_eq!(EnvelopeType::from("transcribe_audio"), EnvelopeType::TranscribeAudio);
        assert_eq!(EnvelopeType::from("message"), EnvelopeType::Message);
    }

    #[test]
    fn from_str_maps_unknown_type_to_unrecognized() {
        assert_eq!(
            EnvelopeType::from("some_future_type"),
            EnvelopeType::Unrecognized("some_future_type".to_string())
        );
    }
}
