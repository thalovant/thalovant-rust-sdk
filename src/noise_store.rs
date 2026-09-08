//! On-disk state for the v3 Noise handshake: this client's static key and the
//! hub keys it has pinned.
//!
//! Both live beside the SDK config file, so `XDG_CONFIG_HOME` and the Windows
//! `APPDATA` location are honored the same way, and both are written `0600`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::errors::{Result, ThalovantError};
use crate::identity::{assert_secure_secret_file, default_config_path};

/// This client's static X25519 private key, hex encoded.
///
/// It must persist. A hub pins it on first contact, so a client that
/// regenerates it looks like a different peer and is refused.
pub const NOISE_KEY_FILENAME: &str = "noise_key";

/// The hub static keys this client has pinned, as a JSON object keyed by hub
/// node id.
pub const NOISE_PINS_FILENAME: &str = "noise_pins.json";

/// Cached password-to-PSK derivations, as a JSON object keyed by hub node id.
///
/// The derivation is argon2id at 64 MiB and depends only on the password and
/// the hub's node id, both constant for the life of the pairing, so it is the
/// same answer every time. The in-memory cache on a transport only helps that
/// one object; this survives reconnects and restarts.
pub const NOISE_PSK_FILENAME: &str = "noise_psks.json";

/// Serializes the read-modify-write of the pin file, so two connections pinning
/// different hubs at once cannot lose one another's entry.
///
/// This coordinates threads in one process only. Two *processes* sharing a
/// state directory can still each read the map, decide, and commit -- and the
/// rename means the later commit wins, which can drop a pin the other just
/// added and make that hub look like first contact again. Closing that needs an
/// inter-process lock, which is a dependency this SDK does not carry today; the
/// static key is handled separately, with `create_new`, because losing that
/// race strands a client permanently rather than costing a re-pin.
static PIN_LOCK: Mutex<()> = Mutex::new(());

/// The directory holding the static key and the pin file.
pub fn noise_state_dir() -> Result<PathBuf> {
    let config = default_config_path()?;
    config
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| ThalovantError::InvalidIdentity("unable to resolve config directory".into()))
}

fn resolve_dir(dir: Option<&Path>) -> Result<PathBuf> {
    match dir {
        Some(path) => Ok(path.to_path_buf()),
        None => noise_state_dir(),
    }
}

/// Return this client's persistent static X25519 private key, generating and
/// storing one on first use.
///
/// The file is created `0600` and refused if it is group- or world-accessible,
/// matching how the SDK treats every other on-disk secret.
pub fn load_or_create_noise_key(dir: Option<&Path>) -> Result<[u8; 32]> {
    let dir = resolve_dir(dir)?;
    let path = dir.join(NOISE_KEY_FILENAME);

    let _guard = PIN_LOCK.lock().map_err(poisoned)?;

    match fs::read_to_string(&path) {
        Ok(raw) => {
            assert_secure_secret_file(&path, "Noise key file")?;
            let decoded = hex::decode(raw.trim()).map_err(|_| {
                ThalovantError::InvalidIdentity(format!(
                    "Noise key file {} is not a 32-byte hex key",
                    path.display()
                ))
            })?;
            let key: [u8; 32] = decoded.try_into().map_err(|_| {
                ThalovantError::InvalidIdentity(format!(
                    "Noise key file {} is not a 32-byte hex key",
                    path.display()
                ))
            })?;
            Ok(key)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0_u8; 32];
            rand::thread_rng().fill_bytes(&mut key);
            fs::create_dir_all(&dir)?;
            // create_new at the final path, not a rename: PIN_LOCK only covers
            // this process, so another process can be generating a key at the
            // same moment. Renaming over the destination would leave one of
            // them holding a key that is not the one on disk -- and the hub
            // pins what it was shown, so that client would be refused for good.
            // Losing the create means the other process won; read its key.
            match write_private_exclusive(&path, &hex::encode(key)) {
                Ok(()) => Ok(key),
                Err(ThalovantError::Io(err)) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    load_existing_noise_key(&path)
                }
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err.into()),
    }
}

