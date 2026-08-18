//! Pairing store for DM sender approval.
//!
//! Persistent storage at `<data_dir>/pairing.json` keeps pending pairing
//! codes (and, on disk, an `approved` map reserved for future use — see
//! nanobot's `nanobot/pairing/store.py`, which this is a partial port of).
//! Designed for private-assistant scale: small JSON file, simple locking,
//! no external DB.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};

use crate::config::paths::get_data_dir;
use crate::utils::helpers::write_text_atomic;

/// Guards the load-mutate-save critical section (`threading.Lock` equivalent).
/// At private-assistant scale (small JSON file, sub-millisecond operations)
/// a brief block is acceptable, mirroring nanobot's own comment.
static PAIRING_LOCK: Mutex<()> = Mutex::new(());

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const CODE_LENGTH: usize = 8; // e.g. ABCD-EFGH
const TTL_DEFAULT_S: u64 = 600; // 10 minutes

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PendingEntry {
    channel: String,
    sender_id: String,
    created_at: f64,
    expires_at: f64,
}

/// On-disk shape. `approved` round-trips losslessly even though nothing in
/// this module reads/writes it yet — `approve_code`/`is_approved` etc. are
/// a follow-up port; losing existing approvals on a `generate_code` call
/// would be a real regression once they exist.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct PairingData {
    #[serde(default)]
    approved: HashMap<String, Vec<String>>,
    #[serde(default)]
    pending: HashMap<String, PendingEntry>,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn store_path() -> PathBuf {
    get_data_dir().join("pairing.json")
}

fn load_from(path: &Path) -> PairingData {
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|e| {
            log::warn!("Corrupted pairing store, resetting: {e}");
            PairingData::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PairingData::default(),
        Err(e) => {
            log::warn!("Corrupted pairing store, resetting: {e}");
            PairingData::default()
        }
    }
}

fn save_to(path: &Path, data: &PairingData) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let contents = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
    write_text_atomic(path, &contents).map_err(|e| e.to_string())
}

/// Remove expired pending entries in-place. Mirrors `_gc_pending`; simpler
/// than the Python version since `serde` already enforces the entry shape
/// on load (a malformed entry just fails deserialization and resets the
/// whole store, rather than being pruned field-by-field).
fn gc_pending(data: &mut PairingData) {
    let now = now_secs();
    data.pending.retain(|_, entry| entry.expires_at >= now);
}

fn generate_code_at(path: &Path, channel: &str, sender_id: &str, ttl_s: u64) -> String {
    let _guard = PAIRING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut data = load_from(path);
    gc_pending(&mut data);

    let mut rng = rand::rng();
    let raw: String = (0..CODE_LENGTH)
        .map(|_| *ALPHABET.choose(&mut rng).expect("ALPHABET is non-empty") as char)
        .collect();
    let code = format!("{}-{}", &raw[..4], &raw[4..]);

    let now = now_secs();
    data.pending.insert(
        code.clone(),
        PendingEntry {
            channel: channel.to_string(),
            sender_id: sender_id.to_string(),
            created_at: now,
            expires_at: now + ttl_s as f64,
        },
    );

    if let Err(e) = save_to(path, &data) {
        log::error!("Failed to save pairing store: {e}");
    }
    log::info!("Generated pairing code {code} for {sender_id}@{channel}");
    code
}

/// Create a new pairing code for `sender_id` on `channel`, with the default
/// 10-minute TTL. Returns the code (e.g. `"ABCD-EFGH"`).
///
/// A failure to persist the pending entry is logged, not propagated — the
/// code is still returned (matching nanobot's fire-and-forget signature),
/// but in that case approving it later will report "invalid or expired".
pub fn generate_code(channel: &str, sender_id: &str) -> String {
    generate_code_at(&store_path(), channel, sender_id, TTL_DEFAULT_S)
}

