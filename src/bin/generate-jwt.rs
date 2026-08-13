use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rust_bot::api::user_registry::{JsonUserRegistry, User, UserRegistry, hash_password};
use rust_bot::channels::websocket::types::WebSocketConfig;
use rust_bot::config::loader::{load_config, save_config};
use rust_bot::security::jwt::{
    DEFAULT_EXPIRES_IN_MONTHS, generate_jwt_keypair, generate_jwt_token,
};
use rust_bot::utils::exit_codes::{self, GENERAL_ERROR};

#[derive(Debug, Parser)]
#[command(
    name = "generate-jwt",
    about = "Generate Ed25519 JWT keypairs and tokens"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Generate an Ed25519 keypair and update api.jwt key paths in the config file
    GenerateJwtKeypair {
        /// Path to the rust-bot JSON configuration file
        #[arg(long)]
        config: PathBuf,

        /// Directory where private_key.pem and public_key.pem are written
        #[arg(long, default_value = "./.rust-bot/credentials")]
        credentials_dir: PathBuf,

        /// Overwrite existing key files
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Mint an EdDSA JWT using the private key path from the config file
    GenerateJwtToken {
        /// Path to the rust-bot JSON configuration file
        #[arg(long)]
        config: PathBuf,

        /// Override issuer (defaults to api.jwt.iss from config)
        #[arg(long)]
        iss: Option<String>,

        /// Override audience (defaults to api.jwt.aud from config; empty omits claim)
        #[arg(long)]
        aud: Option<String>,

        /// Purpose claim marking what this token was minted for (e.g. "webui"
        /// for a WebSocket-connecting WebUI frontend); omitted when unset
        #[arg(long)]
        purpose: Option<String>,

        /// Token lifetime in months (default: 6)
        #[arg(long, default_value_t = DEFAULT_EXPIRES_IN_MONTHS)]
        expires_in_months: u32,

        /// The email of the user for whom the token is being generated
        #[arg(long, required = true)]
        user_email: String,

        /// Optional password; stored as an Argon2id hash in the users file
        #[arg(long)]
        password: Option<String>,

        /// Path to the JSON user registry file (email -> token map)
        #[arg(long, required = true)]
        users_file: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::GenerateJwtKeypair {
            config,
            credentials_dir,
            force,
        } => {
            if let Err(err) = run_generate_keypair(config, credentials_dir, force) {
                eprintln!("ERROR: {err}");
                exit_codes::exit(GENERAL_ERROR);
            }
        }
        Commands::GenerateJwtToken {
            config,
            iss,
            aud,
            purpose,
            expires_in_months,
            user_email,
            password,
            users_file,
        } => {
            if let Err(err) = run_generate_token(
                config,
                iss,
                aud,
                purpose,
                expires_in_months,
                user_email,
                password,
                users_file,
            ) {
                eprintln!("ERROR: {err}");
                exit_codes::exit(GENERAL_ERROR);
            }
        }
    }
    ExitCode::SUCCESS
}