/// Read a static key that is already on disk, enforcing its permissions.
fn load_existing_noise_key(path: &Path) -> Result<[u8; 32]> {
    assert_secure_secret_file(path, "Noise key file")?;
    let raw = fs::read_to_string(path)?;
    let decoded = hex::decode(raw.trim()).map_err(|_| {
        ThalovantError::InvalidIdentity(format!(
            "Noise key file {} is not a 32-byte hex key",
            path.display()
        ))
    })?;
    decoded.try_into().map_err(|_| {
        ThalovantError::InvalidIdentity(format!(
            "Noise key file {} is not a 32-byte hex key",
            path.display()
        ))
    })
}

/// The pinned hub static key for a node id, or `None` when this client has not
/// seen that hub before.
pub fn load_noise_pin(dir: Option<&Path>, node_id: &str) -> Result<Option<String>> {
    let _guard = PIN_LOCK.lock().map_err(poisoned)?;
    let (pins, _) = read_pins(dir)?;
    Ok(pins.get(node_id).cloned())
}

/// Record the hub static key for a node id on first contact.
pub fn save_noise_pin(dir: Option<&Path>, node_id: &str, public_key: &str) -> Result<()> {
    if node_id.trim().is_empty() || public_key.trim().is_empty() {
        return Ok(());
    }
    let _guard = PIN_LOCK.lock().map_err(poisoned)?;
    let (mut pins, path) = read_pins(dir)?;
    if pins
        .get(node_id)
        .is_some_and(|current| current == public_key)
    {
        return Ok(());
    }
    pins.insert(node_id.to_string(), public_key.to_string());
    write_pins(&path, &pins)
}

/// Enforce trust on first use: record the first key seen for a node id, and
/// refuse a later key that does not match it.
///
/// The read and the write happen in one critical section. Checking for a pin and
/// then writing it as separate steps is the race itself: two connections could
/// both see no pin, and the later one would overwrite the earlier decision.
///
/// A changed key means either the hub was reinstalled or another machine is
/// answering at this address. The SDK cannot tell those apart, so it refuses and
/// leaves clearing the pin ([`forget_noise_pin`]) as a deliberate act.
pub fn pin_hub_key(dir: Option<&Path>, node_id: &str, remote_static_key: &str) -> Result<()> {
    if remote_static_key.is_empty() {
        return Ok(());
    }
    let _guard = PIN_LOCK.lock().map_err(poisoned)?;
    let (mut pins, path) = read_pins(dir)?;
    match pins.get(node_id) {
        None => {
            pins.insert(node_id.to_string(), remote_static_key.to_string());
            write_pins(&path, &pins)
        }
        Some(pinned) if pinned == remote_static_key => Ok(()),
        Some(_) => Err(ThalovantError::Connection(
            "the hub's Noise static key changed. If the hub was not reinstalled or replaced, another machine may be answering at this address. If it was, drop the stale pin with forget_noise_pin and reconnect to trust the new key"
                .to_string(),
        )),
    }
}

/// Drop a pinned hub key.
///
/// Use it when a hub was deliberately reinstalled or replaced. A pin that stops
/// matching on its own is a failure to investigate, not one to clear.
pub fn forget_noise_pin(dir: Option<&Path>, node_id: &str) -> Result<()> {
    let _guard = PIN_LOCK.lock().map_err(poisoned)?;
    let (mut pins, path) = read_pins(dir)?;
    if pins.remove(node_id).is_none() {
        return Ok(());
    }
    write_pins(&path, &pins)
}

