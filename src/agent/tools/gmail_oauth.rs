//! Gmail OAuth helpers shared by the Gmail tools, `rust-bot gmail-auth`, the
//! config wizard, the standalone `gmail-auth` binary and the Tools settings page.
//!
//! The Gmail tools need two files:
//! - `client_secret.json`: an OAuth Desktop-app client downloaded from Google
//!   Cloud Console. It is copied into place from a download, or written from
//!   the client built into release binaries ([`built_in_client_secret`]).
//! - `token_cache.json`: written by an OAuth sign-in ([`authorise_gmail`]).

use std::path::{Path, PathBuf};

use crate::config::schema::GmailToolConfig;
use yup_oauth2::authenticator::DefaultAuthenticator;
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

/// Gmail API base URL.
pub const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1";

/// OAuth scope that lets the tools list and read messages.
pub const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// OAuth scope that lets the tools send messages.
pub const GMAIL_SEND_SCOPE: &str = "https://www.googleapis.com/auth/gmail.send";

/// Every scope requested at sign-in, so one token cache serves all Gmail tools.
pub const GMAIL_AUTH_SCOPES: [&str; 2] = [GMAIL_READONLY_SCOPE, GMAIL_SEND_SCOPE];

/// File name of the persisted OAuth token cache.
const TOKEN_CACHE_FILE_NAME: &str = "token_cache.json";

/// The command users run to install the client secret and sign in.
pub const GMAIL_AUTH_COMMAND: &str = "rust-bot gmail-auth";

/// The OAuth Desktop client embedded at build time from the
/// `RUST_BOT_GMAIL_CLIENT_SECRET` environment variable (see `build.rs`).
/// Empty when the binary was built without one.
const BUILT_IN_CLIENT_SECRET_JSON: &str = include_str!(concat!(
    env!("OUT_DIR"),
    "/gmail_default_client_secret.json"
));

/// The OAuth Desktop client built into this binary, or `None` when it was
/// built without one. Users with a built-in client only need to sign in.
pub fn built_in_client_secret() -> Option<&'static str> {
    let json = BUILT_IN_CLIENT_SECRET_JSON.trim();
    (!json.is_empty()).then_some(json)
}

/// Path of the token cache for a given client secret file: `token_cache.json`
/// in the same directory as the client secret, so both files live together.
pub fn token_cache_path_for(client_secret_path: &Path) -> PathBuf {
    client_secret_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(TOKEN_CACHE_FILE_NAME)
}

/// Which of the two Gmail credential files are present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmailCredentialState {
    /// `client_secret.json` is missing (nothing else can happen until it is installed).
    MissingClientSecret,
    /// The client secret exists but there is no token cache yet: nobody signed in.
    NotAuthorised,
    /// Both files exist.
    Authorised,
}

/// Classify the Gmail credentials by checking whether both files exist. Does
/// not read or validate their contents.
pub fn gmail_credential_state(
    client_secret_path: &Path,
    token_cache_path: &Path,
) -> GmailCredentialState {
    if !client_secret_path.is_file() {
        GmailCredentialState::MissingClientSecret
    } else if !token_cache_path.is_file() {
        GmailCredentialState::NotAuthorised
    } else {
        GmailCredentialState::Authorised
    }
}

/// The user-facing explanation of why the Gmail tools are disabled, or `None`
/// when `state` is [`GmailCredentialState::Authorised`]. Shared by the CLI,
/// gateway, websockets-chat and the Tools page so they all say the same thing.
pub fn missing_credentials_message(
    state: GmailCredentialState,
    client_secret_path: &Path,
    token_cache_path: &Path,
) -> Option<String> {
    credentials_message(
        state,
        client_secret_path,
        token_cache_path,
        built_in_client_secret().is_some(),
    )
}

/// [`missing_credentials_message`] with the built-in client made explicit.
/// With a built-in client a missing `client_secret.json` is not the user's
/// problem: `rust-bot gmail-auth` writes it, so only signing in is asked for.
fn credentials_message(
    state: GmailCredentialState,
    client_secret_path: &Path,
    token_cache_path: &Path,
    built_in_client_available: bool,
) -> Option<String> {
    match state {
        GmailCredentialState::MissingClientSecret if built_in_client_available => {
            credentials_message(
                GmailCredentialState::NotAuthorised,
                client_secret_path,
                token_cache_path,
                built_in_client_available,
            )
        }
        GmailCredentialState::MissingClientSecret => Some(format!(
            "Gmail tools disabled: client_secret.json not found at {}. Download an OAuth \
             Desktop client from Google Cloud Console, then run `{GMAIL_AUTH_COMMAND} \
             --client-secret <file>` and restart.",
            client_secret_path.display()
        )),
        GmailCredentialState::NotAuthorised => Some(format!(
            "Gmail tools disabled: not signed in (no token cache at {}). Run \
             `{GMAIL_AUTH_COMMAND}` and restart.",
            token_cache_path.display()
        )),
        GmailCredentialState::Authorised => None,
    }
}

