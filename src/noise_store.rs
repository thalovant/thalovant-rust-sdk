//! On-disk state for the v3 Noise handshake: this client's static key and the
//! hub keys it has pinned, and the derived PSK credential cache.
//!
//! All three live beside the SDK config file, so `XDG_CONFIG_HOME` and the Windows
//! `APPDATA` location are honored the same way. New files use `0600` on Unix;
//! Windows files inherit the configuration directory's access controls.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use rand::RngCore;

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

/// Cached pre-shared keys, as a JSON object keyed by hub node id.
///
/// The derivation is argon2id at 64 MiB and depends only on the password and
/// the hub's node id, both constant for the life of the pairing, so it is the
/// same answer every time. The in-memory cache on a transport only helps that
/// one object; this survives reconnects and restarts.
///
/// Only the key is stored. A fingerprint of the password would make rotation
/// cheap to detect, but it would also put a fast hash of the password in the
/// same file as the key it protects -- and a fast hash is exactly the offline
/// oracle argon2id exists to deny.
pub const NOISE_PSK_FILENAME: &str = "noise_psks.json";

/// Serializes the read-modify-write of the pin file, so two connections pinning
/// different hubs at once cannot lose one another's entry.
///
/// Each operation also holds an OS file lock, released even when its process
/// exits. Keep the lock file in place: removing it could split concurrent
/// callers across different inodes and lose the transaction boundary.
static PIN_LOCK: Mutex<()> = Mutex::new(());

struct StateGuard {
    _thread: MutexGuard<'static, ()>,
    _file: fs::File,
}

fn lock_state(dir: Option<&Path>) -> Result<StateGuard> {
    lock_state_with_timeout(dir, std::time::Duration::from_secs(5))
}

fn lock_state_with_timeout(dir: Option<&Path>, budget: std::time::Duration) -> Result<StateGuard> {
    let deadline = std::time::Instant::now() + budget;
    let thread = loop {
        match PIN_LOCK.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(error)) => return Err(poisoned(error)),
            Err(std::sync::TryLockError::WouldBlock) => wait_for_lock(deadline)?,
        }
    };
    let directory = resolve_dir(dir)?;
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(&directory)?;
    let path = directory.join(".noise.lock");
    if let Ok(metadata) = path.symlink_metadata() {
        if !metadata.file_type().is_file() {
            return Err(ThalovantError::InvalidIdentity(
                "Noise state lock must be a regular file".into(),
            ));
        }
        assert_secure_secret_file(&path, "Noise state lock")?;
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    assert_secure_secret_file(&path, "Noise state lock")?;
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                wait_for_lock(deadline)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(StateGuard {
        _thread: thread,
        _file: file,
    })
}

fn wait_for_lock(deadline: std::time::Instant) -> Result<()> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(ThalovantError::Timeout(
            "Noise state lock acquisition timed out".into(),
        ));
    }
    std::thread::sleep(remaining.min(std::time::Duration::from_millis(10)));
    Ok(())
}

