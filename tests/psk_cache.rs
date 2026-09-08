//! The PSK cache's correctness properties.
//!
//! A cached key that is wrong by one byte fails the handshake exactly as a
//! wrong password does, so none of this surfaces as a crash -- it surfaces as
//! an outage that looks like bad credentials. That is what these cover.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use thalovant::{
    derive_psk, load_cached_psk, psk_password_verifier, save_cached_psk, NOISE_PSK_FILENAME,
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

#[test]
fn cached_psk_round_trips() {
    let dir = StateDir::new();
    let psk = derive_psk("hunter2", NODE_ID).expect("derive");
    let verifier = psk_password_verifier("hunter2");

    assert!(load_cached_psk(Some(dir.path()), NODE_ID, &verifier)
        .expect("load")
        .is_none());

    save_cached_psk(Some(dir.path()), NODE_ID, &psk, &verifier).expect("save");
    let loaded = load_cached_psk(Some(dir.path()), NODE_ID, &verifier).expect("load");
    assert_eq!(loaded, Some(psk));
}

#[test]
fn rotated_password_is_not_served_from_cache() {
    let dir = StateDir::new();
    let old = derive_psk("old-password", NODE_ID).expect("derive");
    save_cached_psk(
        Some(dir.path()),
        NODE_ID,
        &old,
        &psk_password_verifier("old-password"),
    )
    .expect("save");

    assert!(
        load_cached_psk(
            Some(dir.path()),
            NODE_ID,
            &psk_password_verifier("new-password")
        )
        .expect("load")
        .is_none(),
        "a rotated password must not reuse the previous PSK"
    );
    assert!(load_cached_psk(
        Some(dir.path()),
        NODE_ID,
        &psk_password_verifier("old-password")
    )
    .expect("load")
    .is_some());
}

#[test]
fn cache_file_never_holds_the_password() {
    let dir = StateDir::new();
    let password = "a-very-distinctive-password-9931";
    let psk = derive_psk(password, NODE_ID).expect("derive");
    save_cached_psk(
        Some(dir.path()),
        NODE_ID,
        &psk,
        &psk_password_verifier(password),
    )
    .expect("save");

    let raw = fs::read_to_string(dir.path().join(NOISE_PSK_FILENAME)).expect("read");
    assert!(
        !raw.contains(password),
        "the verifier must not be reversible to the password"
    );
}

#[test]
fn corrupt_cache_is_discarded_rather_than_failing() {
    let dir = StateDir::new();
    let verifier = psk_password_verifier("hunter2");
    let psk = derive_psk("hunter2", NODE_ID).expect("derive");
    save_cached_psk(Some(dir.path()), NODE_ID, &psk, &verifier).expect("save");

    fs::write(dir.path().join(NOISE_PSK_FILENAME), "{ not json").expect("corrupt");
    assert!(load_cached_psk(Some(dir.path()), NODE_ID, &verifier)
        .expect("load")
        .is_none());

    // and it recovers
    save_cached_psk(Some(dir.path()), NODE_ID, &psk, &verifier).expect("resave");
    assert_eq!(
        load_cached_psk(Some(dir.path()), NODE_ID, &verifier).expect("load"),
        Some(psk)
    );
}