fn read_pins(dir: Option<&Path>) -> Result<(BTreeMap<String, String>, PathBuf)> {
    let path = resolve_dir(dir)?.join(NOISE_PINS_FILENAME);
    match fs::read_to_string(&path) {
        Ok(raw) => {
            let pins = serde_json::from_str(&raw).map_err(|err| {
                ThalovantError::InvalidIdentity(format!(
                    "Noise pin file {} is not a JSON object of node id to key: {err}",
                    path.display()
                ))
            })?;
            Ok((pins, path))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((BTreeMap::new(), path)),
        Err(err) => Err(err.into()),
    }
}

fn write_pins(path: &Path, pins: &BTreeMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut encoded = serde_json::to_string_pretty(pins)?;
    encoded.push('\n');
    write_private(path, &encoded)
}

/// Create a secret file at `path`, failing if it already exists.
///
/// Used where losing the race must be observable rather than silently
/// overwriting: whoever creates the file first owns the value.
fn write_private_exclusive(path: &Path, contents: &str) -> Result<()> {
    write_private_at(path, contents)
}

/// Write a secret file atomically: a uniquely named temporary file in the same
/// directory, created `0600`, then renamed into place.
///
/// Truncating the real file first would leave it empty or half-written if the
/// write failed, and an empty pin file reads back as "no pins" -- which makes
/// the next connection look like first contact and silently re-pin whatever key
/// it is offered.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let unique = format!(
        "{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("noise"),
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    );
    let temporary = directory.join(unique);

    let result = write_private_at(&temporary, contents)
        .and_then(|()| fs::rename(&temporary, path).map_err(ThalovantError::from));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Distinguishes temporary files created within one process; the pid alone does
/// not, because two threads can be writing at the same moment.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn write_private_at(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // create_new: never write through an existing file, so a stale temporary
    // cannot be reused and the mode is always applied at creation.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Windows has no mode bits to set at creation, and the SDK's permission check