/// [`generate_code`] with a caller-supplied TTL, in seconds.
pub fn generate_code_with_ttl(channel: &str, sender_id: &str, ttl_s: u64) -> String {
    generate_code_at(&store_path(), channel, sender_id, ttl_s)
}

/// Metadata key used to tag outbound pairing-code replies. Mirrors nanobot's
/// `PAIRING_CODE_META_KEY` (`nanobot/pairing/__init__.py:19`).
pub const PAIRING_CODE_META_KEY: &str = "_pairing_code";

/// Return the pairing-code message sent to unrecognized DM senders. Mirrors
/// nanobot's `format_pairing_reply`.
pub fn format_pairing_reply(code: &str) -> String {
    format!(
        "Hi there! This assistant only responds to approved users.\n\n\
         Your pairing code is: `{code}`\n\n\
         To get access, ask the owner to approve this request (`/pairing approve {code}`)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_code_returns_hyphenated_eight_char_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let code = generate_code_at(&path, "email", "user@example.com", TTL_DEFAULT_S);

        assert_eq!(code.len(), 9, "expected XXXX-XXXX, got: {code}");
        let (first, second) = code.split_at(4);
        assert_eq!(&second[..1], "-");
        let chars: String = first.chars().chain(second[1..].chars()).collect();
        assert!(
            chars
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "unexpected characters in code: {code}"
        );
    }

    #[test]
    fn generate_code_persists_pending_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let code = generate_code_at(&path, "email", "user@example.com", TTL_DEFAULT_S);

        let data = load_from(&path);
        let entry = data.pending.get(&code).expect("pending entry should exist");
        assert_eq!(entry.channel, "email");
        assert_eq!(entry.sender_id, "user@example.com");
    }

    #[test]
    fn generate_code_uses_requested_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        let code = generate_code_at(&path, "email", "user@example.com", 42);

        let data = load_from(&path);
        let entry = data.pending.get(&code).unwrap();
        assert!((entry.expires_at - entry.created_at - 42.0).abs() < 0.01);
    }

    #[test]
    fn gc_pending_removes_only_expired_entries() {
        let mut data = PairingData::default();
        let now = now_secs();
        data.pending.insert(
            "EXPIRED-1".to_string(),
            PendingEntry {
                channel: "email".to_string(),
                sender_id: "a".to_string(),
                created_at: now - 100.0,
                expires_at: now - 1.0,
            },
        );
        data.pending.insert(
            "LIVE-1".to_string(),
            PendingEntry {
                channel: "email".to_string(),
                sender_id: "b".to_string(),
                created_at: now,
                expires_at: now + 100.0,
            },
        );

        gc_pending(&mut data);

        assert!(!data.pending.contains_key("EXPIRED-1"));
        assert!(data.pending.contains_key("LIVE-1"));
    }

    #[test]
    fn load_from_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");

        let data = load_from(&path);
        assert!(data.pending.is_empty());
        assert!(data.approved.is_empty());
    }

    #[test]
    fn load_from_corrupted_file_resets_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");
        std::fs::write(&path, b"not valid json").unwrap();

        let data = load_from(&path);
        assert!(data.pending.is_empty());
        assert!(data.approved.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip_preserves_approved_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pairing.json");

        let mut data = PairingData::default();
        data.approved
            .insert("email".to_string(), vec!["alice@example.com".to_string()]);
        save_to(&path, &data).unwrap();

        // generate_code_at loads, mutates only `pending`, and saves again —
        // `approved` must survive that round-trip untouched.
        generate_code_at(&path, "email", "bob@example.com", TTL_DEFAULT_S);

        let reloaded = load_from(&path);
        assert_eq!(
            reloaded.approved.get("email"),
            Some(&vec!["alice@example.com".to_string()])
        );
    }

    #[test]
    fn format_pairing_reply_contains_code_and_approve_command() {
        let reply = format_pairing_reply("ABCD-EFGH");
        assert!(reply.contains("ABCD-EFGH"));
        assert!(reply.contains("/pairing approve ABCD-EFGH"));
    }
}
