//! Channel registry — built-in channel discovery and construction.

use std::{collections::HashMap, sync::Arc};

use crate::{
    bus::queue::MessageBus,
    channels::{
        base::BaseChannel,
        email::EmailChannel,
        whatsapp::{WhatsAppChannel, WhatsAppConfig},
    },
    config::{channels::EmailConfig, schema::Config},
};

/// Built-in channel module names.
const BUILTIN_CHANNELS: &[&str] = &["email", "whatsapp"];

/// Return all built-in channel module names.
pub fn discover_channel_names() -> Vec<&'static str> {
    BUILTIN_CHANNELS.to_vec()
}

pub fn discover_all(
    config: &Config,
    bus: Arc<MessageBus>,
) -> HashMap<&'static str, Box<dyn BaseChannel>> {
    let mut channels: HashMap<&'static str, Box<dyn BaseChannel>> = HashMap::new();
    for &name in BUILTIN_CHANNELS {
        match name {
            "email" => {
                let email_cfg = config
                    .channels
                    .extra
                    .get("email")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<EmailConfig>(v).ok())
                    .unwrap_or_default();
                if !email_cfg.enabled {
                    continue;
                }
                channels.insert(
                    name,
                    Box::new(EmailChannel::new(
                        email_cfg,
                        Arc::clone(&bus),
                        config.channels.clone(),
                    )),
                );
            }
            "whatsapp" => {
                let whatsapp_cfg = config
                    .channels
                    .extra
                    .get("whatsapp")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<WhatsAppConfig>(v).ok())
                    .unwrap_or_default();
                if !whatsapp_cfg.enabled {
                    continue;
                }
                channels.insert(
                    name,
                    Box::new(WhatsAppChannel::new(
                        whatsapp_cfg,
                        Arc::clone(&bus),
                        config.channels.clone(),
                    )),
                );
            }
            _ => panic!("Unknown built-in channel: {name}"),
        }
    }
    channels
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

use crate::config::loader::load_config;

use super::*;

    #[test]
    fn discovers_email_channel() {
        let names = discover_channel_names();
        assert!(
            names.contains(&"email"),
            "expected 'email' in {names:?}"
        );
    }

    #[test]
    fn excludes_internal_modules() {
        let names = discover_channel_names();
        for internal in ["base", "manager", "registry", "types"] {
            assert!(
                !names.iter().any(|n| *n == internal),
                "internal module '{internal}' must not appear in {names:?}"
            );
        }
    }

    #[test]
    fn discovers_all_channels() {
        let config_path = PathBuf::from("configs/simple1/email/config.json");
        assert!(config_path.exists(), "config file does not exist");
        let config = load_config(Some(config_path));
        let bus = Arc::new(MessageBus::new());
        let channels = discover_all(&config, Arc::clone(&bus));
        let message = channels.iter().map(|(k, _v)| k.to_string()).collect::<Vec<String>>().join(", ");
        let err= format!("expected 'email' in {}", message);
        assert!(channels.contains_key("email"), "{err}");
    }
}
