//! On-disk state for the v3 Noise handshake: this client's static key and the
//! hub keys it has pinned.
//!
//! Both live beside the SDK config file, so `XDG_CONFIG_HOME` and the Windows
//! `APPDATA` location are honored the same way, and both are written `0600`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// Serializes the read-modify-write of the pin file, so two connections pinning
/// different hubs at once cannot lose one another's entry.
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
            write_private(&path, &hex::encode(key))?;
            Ok(key)
        }
        Err(err) => Err(err.into()),
    }
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

/// Write a secret file, creating it `0600` so it is never briefly readable.
#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

/// Write a secret file. Windows has no mode bits to set at creation, and the
/// SDK's permission check is Unix-only for the same reason.
#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents)?;
    Ok(())
}

fn poisoned<T>(_: T) -> ThalovantError {
    ThalovantError::Connection("the Noise state lock was poisoned by a panic".to_string())
}