/// is Unix-only for the same reason.
#[cfg(not(unix))]
fn write_private_at(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn poisoned<T>(_: T) -> ThalovantError {
    ThalovantError::Connection("the Noise state lock was poisoned by a panic".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_hub_key_records_then_refuses_a_changed_key() {
        let dir = tempdir();
        let first = "aa".repeat(32);
        let second = "bb".repeat(32);

        pin_hub_key(Some(&dir), "hub", &first).unwrap();
        // The same key again is a normal reconnect.
        pin_hub_key(Some(&dir), "hub", &first).unwrap();

        let error = pin_hub_key(Some(&dir), "hub", &second)
            .expect_err("a changed hub key was accepted; pinning gives no protection")
            .to_string();
        assert!(
            error.contains("forget_noise_pin"),
            "the refusal does not tell the operator how to proceed: {error}"
        );
        assert_eq!(
            load_noise_pin(Some(&dir), "hub").unwrap().as_deref(),
            Some(first.as_str()),
            "the stored pin was overwritten by the rejected key"
        );
    }

    #[test]
    fn forgetting_one_hub_keeps_the_others() {
        let dir = tempdir();
        pin_hub_key(Some(&dir), "hub-one", &"11".repeat(32)).unwrap();
        pin_hub_key(Some(&dir), "hub-two", &"22".repeat(32)).unwrap();

        forget_noise_pin(Some(&dir), "hub-one").unwrap();

        assert_eq!(load_noise_pin(Some(&dir), "hub-one").unwrap(), None);
        assert_eq!(
            load_noise_pin(Some(&dir), "hub-two").unwrap().as_deref(),
            Some("22".repeat(32).as_str())
        );
    }

    /// The create race is closed by `create_new` semantics: the second writer
    /// must be told the file exists rather than replacing it, so
    /// `load_or_create_noise_key` can reload the winner's key.
    ///
    /// This pins that primitive. The full race needs the file to be absent at
    /// the read and present at the create, which one process cannot stage
    /// without injecting a delay into the function under test -- so what is
    /// asserted here is the property the fix rests on, not the interleaving.
    #[test]
    fn creating_a_static_key_never_replaces_an_existing_one() {
        let dir = tempdir();
        let path = dir.join(NOISE_KEY_FILENAME);
        let theirs = [7_u8; 32];

        write_private_exclusive(&path, &hex::encode(theirs)).unwrap();

        let error = write_private_exclusive(&path, &hex::encode([9_u8; 32]))
            .expect_err("a second create replaced the existing static key");
        match error {
            ThalovantError::Io(err) => assert_eq!(
                err.kind(),
                std::io::ErrorKind::AlreadyExists,
                "the caller cannot tell it lost the race: {err}"
            ),
            other => panic!("expected an AlreadyExists io error, got {other}"),
        }

        // And the winner's key is what a subsequent load returns.
        assert_eq!(load_or_create_noise_key(Some(&dir)).unwrap(), theirs);
    }

    #[test]
    fn the_static_key_persists_and_stays_private() {
        let dir = tempdir();
        let first = load_or_create_noise_key(Some(&dir)).unwrap();
        let second = load_or_create_noise_key(Some(&dir)).unwrap();
        assert_eq!(
            first, second,
            "a second call generated a new static key; every connection would look like a new peer"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(NOISE_KEY_FILENAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "the static key file is group or world accessible: {mode:o}"
            );
        }
    }

    /// A unique directory under the system temp dir; removed on the next run of
    /// the same name rather than tracked, which keeps this dependency-free.
    fn tempdir() -> PathBuf {
        let unique = format!(
            "thalovant-noise-store-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

/// One cached derivation: the key, and a fingerprint of the password it came
/// from.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CachedPsk {
    psk: String,
    verifier: String,
}

/// Fingerprint the password a cached PSK was derived from, so a rotated
/// password is noticed and re-derived rather than offered to the hub -- which
/// refuses it exactly as it refuses a wrong password.
///
/// A hash, never the password, and it never leaves the machine. The identity
/// file on the same host already holds the password itself, so this adds no
/// exposure that was not already there.
pub fn psk_password_verifier(password: &str) -> String {
    let digest = Sha256::digest(format!("thalovant-psk-verifier:{password}").as_bytes());
    hex::encode(digest)
}

fn read_psk_cache(dir: Option<&Path>) -> Result<(BTreeMap<String, CachedPsk>, PathBuf)> {
    let path = resolve_dir(dir)?.join(NOISE_PSK_FILENAME);
    match fs::read_to_string(&path) {
        Ok(raw) => {
            assert_secure_secret_file(&path, "Noise PSK cache")?;
            // A corrupt cache is derivable state, not a reason to fail a
            // connection: drop it and pay the derivation once.
            Ok((serde_json::from_str(&raw).unwrap_or_default(), path))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((BTreeMap::new(), path)),
        Err(err) => Err(err.into()),
    }
}

/// The cached PSK for a hub, or `None` when absent or derived from another
/// password.
pub fn load_cached_psk(
    dir: Option<&Path>,
    node_id: &str,
    verifier: &str,
) -> Result<Option<[u8; 32]>> {
    if node_id.trim().is_empty() {
        return Ok(None);
    }
    let _guard = PIN_LOCK.lock().map_err(poisoned)?;
    let (cache, _) = read_psk_cache(dir)?;
    let Some(entry) = cache.get(node_id) else {
        return Ok(None);
    };
    if entry.verifier != verifier {
        return Ok(None);
    }
    let Ok(raw) = hex::decode(entry.psk.trim()) else {
        return Ok(None);
    };
    Ok(<[u8; 32]>::try_from(raw.as_slice()).ok())
}

/// Record a derived PSK so the next connection to this hub skips argon2id.
pub fn save_cached_psk(
    dir: Option<&Path>,
    node_id: &str,
    psk: &[u8; 32],
    verifier: &str,
) -> Result<()> {
    if node_id.trim().is_empty() {
        return Ok(());
    }
    let _guard = PIN_LOCK.lock().map_err(poisoned)?;
    let (mut cache, path) = read_psk_cache(dir)?;
    let encoded = hex::encode(psk);
    if cache
        .get(node_id)
        .is_some_and(|entry| entry.psk == encoded && entry.verifier == verifier)
    {
        return Ok(());
    }
    cache.insert(
        node_id.to_string(),
        CachedPsk {
            psk: encoded,
            verifier: verifier.to_string(),
        },
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut serialized = serde_json::to_string_pretty(&cache)?;
    serialized.push('\n');
    write_private(&path, &serialized)
}
