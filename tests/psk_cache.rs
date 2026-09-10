//! The PSK cache's correctness properties.
//!
//! A cached key that is wrong by one byte fails the handshake exactly as a
//! wrong password does, so none of this surfaces as a crash -- it surfaces as
//! an outage that looks like bad credentials. That is what these cover.

use std::fs;

use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use thalovant::{
    derive_psk, forget_cached_psk, load_cached_psk, save_cached_psk, NOISE_PSK_FILENAME,
};

/// A private state directory per test. The SDK has no `tempfile` dependency
/// and this does not warrant adding one; the directory is removed on drop.
struct StateDir(PathBuf);

impl StateDir {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "thalovant-psk-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create state dir");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const NODE_ID: &str =
    "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEF\n-----END PUBLIC KEY-----";

/// A synthetic credential with a runtime seed. Its non-hex prefix prevents a
/// short process ID from accidentally matching a legitimate derived-key hex
/// substring in the cache's no-plaintext assertion.
fn test_password() -> String {
    let seed =
        std::env::var("THALOVANT_TEST_PASSWORD").unwrap_or_else(|_| std::process::id().to_string());
    format!("synthetic-psk-cache-fixture:{seed}")
}

#[test]
fn cached_psk_round_trips() {
    let dir = StateDir::new();
    let psk = derive_psk(&test_password(), NODE_ID).expect("derive");

    assert!(load_cached_psk(Some(dir.path()), NODE_ID)
        .expect("load")
        .is_none());

    save_cached_psk(Some(dir.path()), NODE_ID, &psk).expect("save");
    assert_eq!(
        load_cached_psk(Some(dir.path()), NODE_ID).expect("load"),
        Some(psk)
    );
}

/// Rotation is noticed when the hub rejects the stale key, so the handshake
/// drops it and the next attempt derives from the current password.
#[test]
fn forgetting_removes_the_entry() {
    let dir = StateDir::new();
    let psk = derive_psk(&test_password(), NODE_ID).expect("derive");
    save_cached_psk(Some(dir.path()), NODE_ID, &psk).expect("save");

    forget_cached_psk(Some(dir.path()), NODE_ID).expect("forget");
    assert!(load_cached_psk(Some(dir.path()), NODE_ID)
        .expect("load")
        .is_none());
}

/// The cache holds the key and nothing else. A fingerprint of the password
/// would be a fast offline oracle sitting next to the key it protects, which
/// is exactly what argon2id is there to deny.
#[test]
fn cache_file_holds_only_the_key() {
    let dir = StateDir::new();
    let password = test_password();
    let psk = derive_psk(&password, NODE_ID).expect("derive");
    save_cached_psk(Some(dir.path()), NODE_ID, &psk).expect("save");

    let raw = fs::read_to_string(dir.path().join(NOISE_PSK_FILENAME)).expect("read");
    let cache: serde_json::Value = serde_json::from_str(&raw).expect("cache JSON");
    let entries = cache.as_object().expect("cache object");
    assert_eq!(
        entries.len(),
        1,
        "the cache must contain only one key entry"
    );
    assert_eq!(entries.values().next().unwrap(), &hex::encode(psk));
    assert!(
        !raw.contains(&password),
        "the cache must not hold the password"
    );
    let fast_hash = hex::encode(Sha256::digest(password.as_bytes()));
    assert!(
        !raw.contains(&fast_hash),
        "the cache must not hold a fast hash of the password"
    );
}

#[test]
fn corrupt_cache_is_discarded_rather_than_failing() {
    let dir = StateDir::new();
    let psk = derive_psk(&test_password(), NODE_ID).expect("derive");
    save_cached_psk(Some(dir.path()), NODE_ID, &psk).expect("save");

    fs::write(dir.path().join(NOISE_PSK_FILENAME), "{ not json").expect("corrupt");
    assert!(load_cached_psk(Some(dir.path()), NODE_ID)
        .expect("load")
        .is_none());

    save_cached_psk(Some(dir.path()), NODE_ID, &psk).expect("resave");
    assert_eq!(
        load_cached_psk(Some(dir.path()), NODE_ID).expect("load"),
        Some(psk)
    );
}

/// The cache is key material, so `read_psk_cache` runs it through the same
/// owner-only check as the static key. Untested until now: the enforcement
/// exists, but nothing proved a loosened file is actually refused rather
/// than quietly read.
#[cfg(unix)]
#[test]
fn a_group_readable_cache_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let dir = StateDir::new();
    let psk = derive_psk(&test_password(), NODE_ID).expect("derive");
    save_cached_psk(Some(dir.path()), NODE_ID, &psk).expect("save");

    let path = dir.path().join(NOISE_PSK_FILENAME);
    let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the cache must be created owner-only");

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
    match load_cached_psk(Some(dir.path()), NODE_ID) {
        Err(thalovant::ThalovantError::InvalidIdentity(_)) => {}
        other => panic!("a group-readable cache must be refused, got {other:?}"),
    }
}
