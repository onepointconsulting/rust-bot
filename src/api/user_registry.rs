//! Simple JSON-file-backed registry mapping user emails to JWTs.
//!
//! Login and `generate-jwt-token` stamp the user email into the JWT `sub`
//! claim (see [`crate::security::jwt::Claims`]). This registry still maps
//! email → current token / password hash so operators can look up and rotate
//! credentials. Tokens are minted by `rust-bot generate-jwt-token` at
//! registration time and refreshed on each successful `/v1/login`. An
//! Argon2id password hash is stored alongside the token for login.
//!
//! The JSON file is the source of truth: [`JsonUserRegistry`] re-reads it on
//! every operation, so an operator may edit it by hand (for example to promote
//! a user to `"role": "admin"`) while the gateway is running. Writes re-read the
//! file first and only replace the affected entry, then save atomically, so a
//! hand edit is neither ignored nor overwritten by the next sign-in.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};

use crate::security::jwt::UserRole;
use crate::utils::fs::write_atomic;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub email: String,
    pub password_hash: Option<String>,
    pub token: String,
    /// Role stamped into every token minted for this user at login.
    pub role: UserRole,
}

/// On-disk representation of a single user entry, keyed by email in
/// [`JsonUserRegistry`]'s map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct StoredUser {
    token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password_hash: Option<String>,
    /// Absent in entries written before roles existed, which read as plain users.
    #[serde(default)]
    role: UserRole,
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
/// `{ "email": { "token": "...", "password_hash": "...", "role": "admin" } }`. `password_hash`
/// is omitted when absent (legacy entries registered before a password was
/// required).
///
/// The file is re-read on every lookup and before every write, so changes made
/// to it by hand while the process runs take effect immediately and are kept.
/// A registry built with [`JsonUserRegistry::empty`] has no file and lives
/// purely in memory.
pub struct JsonUserRegistry {
    /// Location of the JSON file; empty for the in-memory [`JsonUserRegistry::empty`] registry.
    path: PathBuf,
    /// Working copy of the registry. For a file-backed registry it is reloaded
    /// from disk before each write; it is only authoritative for the in-memory registry.
    users: HashMap<String, StoredUser>,
}

impl JsonUserRegistry {
    /// Create an in-memory registry that is not backed by any file.
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
        let users = load_users_from_disk(&path)?;
        Ok(Self { path, users })
    }

    /// Whether this registry has no backing file (see [`JsonUserRegistry::empty`]).
    fn is_in_memory(&self) -> bool {
        self.path.as_os_str().is_empty()
    }

    /// The current set of users: read fresh from disk for a file-backed
    /// registry, or a copy of the in-memory map otherwise.
    fn current_users(&self) -> Result<HashMap<String, StoredUser>, UserRegistryError> {
        if self.is_in_memory() {
            Ok(self.users.clone())
        } else {
            load_users_from_disk(&self.path)
        }
    }

    /// Replace the working copy with the current on-disk contents, so a
    /// following write only changes the entry it targets.
    fn refresh(&mut self) -> Result<(), UserRegistryError> {
        self.users = self.current_users()?;
        Ok(())
    }

    /// Write the working copy to the backing file atomically (see
    /// [`write_atomic`]), so readers and editors never see a half-written
    /// file and a private (`0600`) file keeps its permissions.
    fn save(&self) -> Result<(), UserRegistryError> {
        let contents = serde_json::to_string_pretty(&self.users)?;
        write_atomic(&self.path, contents.as_bytes())?;
        Ok(())
    }
}

/// Parse the user map stored at `path`. A missing file yields an empty map.
fn load_users_from_disk(path: &Path) -> Result<HashMap<String, StoredUser>, UserRegistryError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(serde_json::from_str(&contents)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(err) => Err(err.into()),
    }
}

/// Convert a public [`User`] into its on-disk entry (the email is the map key).
fn to_stored_user(user: &User) -> StoredUser {
    StoredUser {
        token: user.token.clone(),
        password_hash: user.password_hash.clone(),
        role: user.role,
    }
}

/// Convert an on-disk entry keyed by `email` into the public [`User`] shape.
fn to_user(email: &str, stored: &StoredUser) -> User {
    User {
        email: email.to_string(),
        password_hash: stored.password_hash.clone(),
        token: stored.token.clone(),
        role: stored.role,
    }
}

impl UserRegistry for JsonUserRegistry {
    fn register_user(&mut self, user: &User) -> Result<(), Box<dyn std::error::Error>> {
        self.refresh()?;
        if self.users.contains_key(&user.email) {
            return Err(Box::new(UserRegistryError::AlreadyExists(
                user.email.clone(),
            )));
        }
        self.users.insert(user.email.clone(), to_stored_user(user));
        self.save()?;
        Ok(())
    }

    fn get_user_by_email(&self, user_email: &str) -> Result<User, Box<dyn std::error::Error>> {
        self.current_users()?
            .get(user_email)
            .map(|stored| to_user(user_email, stored))
            .ok_or_else(|| {
                Box::new(UserRegistryError::NotFound(user_email.to_string()))
                    as Box<dyn std::error::Error>
            })
    }

    fn get_all_users(&self) -> Result<Vec<User>, Box<dyn std::error::Error>> {
        Ok(self
            .current_users()?
            .iter()
            .map(|(email, stored)| to_user(email, stored))
            .collect())
    }

