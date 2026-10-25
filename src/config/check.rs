//! `rust-bot check-config`: validate a config file without starting anything.
//!
//! Meant for deployments (a container entrypoint, CI) where a bad config should be
//! reported with a clear message before the gateway starts, instead of surfacing as a
//! panic or a half-working bot. The checks are pure functions over the file's text and
//! an environment lookup, so they are unit tested without touching the process
//! environment.

use serde_json::Value;

use crate::config::loader::referenced_env_vars;
use crate::config::schema::{Config, validate_model_presets};

/// JSON pointer to the flag that turns on login for the WebUI websocket.
const WEBSOCKET_JWT_ENABLED_POINTER: &str = "/channels/websocket/jwt/enabled";

/// Bind addresses that only accept connections from the same machine.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "::1", "localhost"];

/// What `check-config` found. Errors stop the bot from starting; warnings do not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ConfigReport {
    /// Problems that make the config unusable.
    pub errors: Vec<String>,
    /// Risky but valid settings.
    pub warnings: Vec<String>,
}

impl ConfigReport {
    /// Whether the config can be used (warnings do not count).
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Check the text of a config file.
///
/// `env_is_set` tells whether an environment variable is defined; it is injected so the
/// check does not depend on the real process environment.
pub fn check_config_text(text: &str, env_is_set: impl Fn(&str) -> bool) -> ConfigReport {
    let mut report = ConfigReport::default();
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            report.errors.push(format!("not valid JSON: {error}"));
            return report;
        }
    };

    report.errors.extend(
        referenced_env_vars(&value)
            .into_iter()
            .filter(|name| !env_is_set(name))
            .map(|name| format!("environment variable '{name}' is referenced but not set")),
    );

    match serde_json::from_value::<Config>(value.clone()) {
        Ok(config) => {
            if let Err(error) = validate_model_presets(&config) {
                report.errors.push(error);
            }
            report.warnings.extend(exposure_warning(&config, &value));
        }
        Err(error) => report
            .errors
            .push(format!("does not match the config schema: {error}")),
    }
    report
}

/// Warn when the gateway listens beyond this machine while the WebUI has no login.
///
/// Anyone who can reach such a port can drive the agent's tools, including the shell.
fn exposure_warning(config: &Config, raw: &Value) -> Option<String> {
    let host = config.gateway.host.as_str();
    let jwt_enabled = raw
        .pointer(WEBSOCKET_JWT_ENABLED_POINTER)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (!LOOPBACK_HOSTS.contains(&host) && !jwt_enabled).then(|| {
        format!(
            "gateway.host is '{host}' (reachable from the network) but \
             channels.websocket.jwt.enabled is not true: anyone who can reach the port can \
             use the agent's tools. Enable JWT login or keep the port private."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_set(_: &str) -> bool {
        true
    }

    fn none_set(_: &str) -> bool {
        false
    }

    #[test]
    fn an_empty_object_is_a_valid_config() {
        assert!(check_config_text("{}", all_set).is_ok());
    }

    #[test]
    fn malformed_json_is_an_error() {
        let report = check_config_text("{ nope", all_set);
        assert!(!report.is_ok());
        assert!(report.errors[0].contains("not valid JSON"));
    }

    #[test]
    fn every_unset_placeholder_is_reported_by_name() {
        let text = r#"{"providers": {"openai": {"apiKey": "${OPENAI_API_KEY}"}},
                       "tools": {"mcpServers": {"s": {"command": "x", "args": ["${TOKEN}"]}}}}"#;
        let report = check_config_text(text, none_set);
        assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
        assert!(report.errors.iter().any(|e| e.contains("OPENAI_API_KEY")));
        assert!(report.errors.iter().any(|e| e.contains("TOKEN")));
    }

    #[test]
    fn set_placeholders_are_not_reported() {
        let text = r#"{"providers": {"openai": {"apiKey": "${OPENAI_API_KEY}"}}}"#;
        assert!(check_config_text(text, all_set).is_ok());
    }

    #[test]
    fn an_unknown_model_preset_reference_is_an_error() {
        let report = check_config_text(r#"{"agents": {"modelPreset": "nope"}}"#, all_set);
        assert!(!report.is_ok());
    }

    #[test]
    fn a_network_reachable_gateway_without_login_warns() {
        let report = check_config_text(r#"{"gateway": {"host": "0.0.0.0"}}"#, all_set);
        assert!(report.is_ok(), "a warning is not an error");
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("0.0.0.0"));
    }

    #[test]
    fn login_or_a_loopback_bind_silences_the_warning() {
        let with_login = r#"{"gateway": {"host": "0.0.0.0"},
                             "channels": {"websocket": {"jwt": {"enabled": true}}}}"#;
        assert!(check_config_text(with_login, all_set).warnings.is_empty());
        let loopback = r#"{"gateway": {"host": "127.0.0.1"}}"#;
        assert!(check_config_text(loopback, all_set).warnings.is_empty());
    }
}