/// Why the Gmail tools configured by `config` cannot run, or `None` when both
/// credential files exist. Ignores `config.enable`: callers decide whether a
/// disabled tool matters.
pub fn gmail_unavailable_reason(config: &GmailToolConfig) -> Option<String> {
    let client_secret_path = config.client_secret_path();
    let token_cache_path = config.token_cache_path();
    let state = gmail_credential_state(&client_secret_path, &token_cache_path);
    missing_credentials_message(state, &client_secret_path, &token_cache_path)
}

/// Check that `bytes` is a Google OAuth client secret (an `installed` or `web` client).
fn validate_client_secret_bytes(bytes: &[u8], source: &Path) -> Result<(), String> {
    yup_oauth2::parse_application_secret(bytes)
        .map(|_| ())
        .map_err(|e| {
            format!(
                "{} is not a Google OAuth client secret: {e}",
                source.display()
            )
        })
}

/// Whether `first` and `second` name the same file on disk. Falls back to
/// comparing the paths when either cannot be canonicalised.
fn is_same_file(first: &Path, second: &Path) -> bool {
    match (first.canonicalize(), second.canonicalize()) {
        (Ok(first), Ok(second)) => first == second,
        _ => first == second,
    }
}

/// Install a downloaded client secret at `destination`.
///
/// Checks the file is a valid Google OAuth client secret, creates the
/// destination directory and copies it. Refuses to replace a different
/// existing file unless `overwrite` is set; an identical existing file is left
/// alone. Does nothing when `downloaded` already is `destination`.
pub fn install_client_secret(
    downloaded: &Path,
    destination: &Path,
    overwrite: bool,
) -> Result<PathBuf, String> {
    let downloaded_bytes = std::fs::read(downloaded)
        .map_err(|e| format!("Cannot read {}: {e}", downloaded.display()))?;
    validate_client_secret_bytes(&downloaded_bytes, downloaded)?;
    if is_same_file(downloaded, destination) {
        return Ok(destination.to_path_buf());
    }
    if destination.exists() {
        let existing_bytes = std::fs::read(destination).unwrap_or_default();
        if existing_bytes == downloaded_bytes {
            return Ok(destination.to_path_buf());
        }
        if !overwrite {
            return Err(format!(
                "A different client secret already exists at {}. Use --force to replace it.",
                destination.display()
            ));
        }
    }
    create_parent_dir(destination)?;
    std::fs::write(destination, &downloaded_bytes)
        .map_err(|e| format!("Cannot write {}: {e}", destination.display()))?;
    Ok(destination.to_path_buf())
}

/// Write the client secret built into this binary to `destination` when no
/// file is there yet. Returns whether a file was written; an existing file
/// (e.g. one the user installed) is never replaced.
pub fn install_built_in_client_secret_if_missing(destination: &Path) -> Result<bool, String> {
    write_client_secret_if_missing(destination, built_in_client_secret())
}

/// Write `client_secret_json` to `destination` unless `destination` already
/// exists or there is nothing to write. Validates the JSON first.
fn write_client_secret_if_missing(
    destination: &Path,
    client_secret_json: Option<&str>,
) -> Result<bool, String> {
    let Some(client_secret_json) = client_secret_json else {
        return Ok(false);
    };
    if destination.exists() {
        return Ok(false);
    }
    validate_client_secret_bytes(
        client_secret_json.as_bytes(),
        Path::new("the built-in Gmail client"),
    )?;
    create_parent_dir(destination)?;
    std::fs::write(destination, client_secret_json)
        .map_err(|e| format!("Cannot write {}: {e}", destination.display()))?;
    Ok(true)
}

/// Create the parent directory of `path` if it has one and it is missing.
fn create_parent_dir(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create directory {}: {e}", parent.display())),
        _ => Ok(()),
    }
}

/// Build an authenticator that runs the installed-app flow (opening a local
/// redirect server) when no usable token is cached, and persists tokens to
/// `token_cache_path`.
async fn build_authenticator(
    client_secret_path: &Path,
    token_cache_path: &Path,
) -> Result<DefaultAuthenticator, String> {
    let secret = yup_oauth2::read_application_secret(client_secret_path)
        .await
        .map_err(|e| {
            format!(
                "Cannot read client secret {}: {e}",
                client_secret_path.display()
            )
        })?;
    InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
        .persist_tokens_to_disk(token_cache_path.to_path_buf())
        .build()
        .await
        .map_err(|e| format!("Cannot start the Google sign-in: {e}"))
}

