//! Ed25519 / EdDSA JWT keypair generation, minting, and validation.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use pkcs8::LineEnding;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default token lifetime: 6 months.
pub const DEFAULT_EXPIRES_IN_MONTHS: u32 = 6;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claims {
    pub iss: String,
    pub sub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    pub exp: i64,
    pub iat: i64,
    /// Custom claim marking what this token was minted for (e.g. `"webui"`),
    /// distinct from `aud` (which, for the WebSocket channel, is already
    /// pinned to the route path — see `validate_jwt_aud_matches_path`). Set
    /// via `generate_jwt_token`'s `purpose` argument (empty omits the claim,
    /// same convention as `aud`); the `generate-jwt` CLI exposes it as
    /// `--purpose`. Checked by `channels::websocket::runtime::authorize`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub purpose: Option<String>,
}

#[derive(Debug)]
pub struct GeneratedKeypair {
    pub private_key_path: PathBuf,
    pub public_key_path: PathBuf,
}

#[derive(Debug)]
pub struct GeneratedToken {
    pub token: String,
    pub claims: Claims,
}

#[derive(Debug)]
pub enum JwtError {
    Message(String),
    Io(std::io::Error),
    Jwt(jsonwebtoken::errors::Error),
    Key(String),
}

impl fmt::Display for JwtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(msg) => write!(f, "{msg}"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Jwt(err) => write!(f, "{err}"),
            Self::Key(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for JwtError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Jwt(err) => Some(err),
            Self::Message(_) | Self::Key(_) => None,
        }
    }
}

impl From<std::io::Error> for JwtError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<jsonwebtoken::errors::Error> for JwtError {
    fn from(value: jsonwebtoken::errors::Error) -> Self {
        Self::Jwt(value)
    }
}

impl JwtError {
    fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Options used when validating a JWT.
#[derive(Debug, Clone)]
pub struct JwtValidationOpts {
    pub iss: String,
    /// When non-empty, the token audience must match.
    pub aud: String,
}

/// Generate an Ed25519 keypair and write PEM files under `credentials_dir`.
///
/// Writes `private_key.pem` and `public_key.pem`. Refuses to overwrite unless
/// `force` is true.
pub fn generate_jwt_keypair(
    credentials_dir: impl AsRef<Path>,
    force: bool,
) -> Result<GeneratedKeypair, JwtError> {
    let credentials_dir = credentials_dir.as_ref();
    fs::create_dir_all(credentials_dir)?;

    let private_key_path = credentials_dir.join("private_key.pem");
    let public_key_path = credentials_dir.join("public_key.pem");

    if !force {
        if private_key_path.exists() {
            return Err(JwtError::msg(format!(
                "Refusing to overwrite existing private key at '{}'; use --force",
                private_key_path.display()
            )));
        }
        if public_key_path.exists() {
            return Err(JwtError::msg(format!(
                "Refusing to overwrite existing public key at '{}'; use --force",
                public_key_path.display()
            )));
        }
    }

    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();

    let private_pem = signing_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| JwtError::Key(e.to_string()))?;
    let public_pem = verifying_key
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| JwtError::Key(e.to_string()))?;

    fs::write(&private_key_path, private_pem.as_bytes())?;
    fs::write(&public_key_path, public_pem.as_bytes())?;

    Ok(GeneratedKeypair {
        private_key_path,
        public_key_path,
    })
}

