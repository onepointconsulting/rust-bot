//! Business limits for inbound WebUI messages and attachments.
//!
//! The WebSocket channel owns its raw frame limit. This module owns semantic
//! limits inside a decoded WebUI message so transport capacity isn't mistaken
//! for a text or attachment policy.
//!
//! Port of nanobot's `nanobot/webui/ingress_policy.py`.

use serde_json::{Value, json};

/// Rejection code returned by [`WebUIIngressPolicy::validate_text`].
pub const MESSAGE_TOO_LARGE: &str = "text_too_large";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageIngressLimits {
    pub max_text_bytes: usize,
}

impl MessageIngressLimits {
    pub const DEFAULT: Self = Self {
        max_text_bytes: 64 * 1024,
    };
}

impl Default for MessageIngressLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentIngressLimits {
    pub max_count: usize,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
}

impl AttachmentIngressLimits {
    pub const DEFAULT: Self = Self {
        max_count: 4,
        max_file_bytes: 6 * 1024 * 1024,
        max_total_bytes: 24 * 1024 * 1024,
    };
}

impl Default for AttachmentIngressLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Limits applied after the channel has decoded the transport envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebUIIngressPolicy {
    pub message: MessageIngressLimits,
    pub attachments: AttachmentIngressLimits,
    /// Covers JSON keys, IDs, attachment names, MIME prefixes, mentions, and
    /// other non-content fields when the browser estimates whether a frame fits.
    pub envelope_reserve_bytes: usize,
}

impl Default for WebUIIngressPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl WebUIIngressPolicy {
    pub const DEFAULT: Self = Self {
        message: MessageIngressLimits::DEFAULT,
        attachments: AttachmentIngressLimits::DEFAULT,
        envelope_reserve_bytes: 64 * 1024,
    };

    /// Returns `Some("text_too_large")` when UTF-8 byte length exceeds the limit.
    ///
    /// Mirrors Python's `validate_text` → `MessageRejection | None`. Rust's
    /// [`str::len`] already counts UTF-8 bytes (same as
    /// `len(content.encode("utf-8"))` in Python).
    pub fn validate_text(&self, text: &str) -> Option<&'static str> {
        if text.len() > self.message.max_text_bytes {
            Some(MESSAGE_TOO_LARGE)
        } else {
            None
        }
    }

    pub fn bootstrap_limits(&self, max_frame_bytes: usize) -> Value {
        json!({
            "transport": {
                "max_frame_bytes": max_frame_bytes,
                "envelope_reserve_bytes": self.envelope_reserve_bytes,
            },
            "message": {
                "max_text_bytes": self.message.max_text_bytes,
            },
            "attachments": {
                "max_count": self.attachments.max_count,
                "max_file_bytes": self.attachments.max_file_bytes,
                "max_total_bytes": self.attachments.max_total_bytes,
            },
        })
    }

    /// Conservative frame size needed for every policy-valid message.
    pub fn minimum_full_policy_frame_bytes(&self) -> usize {
        let encoded_attachments = 4 * self.attachments.max_total_bytes.div_ceil(3);
        let data_url_allowance = self.attachments.max_count * 128;
        encoded_attachments
            + data_url_allowance
            + self.message.max_text_bytes
            + self.envelope_reserve_bytes
    }
}

/// Process-wide default policy (nanobot's `DEFAULT_WEBUI_INGRESS_POLICY`).
pub const DEFAULT_WEBUI_INGRESS_POLICY: WebUIIngressPolicy = WebUIIngressPolicy::DEFAULT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_limit_counts_utf8_bytes() {
        let policy = WebUIIngressPolicy::default();

        assert_eq!(
            policy.validate_text(&"x".repeat(policy.message.max_text_bytes)),
            None
        );
        // "你" is 3 UTF-8 bytes; 22_000 of them is 66_000 > 65_536.
        assert_eq!(
            policy.validate_text(&"你".repeat(22_000)),
            Some(MESSAGE_TOO_LARGE)
        );
    }

    #[test]
    fn bootstrap_keeps_transport_and_business_limits_separate() {
        let policy = WebUIIngressPolicy::default();
        let payload = policy.bootstrap_limits(1_048_576);

        assert_eq!(
            payload["transport"],
            json!({
                "max_frame_bytes": 1_048_576,
                "envelope_reserve_bytes": 65_536,
            })
        );
        assert_eq!(payload["message"], json!({ "max_text_bytes": 65_536 }));
        assert_eq!(
            payload["attachments"],
            json!({
                "max_count": 4,
                "max_file_bytes": 6_291_456,
                "max_total_bytes": 25_165_824,
            })
        );
        assert!(policy.minimum_full_policy_frame_bytes() < 36 * 1024 * 1024);
    }
}