fn path_for_config(path: PathBuf) -> PathBuf {
    let canonical = path.canonicalize().unwrap_or(path);
    #[cfg(windows)]
    {
        let as_str = canonical.to_string_lossy();
        if let Some(stripped) = as_str.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    canonical
}

fn run_generate_keypair(
    config_path: PathBuf,
    credentials_dir: PathBuf,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut config = load_config(Some(config_path.clone()));
    let keys = generate_jwt_keypair(&credentials_dir, force)?;

    let private_key_path = path_for_config(keys.private_key_path);
    let public_key_path = path_for_config(keys.public_key_path);

    config.api.jwt.private_key_path = private_key_path.display().to_string();
    config.api.jwt.public_key_path = public_key_path.display().to_string();
    save_config(&config, Some(config_path.clone()))?;

    eprintln!("Wrote private key: {}", private_key_path.display());
    eprintln!("Wrote public key:  {}", public_key_path.display());
    eprintln!("Updated api.jwt key paths in {}", config_path.display());
    Ok(())
}

/// Resolves the `aud` claim for a minted token.
///
/// Precedence, checked in order:
/// 1. An explicit `--aud` override always wins — the caller knows best.
/// 2. `purpose == "webui"` mints a token meant to authenticate a WebUI
///    frontend's *WebSocket* connection, not a REST API call — so it must
///    carry the WebSocket channel's own audience, or
///    `validate_jwt_aud_matches_path` (`channels::websocket::types`) will
///    401 it at WS upgrade since that channel checks incoming tokens'
///    `aud` against its own `path`, not `api.jwt.aud`. That audience is:
///    - `existing_websocket_config.jwt.aud` if a `websocket` entry already
///      exists in `channels.extra` and its `jwt.aud` is non-empty; else
///    - that same existing entry's `path`; else, if no `websocket` entry
///      exists yet,
///    - `WebSocketConfig::default().path` — the value `run_generate_token`
///      is about to write into a freshly created entry, so the minted token
///      and the config it's paired with always agree.
/// 3. Any other purpose (or none) keeps the REST API's own audience,
///    `api_jwt_aud` (i.e. `api.jwt.aud`) — unchanged from prior behavior.
fn resolve_aud(
    aud_override: Option<&str>,
    purpose: Option<&str>,
    api_jwt_aud: &str,
    existing_websocket_config: Option<&WebSocketConfig>,
) -> String {
    if let Some(explicit) = aud_override {
        return explicit.to_string();
    }

    if purpose == Some("webui") {
        return match existing_websocket_config {
            Some(cfg) if !cfg.jwt.aud.trim().is_empty() => cfg.jwt.aud.clone(),
            Some(cfg) => cfg.path.clone(),
            None => WebSocketConfig::default().path,
        };
    }

    api_jwt_aud.to_string()
}

// One parameter per CLI flag by design (mirrors `Commands::GenerateJwtToken`'s
// own field list) — a params struct would just move the sprawl elsewhere for
// a function called from exactly one place.
#[allow(clippy::too_many_arguments)]
fn run_generate_token(
    config_path: PathBuf,
    iss_override: Option<String>,
    aud_override: Option<String>,
    purpose: Option<String>,
    expires_in_months: u32,
    user_email: String,
    password: Option<String>,
    users_file: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut config = load_config(Some(config_path.clone()));
    let jwt = &config.api.jwt;

    let iss = iss_override.unwrap_or_else(|| jwt.iss.clone());

    // Deserialize the existing `websocket` entry (if any) so `resolve_aud`
    // can mirror the audience the gateway will actually validate against,
    // rather than always falling back to the REST API's own `api.jwt.aud`.
    // A malformed existing entry (fails to deserialize as `WebSocketConfig`)
    // is treated as absent — `resolve_aud` then falls back to the default
    // WebSocket path, the same value used when writing a fresh entry below.
    let existing_websocket_config: Option<WebSocketConfig> = config
        .channels
        .extra
        .get("websocket")
        .and_then(|value| serde_json::from_value(value.clone()).ok());

    let aud = resolve_aud(
        aud_override.as_deref(),
        purpose.as_deref(),
        &jwt.aud,
        existing_websocket_config.as_ref(),
    );
    let purpose = purpose.unwrap_or_default();

    let minted = generate_jwt_token(&jwt.private_key_path, iss, aud, purpose, expires_in_months)?;

    let mut registry = JsonUserRegistry::open(users_file.clone())?;
    registry.register_user(&User {
        email: user_email,
        password_hash: hash_password(password)?,
        token: minted.token.clone(),
    })?;
    // Canonicalize after register_user so the file exists on disk.
    config.api.users_file = path_for_config(users_file).display().to_string();

    let websocket_config = WebSocketConfig::default();
    if !config.channels.extra.contains_key("websocket") {
        config.channels.extra.insert(
            "websocket".to_string(),
            serde_json::json!({
                "enabled": websocket_config.enabled,
                "host": websocket_config.host,
                "port": websocket_config.port,
                "path": websocket_config.path,
                "jwt": serde_json::json!({
                    "enabled": true,
                    "privateKeyPath": config.api.jwt.private_key_path,
                    "publicKeyPath": config.api.jwt.public_key_path,
                    "iss": config.api.jwt.iss,
                    "aud": websocket_config.path,
                }),
                "allowFrom": websocket_config.allow_from,
                "streaming": websocket_config.streaming,
                "maxMessageBytes": websocket_config.max_message_bytes,
                "pingIntervalS": websocket_config.ping_interval_s,
                "pingTimeoutS": websocket_config.ping_timeout_s,
                "sslCertfile": websocket_config.ssl_certfile,
            }),
        );
    }

    eprintln!("Updated websocket config in {}", config_path.display());

    save_config(&config, Some(config_path.clone()))?;

    eprintln!("sub: {}", minted.claims.sub);
    eprintln!("exp: {} (unix)", minted.claims.exp);
    if let Some(aud) = &minted.claims.aud {
        eprintln!("aud: {aud}");
    } else {
        eprintln!("aud: (omitted)");
    }
    if let Some(purpose) = &minted.claims.purpose {
        eprintln!("purpose: {purpose}");
    } else {
        eprintln!("purpose: (omitted)");
    }
    println!("{}", minted.token);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `purpose = "webui"`, no `--aud` override, no existing `websocket`
    /// entry in `channels.extra` yet: falls back to
    /// `WebSocketConfig::default().path`, matching what `run_generate_token`
    /// is about to write into a freshly created entry.
    #[test]
    fn webui_purpose_with_no_existing_websocket_config_uses_default_path() {
        let aud = resolve_aud(None, Some("webui"), "https://api.example.com", None);
        assert_eq!(aud, WebSocketConfig::default().path);
    }

    /// `purpose = "webui"`, no `--aud` override, an existing `websocket`
    /// entry whose `jwt.aud` is already set: that existing `jwt.aud` wins,
    /// not the default path.
    #[test]
    fn webui_purpose_with_existing_jwt_aud_uses_that_aud() {
        let existing = WebSocketConfig {
            path: "/ws".to_string(),
            jwt: rust_bot::config::schema::JwtConfig {
                aud: "/ws".to_string(),
                ..Default::default()
            },
            ..WebSocketConfig::default()
        };

        let aud = resolve_aud(
            None,
            Some("webui"),
            "https://api.example.com",
            Some(&existing),
        );
        assert_eq!(aud, "/ws");
    }

    /// `purpose = "webui"`, no `--aud` override, an existing `websocket`
    /// entry whose `path` was customized but `jwt.aud` is left empty: falls
    /// back to that existing entry's `path`, not the global default.
    #[test]
    fn webui_purpose_with_existing_config_and_empty_jwt_aud_uses_existing_path() {
        let existing = WebSocketConfig {
            path: "/custom-ws".to_string(),
            ..WebSocketConfig::default()
        };
        assert!(existing.jwt.aud.trim().is_empty(), "test assumes empty aud");

        let aud = resolve_aud(
            None,
            Some("webui"),
            "https://api.example.com",
            Some(&existing),
        );
        assert_eq!(aud, "/custom-ws");
    }

    /// An explicit `--aud` override always wins, even for `purpose = "webui"`
    /// and even when an existing `websocket` config entry would otherwise
    /// resolve to a different audience.
    #[test]
    fn explicit_override_wins_even_for_webui_purpose() {
        let existing = WebSocketConfig {
            path: "/ws".to_string(),
            ..WebSocketConfig::default()
        };

        let aud = resolve_aud(
            Some("explicit-aud"),
            Some("webui"),
            "https://api.example.com",
            Some(&existing),
        );
        assert_eq!(aud, "explicit-aud");
    }

    /// No purpose (or some other purpose) keeps the prior behavior: fall
    /// back to `api.jwt.aud`, regardless of any existing `websocket` config.
    #[test]
    fn non_webui_purpose_falls_back_to_api_jwt_aud() {
        let existing = WebSocketConfig {
            path: "/ws".to_string(),
            ..WebSocketConfig::default()
        };

        assert_eq!(
            resolve_aud(None, None, "https://api.example.com", Some(&existing)),
            "https://api.example.com"
        );
        assert_eq!(
            resolve_aud(
                None,
                Some("some-other-purpose"),
                "https://api.example.com",
                Some(&existing)
            ),
            "https://api.example.com"
        );
    }
}