/// Mint an EdDSA JWT signed with the private key at `private_key_path`.
///
/// Empty `aud`/`purpose` omit their respective claims.
pub fn generate_jwt_token(
    private_key_path: impl AsRef<Path>,
    iss: impl Into<String>,
    aud: impl Into<String>,
    purpose: impl Into<String>,
    expires_in_months: u32,
) -> Result<GeneratedToken, JwtError> {
    let private_pem = fs::read(private_key_path.as_ref()).map_err(|e| {
        JwtError::msg(format!(
            "Failed to read private key '{}': {e}",
            private_key_path.as_ref().display()
        ))
    })?;

    let iss = iss.into();
    let aud_raw = aud.into();
    let aud = if aud_raw.trim().is_empty() {
        None
    } else {
        Some(aud_raw)
    };
    let purpose_raw = purpose.into();
    let purpose = if purpose_raw.trim().is_empty() {
        None
    } else {
        Some(purpose_raw)
    };

    let now = Utc::now();
    let iat = now.timestamp();
    let months = expires_in_months.max(1);
    let exp = now
        .checked_add_months(chrono::Months::new(months))
        .unwrap_or_else(|| now + Duration::days(i64::from(months) * 30))
        .timestamp();

    let claims = Claims {
        iss,
        sub: Uuid::new_v4().to_string(),
        aud,
        exp,
        iat,
        purpose,
    };

    let header = Header::new(Algorithm::EdDSA);
    let encoding_key = EncodingKey::from_ed_pem(&private_pem)?;
    let token = encode(&header, &claims, &encoding_key)?;

    Ok(GeneratedToken { token, claims })
}

/// Validate an EdDSA JWT using the given public key PEM bytes.
pub fn validate_jwt_token(
    token: &str,
    public_key_pem: &[u8],
    opts: &JwtValidationOpts,
) -> Result<Claims, JwtError> {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[opts.iss.as_str()]);
    validation.validate_aud = false;

    if !opts.aud.trim().is_empty() {
        validation.set_audience(&[opts.aud.as_str()]);
        validation.validate_aud = true;
    }

    let decoding_key = DecodingKey::from_ed_pem(public_key_pem)?;
    let data = decode::<Claims>(token, &decoding_key, &validation)?;
    Ok(data.claims)
}

