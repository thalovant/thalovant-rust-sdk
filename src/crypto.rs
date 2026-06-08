use crate::errors::{Result, ThalovantError};
use aes_gcm::{
    aead::{consts::U16, generic_array::GenericArray, Aead, KeyInit},
    aes::Aes128,
    Aes128Gcm, AesGcm, Nonce,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

type Aes128Gcm16 = AesGcm<Aes128, U16>;

#[derive(Debug, Deserialize, Serialize)]
struct EncryptedJson {
    ciphertext: String,
    tag: String,
    nonce: String,
}

pub fn runtime_crypto_key(raw: Option<&str>) -> Option<[u8; 16]> {
    let normalized = raw?.trim();
    if normalized.is_empty() {
        return None;
    }
    let bytes = normalized.as_bytes();
    if bytes.len() < 16 {
        return None;
    }
    let mut key = [0_u8; 16];
    key.copy_from_slice(&bytes[..16]);
    Some(key)
}

pub fn encrypt_as_json(key: &str, plaintext: &str) -> Result<String> {
    let key = runtime_crypto_key(Some(key))
        .ok_or_else(|| ThalovantError::Crypto("missing crypto key".to_string()))?;
    let cipher =
        Aes128Gcm::new_from_slice(&key).map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    let mut nonce = [0_u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
        .map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    let tag_start = sealed
        .len()
        .checked_sub(16)
        .ok_or_else(|| ThalovantError::Crypto("invalid encrypted payload".to_string()))?;
    let encrypted = EncryptedJson {
        ciphertext: hex::encode(&sealed[..tag_start]),
        tag: hex::encode(&sealed[tag_start..]),
        nonce: hex::encode(nonce),
    };
    Ok(serde_json::to_string(&encrypted)?)
}

pub fn decrypt_from_json(key: &str, raw: &str) -> Result<String> {
    let encrypted: EncryptedJson = serde_json::from_str(raw)?;
    let key = runtime_crypto_key(Some(key))
        .ok_or_else(|| ThalovantError::Crypto("missing crypto key".to_string()))?;
    let cipher =
        Aes128Gcm::new_from_slice(&key).map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    let nonce =
        hex::decode(encrypted.nonce).map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    let ciphertext =
        hex::decode(encrypted.ciphertext).map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    let tag = hex::decode(encrypted.tag).map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    let mut sealed = ciphertext;
    sealed.extend(tag);
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), sealed.as_ref())
        .map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    String::from_utf8(plaintext).map_err(|err| ThalovantError::Crypto(err.to_string()))
}

pub fn encrypt_as_binary(key: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let key = runtime_crypto_key(Some(key))
        .ok_or_else(|| ThalovantError::Crypto("missing crypto key".to_string()))?;
    let cipher =
        Aes128Gcm16::new_from_slice(&key).map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    let mut nonce = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce);
    let sealed = cipher
        .encrypt(GenericArray::from_slice(&nonce), plaintext)
        .map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    let mut out = Vec::with_capacity(nonce.len() + sealed.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

pub fn decrypt_binary(key: &str, payload: &[u8]) -> Result<Vec<u8>> {
    let key = runtime_crypto_key(Some(key))
        .ok_or_else(|| ThalovantError::Crypto("missing crypto key".to_string()))?;
    if payload.len() <= 32 {
        return Err(ThalovantError::Crypto(
            "invalid encrypted binary payload".to_string(),
        ));
    }
    let cipher =
        Aes128Gcm16::new_from_slice(&key).map_err(|err| ThalovantError::Crypto(err.to_string()))?;
    let nonce = &payload[..16];
    let ciphertext = &payload[16..];
    cipher
        .decrypt(GenericArray::from_slice(nonce), ciphertext)
        .map_err(|err| ThalovantError::Crypto(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_runtime_crypto_key() {
        assert_eq!(
            runtime_crypto_key(Some("0123456789abcdef-extra")).unwrap(),
            *b"0123456789abcdef"
        );
        assert!(runtime_crypto_key(Some("   ")).is_none());
    }

    #[test]
    fn encrypted_json_round_trips() {
        let encrypted = encrypt_as_json("0123456789abcdef-extra", "hello").unwrap();
        let decrypted = decrypt_from_json("0123456789abcdef-extra", &encrypted).unwrap();
        assert_eq!(decrypted, "hello");
    }

    #[test]
    fn encrypted_binary_round_trips() {
        let encrypted = encrypt_as_binary("0123456789abcdef-extra", b"hello").unwrap();
        let decrypted = decrypt_binary("0123456789abcdef-extra", &encrypted).unwrap();
        assert_eq!(decrypted, b"hello");
    }
}