/// Run the Google sign-in for [`GMAIL_AUTH_SCOPES`] and persist the tokens to
/// `token_cache_path` (creating its directory). Opens a local redirect server
/// and asks the user to sign in in the browser if no valid token is cached.
/// Returns the access token.
pub async fn authorise_gmail(
    client_secret_path: &Path,
    token_cache_path: &Path,
) -> Result<String, String> {
    create_parent_dir(token_cache_path)?;
    let authenticator = build_authenticator(client_secret_path, token_cache_path).await?;
    let token = authenticator
        .token(&GMAIL_AUTH_SCOPES)
        .await
        .map_err(|e| format!("Google sign-in failed: {e}"))?;
    token
        .token()
        .map(str::to_string)
        .ok_or_else(|| "Google sign-in returned no access token".to_string())
}

/// Smoke check: list at most one inbox message with `access_token`. Returns
/// the number of messages returned (0 or 1).
pub async fn verify_inbox_access(access_token: &str) -> Result<usize, String> {
    let response = reqwest::Client::new()
        .get(format!("{GMAIL_API}/users/me/messages"))
        .bearer_auth(access_token)
        .query(&[("labelIds", "INBOX"), ("maxResults", "1")])
        .send()
        .await
        .map_err(|e| format!("Gmail API request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Gmail API error {status}: {body}"));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Gmail API returned invalid JSON: {e}"))?;
    Ok(body["messages"].as_array().map_or(0, Vec::len))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but valid Desktop-app client secret.
    const VALID_CLIENT_SECRET: &str = r#"{"installed":{"client_id":"id.apps.googleusercontent.com","client_secret":"secret","auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token","redirect_uris":["http://localhost"]}}"#;

    #[test]
    fn token_cache_is_placed_next_to_client_secret() {
        let path = token_cache_path_for(Path::new("/some/dir/client_secret.json"));
        assert_eq!(path, Path::new("/some/dir/token_cache.json"));
    }

    #[test]
    fn token_cache_for_default_location_is_in_credentials_dir() {
        let path = token_cache_path_for(Path::new("./credentials/client_secret.json"));
        assert_eq!(path, Path::new("./credentials/token_cache.json"));
    }

    #[test]
    fn token_cache_for_bare_file_name_is_in_current_dir() {
        let path = token_cache_path_for(Path::new("client_secret.json"));
        assert_eq!(path, Path::new("token_cache.json"));
    }

    #[test]
    fn credential_state_reports_missing_client_secret_first() {
        let dir = tempfile::tempdir().unwrap();
        let state = gmail_credential_state(
            &dir.path().join("client_secret.json"),
            &dir.path().join("token_cache.json"),
        );
        assert_eq!(state, GmailCredentialState::MissingClientSecret);
    }

    #[test]
    fn credential_state_is_not_authorised_without_token_cache() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("client_secret.json");
        std::fs::write(&secret, VALID_CLIENT_SECRET).unwrap();
        let state = gmail_credential_state(&secret, &dir.path().join("token_cache.json"));
        assert_eq!(state, GmailCredentialState::NotAuthorised);
    }

    #[test]
    fn credential_state_is_authorised_when_both_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("client_secret.json");
        let cache = dir.path().join("token_cache.json");
        std::fs::write(&secret, VALID_CLIENT_SECRET).unwrap();
        std::fs::write(&cache, "[]").unwrap();
        assert_eq!(
            gmail_credential_state(&secret, &cache),
            GmailCredentialState::Authorised
        );
    }

    #[test]
    fn missing_secret_message_names_the_path_and_the_command() {
        let secret = Path::new("/creds/client_secret.json");
        let message = credentials_message(
            GmailCredentialState::MissingClientSecret,
            secret,
            Path::new("/creds/token_cache.json"),
            false,
        )
        .unwrap();
        assert!(message.contains(&secret.display().to_string()));
        assert!(message.contains("gmail-auth --client-secret"));
    }

    #[test]
    fn missing_secret_with_a_built_in_client_only_asks_to_sign_in() {
        let cache = Path::new("/creds/token_cache.json");
        let message = credentials_message(
            GmailCredentialState::MissingClientSecret,
            Path::new("/creds/client_secret.json"),
            cache,
            true,
        )
        .unwrap();
        assert!(message.contains("not signed in"));
        assert!(message.contains(&cache.display().to_string()));
        assert!(!message.contains("--client-secret"));
    }

    #[test]
    fn built_in_secret_is_written_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("nested").join("client_secret.json");
        assert!(write_client_secret_if_missing(&destination, Some(VALID_CLIENT_SECRET)).unwrap());
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            VALID_CLIENT_SECRET
        );
    }

    #[test]
    fn built_in_secret_never_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("client_secret.json");
        std::fs::write(&destination, "user's own client").unwrap();
        assert!(!write_client_secret_if_missing(&destination, Some(VALID_CLIENT_SECRET)).unwrap());
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "user's own client"
        );
    }

    #[test]
    fn nothing_is_written_without_a_built_in_secret() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("client_secret.json");
        assert!(!write_client_secret_if_missing(&destination, None).unwrap());
        assert!(!destination.exists());
    }

    #[test]
    fn an_invalid_built_in_secret_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("client_secret.json");
        assert!(write_client_secret_if_missing(&destination, Some("{\"nope\":1}")).is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn not_authorised_message_names_the_token_cache_and_the_command() {
        let cache = Path::new("/creds/token_cache.json");
        let message = missing_credentials_message(
            GmailCredentialState::NotAuthorised,
            Path::new("/creds/client_secret.json"),
            cache,
        )
        .unwrap();
        assert!(message.contains(&cache.display().to_string()));
        assert!(message.contains("gmail-auth"));
    }

    #[test]
    fn authorised_has_no_message() {
        assert!(
            missing_credentials_message(
                GmailCredentialState::Authorised,
                Path::new("a"),
                Path::new("b"),
            )
            .is_none()
        );
    }

    #[test]
    fn unavailable_reason_follows_the_configured_paths() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("client_secret.json");
        let cache = dir.path().join("token_cache.json");
        let config = GmailToolConfig {
            client_secret_path: secret.to_string_lossy().to_string(),
            token_cache_path: cache.to_string_lossy().to_string(),
            ..GmailToolConfig::default()
        };
        let expected_without_secret = if built_in_client_secret().is_some() {
            "not signed in"
        } else {
            "client_secret.json not found"
        };
        assert!(
            gmail_unavailable_reason(&config)
                .unwrap()
                .contains(expected_without_secret)
        );
        std::fs::write(&secret, VALID_CLIENT_SECRET).unwrap();
        assert!(
            gmail_unavailable_reason(&config)
                .unwrap()
                .contains("not signed in")
        );
        std::fs::write(&cache, "[]").unwrap();
        assert!(gmail_unavailable_reason(&config).is_none());
    }

    #[test]
    fn install_copies_a_valid_secret_into_a_new_directory() {
        let dir = tempfile::tempdir().unwrap();
        let downloaded = dir.path().join("download.json");
        std::fs::write(&downloaded, VALID_CLIENT_SECRET).unwrap();
        let destination = dir.path().join("nested/creds/client_secret.json");
        let installed = install_client_secret(&downloaded, &destination, false).unwrap();
        assert_eq!(installed, destination);
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            VALID_CLIENT_SECRET
        );
    }

    #[test]
    fn install_rejects_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let downloaded = dir.path().join("download.json");
        std::fs::write(&downloaded, "{\"not\": \"a secret\"}").unwrap();
        let destination = dir.path().join("client_secret.json");
        let error = install_client_secret(&downloaded, &destination, false).unwrap_err();
        assert!(error.contains("not a Google OAuth client secret"));
        assert!(!destination.exists());
    }

    #[test]
    fn install_rejects_a_missing_source() {
        let dir = tempfile::tempdir().unwrap();
        let error = install_client_secret(
            &dir.path().join("missing.json"),
            &dir.path().join("client_secret.json"),
            false,
        )
        .unwrap_err();
        assert!(error.contains("Cannot read"));
    }

    #[test]
    fn install_refuses_to_replace_a_different_file_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let downloaded = dir.path().join("download.json");
        std::fs::write(&downloaded, VALID_CLIENT_SECRET).unwrap();
        let destination = dir.path().join("client_secret.json");
        std::fs::write(&destination, "old").unwrap();
        let error = install_client_secret(&downloaded, &destination, false).unwrap_err();
        assert!(error.contains("--force"));
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "old");
    }

    #[test]
    fn install_replaces_a_different_file_with_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let downloaded = dir.path().join("download.json");
        std::fs::write(&downloaded, VALID_CLIENT_SECRET).unwrap();
        let destination = dir.path().join("client_secret.json");
        std::fs::write(&destination, "old").unwrap();
        install_client_secret(&downloaded, &destination, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            VALID_CLIENT_SECRET
        );
    }

    #[test]
    fn install_accepts_an_identical_existing_file_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let downloaded = dir.path().join("download.json");
        let destination = dir.path().join("client_secret.json");
        std::fs::write(&downloaded, VALID_CLIENT_SECRET).unwrap();
        std::fs::write(&destination, VALID_CLIENT_SECRET).unwrap();
        assert!(install_client_secret(&downloaded, &destination, false).is_ok());
    }

    #[test]
    fn install_onto_itself_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("client_secret.json");
        std::fs::write(&secret, VALID_CLIENT_SECRET).unwrap();
        let installed = install_client_secret(&secret, &secret, false).unwrap();
        assert_eq!(installed, secret);
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            VALID_CLIENT_SECRET
        );
    }
}
