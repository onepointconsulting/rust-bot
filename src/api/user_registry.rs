//! Simple JSON-file-backed registry mapping user emails to JWTs.
//!
//! The token itself never carries the email (see [`crate::security::jwt::Claims`]);
//! this registry provides the out-of-band mapping so operators can look up which
//! token is current for a given user. Tokens are minted by `rust-bot generate-jwt-token` at
//! registration time and refreshed on each successful `/v1/login`. An
//! Argon2id password hash is stored alongside the token for login.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub email: String,
    pub password_hash: Option<String>,
    pub token: String,
}

/// On-disk representation of a single user entry, keyed by email in
/// [`JsonUserRegistry`]'s map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StoredUser {
    token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password_hash: Option<String>,
}

#[derive(Debug)]
pub enum UserRegistryError {
    AlreadyExists(String),
    NotFound(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    PasswordHash(argon2::password_hash::Error),
}

impl fmt::Display for UserRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists(email) => write!(f, "user already registered: {email}"),
            Self::NotFound(email) => write!(f, "user not found: {email}"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::PasswordHash(err) => write!(f, "failed to hash password: {err}"),
        }
    }
}

impl std::error::Error for UserRegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::AlreadyExists(_) | Self::NotFound(_) | Self::PasswordHash(_) => None,
        }
    }
}

impl From<std::io::Error> for UserRegistryError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for UserRegistryError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<argon2::password_hash::Error> for UserRegistryError {
    fn from(value: argon2::password_hash::Error) -> Self {
        Self::PasswordHash(value)
    }
}

/// Hash `password` with Argon2id using a freshly generated random salt,
/// returning the encoded PHC string.
pub fn hash_password(password: String) -> Result<String, UserRegistryError> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string();
    Ok(hash)
}

/// Verify `password` against a previously generated Argon2id `password_hash`.
pub fn verify_password(password: &str, password_hash: &str) -> Result<bool, UserRegistryError> {
    let parsed_hash = PasswordHash::new(password_hash)?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

pub trait UserRegistry {
    fn register_user(&mut self, user: &User) -> Result<(), Box<dyn std::error::Error>>;
    fn get_user_by_email(&self, user_email: &str) -> Result<User, Box<dyn std::error::Error>>;
    fn get_all_users(&self) -> Result<Vec<User>, Box<dyn std::error::Error>>;
    fn delete_user(&mut self, user_email: &str) -> Result<(), Box<dyn std::error::Error>>;
    fn update_user(
        &mut self,
        user_email: &str,
        user: &User,
    ) -> Result<(), Box<dyn std::error::Error>>;
}

/// A [`UserRegistry`] persisted as a single JSON file containing a map of
/// `{ "email": { "token": "...", "password_hash": "..." } }`. `password_hash`
/// is omitted when absent (legacy entries registered before a password was
/// required).
pub struct JsonUserRegistry {
    path: PathBuf,
    users: HashMap<String, StoredUser>,
}

impl JsonUserRegistry {
    pub fn empty() -> Self {
        Self {
            path: PathBuf::new(),
            users: HashMap::new(),
        }
    }

    /// Open the registry at `path`, loading existing entries if the file
    /// exists. A missing file is treated as an empty registry.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, UserRegistryError> {
        let path = path.into();
        let users = match fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self { path, users })
    }

    fn save(&self) -> Result<(), UserRegistryError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let contents = serde_json::to_string_pretty(&self.users)?;
        fs::write(&self.path, contents)?;
        Ok(())
    }
}

fn to_user(email: &str, stored: &StoredUser) -> User {
    User {
        email: email.to_string(),
        password_hash: stored.password_hash.clone(),
        token: stored.token.clone(),
    }
}

impl UserRegistry for JsonUserRegistry {
    fn register_user(&mut self, user: &User) -> Result<(), Box<dyn std::error::Error>> {
        if self.users.contains_key(&user.email) {
            return Err(Box::new(UserRegistryError::AlreadyExists(
                user.email.clone(),
            )));
        }
        self.users.insert(
            user.email.clone(),
            StoredUser {
                token: user.token.clone(),
                password_hash: user.password_hash.clone(),
            },
        );
        self.save()?;
        Ok(())
    }

    fn get_user_by_email(&self, user_email: &str) -> Result<User, Box<dyn std::error::Error>> {
        self.users
            .get(user_email)
            .map(|stored| to_user(user_email, stored))
            .ok_or_else(|| {
                Box::new(UserRegistryError::NotFound(user_email.to_string()))
                    as Box<dyn std::error::Error>
            })
    }

    fn get_all_users(&self) -> Result<Vec<User>, Box<dyn std::error::Error>> {
        Ok(self
            .users
            .iter()
            .map(|(email, stored)| to_user(email, stored))
            .collect())
    }

    fn delete_user(&mut self, user_email: &str) -> Result<(), Box<dyn std::error::Error>> {
        if self.users.remove(user_email).is_none() {
            return Err(Box::new(UserRegistryError::NotFound(
                user_email.to_string(),
            )));
        }
        self.save()?;
        Ok(())
    }

    fn update_user(
        &mut self,
        user_email: &str,
        user: &User,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.users.contains_key(user_email) {
            return Err(Box::new(UserRegistryError::NotFound(
                user_email.to_string(),
            )));
        }
        self.users.remove(user_email);
        self.users.insert(
            user.email.clone(),
            StoredUser {
                token: user.token.clone(),
                password_hash: user.password_hash.clone(),
            },
        );
        self.save()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn registry_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("users.json")
    }