/// Reject unexpected file types before reading any persisted trust or credentials.
fn validate_state_file(path: &Path, label: &str) -> Result<()> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_file() => assert_secure_secret_file(path, label),
        Ok(_) => Err(ThalovantError::InvalidIdentity(format!(
            "{label} must be a regular file"
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

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

    let _guard = lock_state(Some(&dir))?;

    validate_state_file(&path, "Noise key file")?;
    match fs::read_to_string(&path) {
        Ok(raw) => {
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
            // Publish only complete, flushed bytes without replacing a winner.
            // A killed writer leaves an untrusted temporary file, never a
            // partially written static identity at the trusted path.
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
    validate_state_file(path, "Noise key file")?;
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
    let _guard = lock_state(dir)?;
    let (pins, _) = read_pins(dir)?;
    Ok(pins.get(node_id).cloned())
}

/// Record the hub static key for a node id on first contact.
/// Repeating the same key succeeds; a different key requires verified rotation
/// through [`forget_noise_pin`]. Malformed saved trust is never overwritten.
pub fn save_noise_pin(dir: Option<&Path>, node_id: &str, public_key: &str) -> Result<()> {
    pin_hub_key(dir, node_id, public_key)
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
    validate_pin(node_id, remote_static_key)?;
    let _guard = lock_state(dir)?;
    let (mut pins, path) = read_pins(dir)?;
    match pins.get(node_id) {
        None => {
            pins.insert(node_id.to_string(), remote_static_key.to_string());
            write_pins(&path, &pins)
        }
        Some(pinned) if pinned.eq_ignore_ascii_case(remote_static_key) => Ok(()),
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
    let _guard = lock_state(dir)?;
    let (mut pins, path) = read_pins(dir)?;
    if pins.remove(node_id).is_none() {
        return Ok(());
    }
    write_pins(&path, &pins)
}

fn read_pins(dir: Option<&Path>) -> Result<(BTreeMap<String, String>, PathBuf)> {
    let path = resolve_dir(dir)?.join(NOISE_PINS_FILENAME);
    validate_state_file(&path, "Noise pin file")?;
    match fs::read_to_string(&path) {
        Ok(raw) => {
            let pins: BTreeMap<String, String> = serde_json::from_str(&raw).map_err(|err| {
                ThalovantError::InvalidIdentity(format!(
                    "Noise pin file {} is not a JSON object of node id to key: {err}",
                    path.display()
                ))
            })?;
            for (node_id, key) in &pins {
                validate_pin(node_id, key)?;
            }
            Ok((pins, path))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((BTreeMap::new(), path)),
        Err(err) => Err(err.into()),
    }
}

fn validate_pin(node_id: &str, key: &str) -> Result<()> {
    if node_id.trim().is_empty()
        || key.len() != 64
        || !key.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ThalovantError::InvalidIdentity(
            "Noise pin requires a nonempty node id and a 32-byte hexadecimal static key".into(),
        ));
    }
    Ok(())
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
    publish_private(path, contents, true, write_private_at)
}

/// Write a secret file atomically: a uniquely named temporary file in the same
/// directory, created `0600`, then renamed into place.
///
/// Truncating the real file first would leave it empty or half-written if the
/// write failed, and an empty pin file reads back as "no pins" -- which makes
/// the next connection look like first contact and silently re-pin whatever key
/// it is offered.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    publish_private(path, contents, false, write_private_at)
}

fn publish_private(
    path: &Path,
    contents: &str,
    exclusive: bool,
    write: impl FnOnce(&Path, &str) -> Result<()>,
) -> Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let unique = format!(
        "{}.{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("noise"),
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
        uuid::Uuid::new_v4(),
    );
    let temporary = directory.join(unique);

    // A staging collision is not a lost publication race and does not give
    // this caller ownership of the existing staging file.
    match write(&temporary, contents) {
        Err(ThalovantError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ThalovantError::Io(std::io::Error::other(
                "Noise staging path already exists; retry publication",
            )));
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(()) => {}
    }
    let result = if exclusive {
        fs::hard_link(&temporary, path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists { ThalovantError::Io(error) }
            else { ThalovantError::Io(std::io::Error::new(error.kind(), format!("atomic Noise key publication requires a filesystem supporting hard links: {error}"))) }
        })
    } else {
        fs::rename(&temporary, path).map_err(ThalovantError::from)
    };
    let _ = fs::remove_file(&temporary);
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

/// The cached pre-shared key for a hub, or `None` when there is none.
pub fn load_cached_psk(dir: Option<&Path>, node_id: &str) -> Result<Option<[u8; 32]>> {
    if node_id.trim().is_empty() {
        return Ok(None);
    }
    let _guard = lock_state(dir)?;
    let (cache, _) = read_psk_cache(dir)?;
    let Some(encoded) = cache.get(node_id) else {
        return Ok(None);
    };
    let Ok(raw) = hex::decode(encoded.trim()) else {
        return Ok(None);
    };
    Ok(<[u8; 32]>::try_from(raw.as_slice()).ok())
}

/// Record a derived key so the next connection to this hub skips argon2id.
pub fn save_cached_psk(dir: Option<&Path>, node_id: &str, psk: &[u8; 32]) -> Result<()> {
    if node_id.trim().is_empty() {
        return Ok(());
    }
    let _guard = lock_state(dir)?;
    let (mut cache, path) = read_psk_cache(dir)?;
    let encoded = hex::encode(psk);
    if cache
        .get(node_id)
        .is_some_and(|current| current == &encoded)
    {
        return Ok(());
    }
    cache.insert(node_id.to_string(), encoded);
    write_psk_cache(&path, &cache)
}

/// Drop a stored key.
///
/// The handshake calls this when the hub rejects the key we offered, which is
/// how a password rotated elsewhere is noticed: the next attempt derives again.
pub fn forget_cached_psk(dir: Option<&Path>, node_id: &str) -> Result<()> {
    let _guard = lock_state(dir)?;
    let (mut cache, path) = read_psk_cache(dir)?;
    if cache.remove(node_id).is_none() {
        return Ok(());
    }
    write_psk_cache(&path, &cache)
}

fn read_psk_cache(dir: Option<&Path>) -> Result<(BTreeMap<String, String>, PathBuf)> {
    let path = resolve_dir(dir)?.join(NOISE_PSK_FILENAME);
    validate_state_file(&path, "Noise PSK cache")?;
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

fn write_psk_cache(path: &Path, cache: &BTreeMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut serialized = serde_json::to_string_pretty(cache)?;
    serialized.push('\n');
    write_private(path, &serialized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_save_preserves_first_contact_until_explicit_forget() {
        let dir = tempdir();
        let first = "ab".repeat(32);
        let second = "cd".repeat(32);
        save_noise_pin(Some(&dir), "hub", &first).unwrap();
        let path = dir.join(NOISE_PINS_FILENAME);
        let original = fs::read(&path).unwrap();
        save_noise_pin(Some(&dir), "hub", &first).unwrap();
        assert!(matches!(
            save_noise_pin(Some(&dir), "hub", &second),
            Err(ThalovantError::Connection(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        forget_noise_pin(Some(&dir), "hub").unwrap();
        save_noise_pin(Some(&dir), "hub", &second).unwrap();
        assert_eq!(load_noise_pin(Some(&dir), "hub").unwrap(), Some(second));
    }

    #[test]
    fn malformed_saved_pins_are_rejected_without_rewriting() {
        for raw in [
            "null".to_string(),
            "[]".to_string(),
            r#"{"hub":null}"#.to_string(),
            r#"{"hub":""}"#.to_string(),
            r#"{"hub":" "}"#.to_string(),
            r#"{"hub":"aa"}"#.to_string(),
            format!(r#"{{"hub":"{}"}}"#, "gg".repeat(32)),
        ] {
            let dir = tempdir();
            let path = dir.join(NOISE_PINS_FILENAME);
            write_private(&path, &raw).unwrap();
            assert!(
                matches!(
                    load_noise_pin(Some(&dir), "hub"),
                    Err(ThalovantError::InvalidIdentity(_))
                ),
                "invalid stored pin accepted: {raw}"
            );
            for result in [
                save_noise_pin(Some(&dir), "hub", &"ab".repeat(32)),
                pin_hub_key(Some(&dir), "hub", &"ab".repeat(32)),
                forget_noise_pin(Some(&dir), "hub"),
            ] {
                assert!(matches!(result, Err(ThalovantError::InvalidIdentity(_))));
                assert_eq!(fs::read_to_string(&path).unwrap(), raw);
            }
        }
    }

    #[test]
    fn invalid_pin_inputs_cannot_create_trust() {
        for (node_id, key) in [
            ("hub", "".to_string()),
            ("hub", " ".to_string()),
            ("hub", "aa".to_string()),
            ("hub", "gg".repeat(32)),
            (" ", "ab".repeat(32)),
        ] {
            let dir = tempdir();
            assert!(matches!(
                save_noise_pin(Some(&dir), node_id, &key),
                Err(ThalovantError::InvalidIdentity(_))
            ));
            assert!(!dir.join(NOISE_PINS_FILENAME).exists());
        }
    }

    #[test]
    fn equivalent_hex_pins_preserve_saved_bytes() {
        for saved in ["AB".repeat(32), "aB".repeat(32)] {
            let dir = tempdir();
            save_noise_pin(Some(&dir), "hub", &saved).unwrap();
            let path = dir.join(NOISE_PINS_FILENAME);
            let before = fs::read(&path).unwrap();
            save_noise_pin(Some(&dir), "hub", &saved.to_ascii_lowercase()).unwrap();
            pin_hub_key(Some(&dir), "hub", &saved.to_ascii_lowercase()).unwrap();
            assert!(matches!(
                save_noise_pin(Some(&dir), "hub", &"cd".repeat(32)),
                Err(ThalovantError::Connection(_))
            ));
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

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

    /// Complete publication cannot replace an existing identity, including a
    /// writer that does not cooperate with our lock-file convention.
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

    #[test]
    fn interrupted_staging_never_publishes_or_replaces_trust() {
        for exclusive in [true, false] {
            for complete in [true, false] {
                let dir = tempdir();
                let path = dir.join(if exclusive {
                    NOISE_KEY_FILENAME
                } else {
                    NOISE_PINS_FILENAME
                });
                if !exclusive {
                    write_private(&path, "original trust").unwrap();
                }
                let error =
                    publish_private(&path, &"ab".repeat(32), exclusive, |temporary, contents| {
                        assert_ne!(temporary, path);
                        write_private_at(
                            temporary,
                            if complete { contents } else { &contents[..32] },
                        )?;
                        Err(std::io::Error::other("injected write/flush failure").into())
                    })
                    .unwrap_err();
                assert!(error.to_string().contains("injected"));
                if exclusive {
                    assert!(!path.exists());
                } else {
                    assert_eq!(fs::read_to_string(&path).unwrap(), "original trust");
                }
                assert!(!fs::read_dir(&dir).unwrap().any(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "tmp")));
                fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    struct ChildWorker(std::process::Child);
    impl Drop for ChildWorker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn worker(dir: &Path, operation: &str, id: usize) -> ChildWorker {
        ChildWorker(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "noise_store::tests::store_process_worker",
                    "--nocapture",
                ])
                .env("THALOVANT_STORE_TEST_DIR", dir)
                .env("THALOVANT_STORE_TEST_OPERATION", operation)
                .env("THALOVANT_STORE_TEST_ID", id.to_string())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn wait_for_file(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not reach {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    // This stress fixture checks durable state under competing processes. The
    // production API deliberately bounds each lock wait to five seconds; a slow
    // Windows runner may reach that bound while the other workers publish pins.
    // Retry only that pre-mutation refusal, with an overall fixture deadline.
    fn under_store_contention<T>(mut operation: impl FnMut() -> Result<T>) -> Result<T> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match operation() {
                Err(ThalovantError::Timeout(message))
                    if message == "Noise state lock acquisition timed out"
                        && std::time::Instant::now() < deadline => {}
                result => return result,
            }
        }
    }

    #[test]
    fn store_process_worker() {
        let Some(directory) = std::env::var_os("THALOVANT_STORE_TEST_DIR") else {
            return;
        };
        let dir = PathBuf::from(directory);
        let operation = std::env::var("THALOVANT_STORE_TEST_OPERATION").unwrap();
        let id: usize = std::env::var("THALOVANT_STORE_TEST_ID")
            .unwrap()
            .parse()
            .unwrap();
        if operation.starts_with("crash") {
            let _guard = lock_state(Some(&dir)).unwrap();
            let exclusive = operation == "crash-static";
            let path = dir.join(if exclusive {
                NOISE_KEY_FILENAME
            } else {
                NOISE_PINS_FILENAME
            });
            let _ = publish_private(&path, &"cd".repeat(32), exclusive, |temporary, contents| {
                write_private_at(temporary, if id == 0 { &contents[..32] } else { contents })?;
                fs::write(dir.join("staged"), b"ready")?;
                loop {
                    std::thread::park();
                }
            });
        } else {
            fs::write(dir.join(format!("ready-{id}")), b"ready").unwrap();
            wait_for_file(&dir.join("go"));
            let key = under_store_contention(|| load_or_create_noise_key(Some(&dir))).unwrap();
            fs::write(dir.join(format!("result-{id}")), hex::encode(key)).unwrap();
            if operation == "distinct" {
                for index in 0..12 {
                    let node = format!("hub-{id}-{index}");
                    under_store_contention(|| {
                        pin_hub_key(Some(&dir), &node, &format!("{:02x}", id + 1).repeat(32))
                    })
                    .unwrap();
                }
            } else {
                let result = under_store_contention(|| {
                    pin_hub_key(
                        Some(&dir),
                        "contested",
                        &format!("{:02x}", id + 1).repeat(32),
                    )
                });
                let outcome = match result {
                    Ok(()) => "won",
                    Err(ThalovantError::Connection(message))
                        if message.starts_with("the hub's Noise static key changed.") =>
                    {
                        "lost"
                    }
                    Err(error) => panic!("Unexpected contested pin error: {error}"),
                };
                fs::write(dir.join(format!("pin-{id}")), outcome).unwrap();
            }
        }
    }

    #[test]
    fn killed_writer_leaves_only_untrusted_staging_and_releases_lock() {
        for operation in ["crash-static", "crash-pin"] {
            for complete in [0, 1] {
                let dir = tempdir();
                if operation == "crash-pin" {
                    pin_hub_key(Some(&dir), "preserved", &"12".repeat(32)).unwrap();
                }
                let mut child = worker(&dir, operation, complete);
                wait_for_file(&dir.join("staged"));
                if operation == "crash-static" {
                    assert!(!dir.join(NOISE_KEY_FILENAME).exists());
                }
                child.0.kill().unwrap();
                child.0.wait().unwrap();
                let key = load_or_create_noise_key(Some(&dir)).unwrap();
                assert_eq!(load_or_create_noise_key(Some(&dir)).unwrap(), key);
                if operation == "crash-pin" {
                    assert_eq!(
                        load_noise_pin(Some(&dir), "preserved").unwrap(),
                        Some("12".repeat(32))
                    );
                }
                fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn independent_processes_preserve_one_key_and_transactional_pins() {
        for operation in ["distinct", "conflicting"] {
            let dir = tempdir();
            let mut children: Vec<_> = (0..6).map(|id| worker(&dir, operation, id)).collect();
            for id in 0..6 {
                wait_for_file(&dir.join(format!("ready-{id}")));
            }
            fs::write(dir.join("go"), b"start").unwrap();
            for child in &mut children {
                assert!(child.0.wait().unwrap().success());
            }
            let expected = hex::encode(load_or_create_noise_key(Some(&dir)).unwrap());
            for id in 0..6 {
                assert_eq!(
                    fs::read_to_string(dir.join(format!("result-{id}"))).unwrap(),
                    expected
                );
            }
            if operation == "distinct" {
                for id in 0..6 {
                    for index in 0..12 {
                        assert_eq!(
                            load_noise_pin(Some(&dir), &format!("hub-{id}-{index}")).unwrap(),
                            Some(format!("{:02x}", id + 1).repeat(32))
                        );
                    }
                }
            } else {
                let winners = (0..6)
                    .filter(|id| {
                        fs::read_to_string(dir.join(format!("pin-{id}"))).unwrap() == "won"
                    })
                    .count();
                assert_eq!(winners, 1);
            }
            fs::remove_dir_all(dir).unwrap();
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
    #[test]
    fn corrupt_trust_files_are_preserved_and_never_reset() {
        let dir = tempdir();
        let key = dir.join(NOISE_KEY_FILENAME);
        write_private_at(&key, "incomplete").unwrap();
        assert!(load_or_create_noise_key(Some(&dir)).is_err());
        assert_eq!(fs::read_to_string(&key).unwrap(), "incomplete");
        let pins = dir.join(NOISE_PINS_FILENAME);
        write_private_at(&pins, "{partial").unwrap();
        assert!(pin_hub_key(Some(&dir), "hub", &"aa".repeat(32)).is_err());
        assert_eq!(fs::read_to_string(&pins).unwrap(), "{partial");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_and_exposed_pins_are_refused() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        for filename in [NOISE_KEY_FILENAME, NOISE_PINS_FILENAME, NOISE_PSK_FILENAME] {
            let dir = tempdir();
            let outside = dir.join("outside");
            write_private_at(&outside, "{}").unwrap();
            symlink(&outside, dir.join(filename)).unwrap();
            let rejected = match filename {
                NOISE_KEY_FILENAME => load_or_create_noise_key(Some(&dir)).is_err(),
                NOISE_PINS_FILENAME => load_noise_pin(Some(&dir), "hub").is_err(),
                _ => load_cached_psk(Some(&dir), "hub").is_err(),
            };
            assert!(rejected);
            assert_eq!(fs::read_to_string(outside).unwrap(), "{}");
        }
        let dir = tempdir();
        pin_hub_key(Some(&dir), "hub", &"aa".repeat(32)).unwrap();
        fs::set_permissions(
            dir.join(NOISE_PINS_FILENAME),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(load_noise_pin(Some(&dir), "hub").is_err());
    }
    #[test]
    fn an_external_process_lock_has_a_finite_acquisition_budget() {
        let dir = tempdir();
        let mut holder = worker(&dir, "crash-static", 0);
        wait_for_file(&dir.join("staged"));
        let started = std::time::Instant::now();
        assert!(matches!(
            lock_state_with_timeout(Some(&dir), std::time::Duration::from_millis(30)),
            Err(ThalovantError::Timeout(_))
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        holder.0.kill().unwrap();
        holder.0.wait().unwrap();
        load_or_create_noise_key(Some(&dir)).unwrap();
    }
    #[test]
    fn staging_collisions_do_not_claim_an_existing_published_key_or_remove_the_staging_file() {
        let dir = tempdir();
        let path = dir.join(NOISE_KEY_FILENAME);
        let mut staged = None;
        let error = publish_private(&path, "new-key", true, |temporary, _| {
            write_private_at(temporary, "other-writer").unwrap();
            staged = Some(temporary.to_path_buf());
            Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into())
        })
        .unwrap_err();
        assert!(
            !matches!(error, ThalovantError::Io(ref error) if error.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(staged.unwrap()).unwrap(), "other-writer");
        load_or_create_noise_key(Some(&dir)).unwrap();
    }
}