/// Load public key PEM from disk and validate a token.
pub fn validate_jwt_token_from_path(
    token: &str,
    public_key_path: impl AsRef<Path>,
    opts: &JwtValidationOpts,
) -> Result<Claims, JwtError> {
    let public_pem = fs::read(public_key_path.as_ref()).map_err(|e| {
        JwtError::msg(format!(
            "Failed to read public key '{}': {e}",
            public_key_path.as_ref().display()
        ))
    })?;
    validate_jwt_token(token, &public_pem, opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use tempfile::tempdir;

    fn opts(iss: &str, aud: &str) -> JwtValidationOpts {
        JwtValidationOpts {
            iss: iss.to_string(),
            aud: aud.to_string(),
        }
    }

    #[test]
    fn keypair_mint_validate_roundtrip() {
        let dir = tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let minted = generate_jwt_token(
            &keys.private_key_path,
            "rust-bot",
            "https://api.example.com",
            "",
            DEFAULT_EXPIRES_IN_MONTHS,
        )
        .unwrap();

        let claims = validate_jwt_token_from_path(
            &minted.token,
            &keys.public_key_path,
            &opts("rust-bot", "https://api.example.com"),
        )
        .unwrap();

        assert_eq!(claims.iss, "rust-bot");
        assert_eq!(claims.aud.as_deref(), Some("https://api.example.com"));
        assert!(!claims.sub.is_empty());
    }

    #[test]
    fn generate_jwt_token_mints_without_purpose_by_default() {
        let dir = tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let minted = generate_jwt_token(
            &keys.private_key_path,
            "rust-bot",
            "aud-1",
            "",
            DEFAULT_EXPIRES_IN_MONTHS,
        )
        .unwrap();
        assert!(minted.claims.purpose.is_none());
    }

    #[test]
    fn generate_jwt_token_sets_purpose_claim_when_given() {
        let dir = tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let minted = generate_jwt_token(
            &keys.private_key_path,
            "rust-bot",
            "",
            "webui",
            DEFAULT_EXPIRES_IN_MONTHS,
        )
        .unwrap();
        assert_eq!(minted.claims.purpose.as_deref(), Some("webui"));
    }

    #[test]
    fn purpose_claim_round_trips_through_validation() {
        let dir = tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let minted = generate_jwt_token(
            &keys.private_key_path,
            "rust-bot",
            "",
            "webui",
            DEFAULT_EXPIRES_IN_MONTHS,
        )
        .unwrap();

        let validated = validate_jwt_token_from_path(
            &minted.token,
            &keys.public_key_path,
            &opts("rust-bot", ""),
        )
        .unwrap();
        assert_eq!(validated.purpose.as_deref(), Some("webui"));
    }

    #[test]
    fn missing_purpose_claim_deserializes_to_none() {
        // Tokens minted without a purpose (e.g. /v1/login, or the
        // `generate-jwt` CLI without `--purpose`) must still validate, with
        // `purpose: None` — backward compatible.
        let dir = tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let minted = generate_jwt_token(
            &keys.private_key_path,
            "rust-bot",
            "",
            "",
            DEFAULT_EXPIRES_IN_MONTHS,
        )
        .unwrap();
        let validated = validate_jwt_token_from_path(
            &minted.token,
            &keys.public_key_path,
            &opts("rust-bot", ""),
        )
        .unwrap();
        assert!(validated.purpose.is_none());
    }

    #[test]
    fn empty_aud_omits_claim_and_skips_aud_validation() {
        let dir = tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let minted = generate_jwt_token(
            &keys.private_key_path,
            "rust-bot",
            "",
            "",
            DEFAULT_EXPIRES_IN_MONTHS,
        )
        .unwrap();

        assert!(minted.claims.aud.is_none());

        let parts: Vec<_> = minted.token.split('.').collect();
        let payload = parts[1];
        let padded = match payload.len() % 4 {
            2 => format!("{payload}=="),
            3 => format!("{payload}="),
            _ => payload.to_string(),
        };
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&padded))
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert!(json.get("aud").is_none());

        validate_jwt_token_from_path(&minted.token, &keys.public_key_path, &opts("rust-bot", ""))
            .unwrap();
    }

    #[test]
    fn wrong_iss_fails() {
        let dir = tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let minted = generate_jwt_token(
            &keys.private_key_path,
            "rust-bot",
            "aud-1",
            "",
            DEFAULT_EXPIRES_IN_MONTHS,
        )
        .unwrap();

        assert!(
            validate_jwt_token_from_path(
                &minted.token,
                &keys.public_key_path,
                &opts("other-issuer", "aud-1"),
            )
            .is_err()
        );
    }

    #[test]
    fn wrong_aud_fails_when_required() {
        let dir = tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let minted = generate_jwt_token(
            &keys.private_key_path,
            "rust-bot",
            "aud-1",
            "",
            DEFAULT_EXPIRES_IN_MONTHS,
        )
        .unwrap();

        assert!(
            validate_jwt_token_from_path(
                &minted.token,
                &keys.public_key_path,
                &opts("rust-bot", "aud-2"),
            )
            .is_err()
        );
    }

    #[test]
    fn refuse_overwrite_without_force() {
        let dir = tempdir().unwrap();
        generate_jwt_keypair(dir.path(), false).unwrap();
        assert!(generate_jwt_keypair(dir.path(), false).is_err());
        assert!(generate_jwt_keypair(dir.path(), true).is_ok());
    }

    #[test]
    fn tampered_token_fails() {
        let dir = tempdir().unwrap();
        let keys = generate_jwt_keypair(dir.path(), false).unwrap();
        let minted = generate_jwt_token(
            &keys.private_key_path,
            "rust-bot",
            "aud-1",
            "",
            DEFAULT_EXPIRES_IN_MONTHS,
        )
        .unwrap();

        let mut bad = minted.token;
        bad.push('x');
        assert!(
            validate_jwt_token_from_path(&bad, &keys.public_key_path, &opts("rust-bot", "aud-1"))
                .is_err()
        );
    }
}