    #[test]
    fn open_missing_file_is_empty() {
        let dir = tempdir().unwrap();
        let registry = JsonUserRegistry::open(registry_path(&dir)).unwrap();
        assert!(registry.get_all_users().unwrap().is_empty());
    }

    #[test]
    fn register_then_get_and_get_all() {
        let dir = tempdir().unwrap();
        let mut registry = JsonUserRegistry::open(registry_path(&dir)).unwrap();
        let user = User {
            email: "alice@example.com".to_string(),
            password_hash: None,
            token: "token-abc".to_string(),
        };
        registry.register_user(&user).unwrap();

        let fetched = registry.get_user_by_email("alice@example.com").unwrap();
        assert_eq!(fetched, user);

        let all = registry.get_all_users().unwrap();
        assert_eq!(all, vec![user]);
    }

    #[test]
    fn register_duplicate_email_errors() {
        let dir = tempdir().unwrap();
        let mut registry = JsonUserRegistry::open(registry_path(&dir)).unwrap();
        let user = User {
            email: "alice@example.com".to_string(),
            password_hash: None,
            token: "token-abc".to_string(),
        };
        registry.register_user(&user).unwrap();

        let err = registry.register_user(&user).unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn get_user_by_email_missing_errors() {
        let dir = tempdir().unwrap();
        let registry = JsonUserRegistry::open(registry_path(&dir)).unwrap();
        let err = registry
            .get_user_by_email("missing@example.com")
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn delete_user_removes_entry() {
        let dir = tempdir().unwrap();
        let mut registry = JsonUserRegistry::open(registry_path(&dir)).unwrap();
        let user = User {
            email: "alice@example.com".to_string(),
            password_hash: None,
            token: "token-abc".to_string(),
        };
        registry.register_user(&user).unwrap();
        registry.delete_user("alice@example.com").unwrap();

        assert!(registry.get_all_users().unwrap().is_empty());
    }

    #[test]
    fn delete_missing_user_errors() {
        let dir = tempdir().unwrap();
        let mut registry = JsonUserRegistry::open(registry_path(&dir)).unwrap();
        let err = registry.delete_user("missing@example.com").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn update_user_replaces_token() {
        let dir = tempdir().unwrap();
        let mut registry = JsonUserRegistry::open(registry_path(&dir)).unwrap();
        let user = User {
            email: "alice@example.com".to_string(),
            password_hash: None,
            token: "token-abc".to_string(),
        };
        registry.register_user(&user).unwrap();

        let updated = User {
            email: "alice@example.com".to_string(),
            password_hash: None,
            token: "token-xyz".to_string(),
        };
        registry.update_user("alice@example.com", &updated).unwrap();

        let fetched = registry.get_user_by_email("alice@example.com").unwrap();
        assert_eq!(fetched.token, "token-xyz");
    }

    #[test]
    fn update_missing_user_errors() {
        let dir = tempdir().unwrap();
        let mut registry = JsonUserRegistry::open(registry_path(&dir)).unwrap();
        let user = User {
            email: "alice@example.com".to_string(),
            password_hash: None,
            token: "token-abc".to_string(),
        };
        let err = registry
            .update_user("alice@example.com", &user)
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn round_trip_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = registry_path(&dir);
        {
            let mut registry = JsonUserRegistry::open(&path).unwrap();
            registry
                .register_user(&User {
                    email: "alice@example.com".to_string(),
                    password_hash: None,
                    token: "token-abc".to_string(),
                })
                .unwrap();
        }

        let reopened = JsonUserRegistry::open(&path).unwrap();
        let fetched = reopened.get_user_by_email("alice@example.com").unwrap();
        assert_eq!(fetched.token, "token-abc");
    }

    #[test]
    fn hash_password_produces_verifiable_argon2id_hash() {
        let hash = hash_password("hunter2".to_string()).unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("hunter2", &hash).unwrap());
        assert!(!verify_password("wrong-password", &hash).unwrap());
    }

    #[test]
    fn register_with_password_hash_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = registry_path(&dir);
        let hash = hash_password("hunter2".to_string()).unwrap();
        {
            let mut registry = JsonUserRegistry::open(&path).unwrap();
            registry
                .register_user(&User {
                    email: "alice@example.com".to_string(),
                    password_hash: Some(hash.clone()),
                    token: "token-abc".to_string(),
                })
                .unwrap();
        }

        let reopened = JsonUserRegistry::open(&path).unwrap();
        let fetched = reopened.get_user_by_email("alice@example.com").unwrap();
        assert_eq!(fetched.password_hash.as_ref(), Some(&hash));
    }

    #[test]
    fn register_without_password_omits_field_on_reload() {
        let dir = tempdir().unwrap();
        let path = registry_path(&dir);
        {
            let mut registry = JsonUserRegistry::open(&path).unwrap();
            registry
                .register_user(&User {
                    email: "bob@example.com".to_string(),
                    password_hash: None,
                    token: "token-xyz".to_string(),
                })
                .unwrap();
        }

        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("password_hash"));

        let reopened = JsonUserRegistry::open(&path).unwrap();
        let fetched = reopened.get_user_by_email("bob@example.com").unwrap();
        assert_eq!(fetched.password_hash, None);
    }
}
