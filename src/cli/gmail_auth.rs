//! `rust-bot gmail-auth`: install a downloaded OAuth client secret at
//! `tools.gmail.clientSecretPath` and sign in to Google, writing the token
//! cache to `tools.gmail.tokenCachePath`.

use std::path::{Path, PathBuf};

use crate::agent::tools::gmail_oauth::{
    GmailCredentialState, authorise_gmail, gmail_credential_state,
    install_built_in_client_secret_if_missing, install_client_secret, missing_credentials_message,
    verify_inbox_access,
};
use crate::cli::command_specifications::GmailAuthArgs;
use crate::cli::commands::CliError;
use crate::config::loader::load_config;
use crate::utils::cli::print_warning;
use crate::utils::helpers::expand_tilde_path;

/// What `rust-bot gmail-auth` does for a given credential state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmailAuthAction {
    /// There is no client secret: tell the user how to get one and stop.
    ReportMissingSecret,
    /// Both files exist and `--force` was not given: nothing to do.
    AlreadyAuthorised,
    /// Run the Google sign-in, deleting the old token cache first when asked.
    Authorise { remove_existing_cache: bool },
}

/// Decide what to do from the credential `state` and the `--force` flag.
pub fn plan_gmail_auth(state: GmailCredentialState, force: bool) -> GmailAuthAction {
    match (state, force) {
        (GmailCredentialState::MissingClientSecret, _) => GmailAuthAction::ReportMissingSecret,
        (GmailCredentialState::Authorised, false) => GmailAuthAction::AlreadyAuthorised,
        (GmailCredentialState::Authorised, true) => GmailAuthAction::Authorise {
            remove_existing_cache: true,
        },
        (GmailCredentialState::NotAuthorised, _) => GmailAuthAction::Authorise {
            remove_existing_cache: false,
        },
    }
}

/// The reminder to switch the Gmail tools on, or `None` when they already are.
pub fn enable_reminder(gmail_enabled: bool, config_path: &Path) -> Option<String> {
    (!gmail_enabled).then(|| {
        format!(
            "tools.gmail.enable is false in {}. Set it to true (or use `rust-bot onboard --wizard`) \
             to load the Gmail tools.",
            config_path.display()
        )
    })
}

/// Delete the token cache at `token_cache_path`; a missing file is fine.
pub(crate) fn remove_token_cache(token_cache_path: &Path) -> Result<(), CliError> {
    match std::fs::remove_file(token_cache_path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CliError::Other(format!(
            "Cannot remove the old token cache {}: {e}",
            token_cache_path.display()
        ))),
    }
}

/// Write the client built into this binary to `client_secret_path` when no
/// client secret is there yet, and say so. Does nothing for binaries built
/// without one or when the user already installed their own.
pub(crate) fn install_built_in_client_secret(client_secret_path: &Path) -> Result<(), CliError> {
    if install_built_in_client_secret_if_missing(client_secret_path).map_err(CliError::Other)? {
        println!(
            "Using the built-in Google client (written to {}).",
            client_secret_path.display()
        );
    }
    Ok(())
}

/// Sign in to Google, then list one inbox message to prove the token works.
/// Prints where the token went and what to do next.
pub(crate) async fn sign_in_and_verify(
    client_secret_path: &Path,
    token_cache_path: &Path,
) -> Result<(), CliError> {
    println!("Starting the Google sign-in. Open the link below in your browser to sign in.");
    let access_token = authorise_gmail(client_secret_path, token_cache_path)
        .await
        .map_err(CliError::Other)?;
    println!(
        "Signed in. Token cache written to {}",
        token_cache_path.display()
    );
    match verify_inbox_access(&access_token).await {
        Ok(count) => println!("Inbox check passed ({count} message(s) listed)."),
        Err(e) => print_warning(format!("Signed in, but the inbox check failed: {e}")),
    }
    println!("Restart the agent or gateway to load the Gmail tools.");
    Ok(())
}

/// `rust-bot gmail-auth`: load the config, install `--client-secret` if given
/// (or the built-in client when no client secret exists), then sign in unless already signed in (or `--force` asks to redo it).
pub async fn run_gmail_auth(args: GmailAuthArgs) -> Result<(), CliError> {
    let config = load_config(Some(args.config.clone()));
    let gmail = &config.tools.gmail;
    let client_secret_path = gmail.client_secret_path();
    let token_cache_path = gmail.token_cache_path();
    println!("Using config {}", args.config.display());

    if let Some(downloaded) = &args.client_secret {
        let downloaded = PathBuf::from(expand_tilde_path(&downloaded.to_string_lossy()).as_ref());
        let installed = install_client_secret(&downloaded, &client_secret_path, args.force)
            .map_err(CliError::Other)?;
        println!("Client secret installed at {}", installed.display());
    }
    install_built_in_client_secret(&client_secret_path)?;

    let state = gmail_credential_state(&client_secret_path, &token_cache_path);
    match plan_gmail_auth(state, args.force) {
        GmailAuthAction::ReportMissingSecret => {
            let message =
                missing_credentials_message(state, &client_secret_path, &token_cache_path)
                    .unwrap_or_default();
            return Err(CliError::Other(message));
        }
        GmailAuthAction::AlreadyAuthorised => println!(
            "Already signed in (token cache at {}). Use --force to sign in again.",
            token_cache_path.display()
        ),
        GmailAuthAction::Authorise {
            remove_existing_cache,
        } => {
            if remove_existing_cache {
                remove_token_cache(&token_cache_path)?;
            }
            sign_in_and_verify(&client_secret_path, &token_cache_path).await?;
        }
    }

    if let Some(reminder) = enable_reminder(gmail.enable, &args.config) {
        print_warning(reminder);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_secret_is_reported_with_or_without_force() {
        for force in [false, true] {
            assert_eq!(
                plan_gmail_auth(GmailCredentialState::MissingClientSecret, force),
                GmailAuthAction::ReportMissingSecret
            );
        }
    }

    #[test]
    fn not_authorised_signs_in_without_removing_anything() {
        for force in [false, true] {
            assert_eq!(
                plan_gmail_auth(GmailCredentialState::NotAuthorised, force),
                GmailAuthAction::Authorise {
                    remove_existing_cache: false
                }
            );
        }
    }

    #[test]
    fn authorised_is_left_alone_without_force() {
        assert_eq!(
            plan_gmail_auth(GmailCredentialState::Authorised, false),
            GmailAuthAction::AlreadyAuthorised
        );
    }

    #[test]
    fn authorised_with_force_signs_in_again_from_scratch() {
        assert_eq!(
            plan_gmail_auth(GmailCredentialState::Authorised, true),
            GmailAuthAction::Authorise {
                remove_existing_cache: true
            }
        );
    }

    #[test]
    fn enable_reminder_only_when_gmail_is_off() {
        let config = Path::new("config.json");
        assert!(enable_reminder(true, config).is_none());
        let reminder = enable_reminder(false, config).unwrap();
        assert!(reminder.contains("tools.gmail.enable"));
        assert!(reminder.contains("config.json"));
    }

    #[test]
    fn removing_a_missing_token_cache_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        assert!(remove_token_cache(&dir.path().join("token_cache.json")).is_ok());
    }

    #[test]
    fn removing_an_existing_token_cache_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("token_cache.json");
        std::fs::write(&cache, "[]").unwrap();
        remove_token_cache(&cache).unwrap();
        assert!(!cache.exists());
    }
}