    fn delete_user(&mut self, user_email: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.refresh()?;
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
        self.refresh()?;
        if self.users.remove(user_email).is_none() {
            return Err(Box::new(UserRegistryError::NotFound(
                user_email.to_string(),
            )));
        }
        self.users.insert(user.email.clone(), to_stored_user(user));
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
            role: UserRole::User,
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
            role: UserRole::User,
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
            role: UserRole::User,
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
            role: UserRole::User,
        };
        registry.register_user(&user).unwrap();

        let updated = User {
            email: "alice@example.com".to_string(),
            password_hash: None,
            token: "token-xyz".to_string(),
            role: UserRole::User,
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
            role: UserRole::User,
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
                    role: UserRole::User,
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
                    role: UserRole::User,
                })
                .unwrap();
        }

        let reopened = JsonUserRegistry::open(&path).unwrap();
        let fetched = reopened.get_user_by_email("alice@example.com").unwrap();
        assert_eq!(fetched.password_hash.as_ref(), Some(&hash));
    }

    #[test]
    fn role_persists_across_reopen_and_legacy_entries_read_as_user() {
        let dir = tempdir().unwrap();
        let path = registry_path(&dir);
        fs::write(
            &path,
            r#"{"old@example.com": {"token": "t1"}, "boss@example.com": {"token": "t2", "role": "admin"}}"#,
        )
        .unwrap();

        let reopened = JsonUserRegistry::open(&path).unwrap();
        assert_eq!(
            reopened.get_user_by_email("old@example.com").unwrap().role,
            UserRole::User
        );
        assert_eq!(
            reopened.get_user_by_email("boss@example.com").unwrap().role,
            UserRole::Admin
        );
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
                    role: UserRole::User,
                })
                .unwrap();
        }

        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("password_hash"));

        let reopened = JsonUserRegistry::open(&path).unwrap();
        let fetched = reopened.get_user_by_email("bob@example.com").unwrap();
        assert_eq!(fetched.password_hash, None);
    }

    /// Rewrite the role of `email` directly in the JSON file, as an operator
    /// editing `users.json` by hand would.
    fn edit_role_on_disk(path: &Path, email: &str, role: &str) {
        let mut document: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        document[email]["role"] = serde_json::Value::String(role.to_string());
        fs::write(path, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    }

    /// A password-less, non-admin user with the given email and token.
    fn plain_user(email: &str, token: &str) -> User {
        User {
            email: email.to_string(),
            password_hash: None,
            token: token.to_string(),
            role: UserRole::User,
        }
    }

    #[test]
    fn external_role_edit_is_seen_by_get_user_by_email() {
        let dir = tempdir().unwrap();
        let path = registry_path(&dir);
        let mut registry = JsonUserRegistry::open(&path).unwrap();
        registry
            .register_user(&plain_user("alice@example.com", "token-abc"))
            .unwrap();

        edit_role_on_disk(&path, "alice@example.com", "admin");

        let fetched = registry.get_user_by_email("alice@example.com").unwrap();
        assert_eq!(fetched.role, UserRole::Admin);
        let all = registry.get_all_users().unwrap();
        assert_eq!(all[0].role, UserRole::Admin);
    }

    #[test]
    fn update_user_preserves_external_edits_to_other_entries() {
        let dir = tempdir().unwrap();
        let path = registry_path(&dir);
        let mut registry = JsonUserRegistry::open(&path).unwrap();
        registry
            .register_user(&plain_user("alice@example.com", "token-a"))
            .unwrap();
        registry
            .register_user(&plain_user("bob@example.com", "token-b"))
            .unwrap();

        edit_role_on_disk(&path, "bob@example.com", "admin");
        registry
            .update_user(
                "alice@example.com",
                &plain_user("alice@example.com", "token-a2"),
            )
            .unwrap();

        let reopened = JsonUserRegistry::open(&path).unwrap();
        assert_eq!(
            reopened.get_user_by_email("bob@example.com").unwrap().role,
            UserRole::Admin
        );
        assert_eq!(
            reopened
                .get_user_by_email("alice@example.com")
                .unwrap()
                .token,
            "token-a2"
        );
    }

    #[test]
    fn register_user_sees_entries_added_on_disk() {
        let dir = tempdir().unwrap();
        let path = registry_path(&dir);
        let mut registry = JsonUserRegistry::open(&path).unwrap();
        fs::write(&path, r#"{"carol@example.com": {"token": "t"}}"#).unwrap();

        let err = registry
            .register_user(&plain_user("carol@example.com", "other"))
            .unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn save_leaves_no_temporary_file_behind() {
        let dir = tempdir().unwrap();
        let path = registry_path(&dir);
        let mut registry = JsonUserRegistry::open(&path).unwrap();
        registry
            .register_user(&plain_user("alice@example.com", "token-abc"))
            .unwrap();
        registry
            .update_user(
                "alice@example.com",
                &plain_user("alice@example.com", "token-xyz"),
            )
            .unwrap();

        let file_names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(file_names, vec![std::ffi::OsString::from("users.json")]);
    }

    #[test]
    fn in_memory_registry_works_without_a_file() {
        let mut registry = JsonUserRegistry::empty();
        assert!(registry.get_all_users().unwrap().is_empty());
        registry.users.insert(
            "alice@example.com".to_string(),
            to_stored_user(&plain_user("alice@example.com", "token-abc")),
        );
        assert_eq!(
            registry
                .get_user_by_email("alice@example.com")
                .unwrap()
                .token,
            "token-abc"
        );
    }
}
