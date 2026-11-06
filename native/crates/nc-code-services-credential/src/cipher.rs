//! Credential cipher provider (port of `packages/services/src/credential/providers/credentialCipherProvider.ts`).
//!
//! Encrypts/decrypts credential values with AES-256-GCM. Legacy plaintext values (not prefixed)
//! are returned as-is on decrypt, matching TS behavior.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};

pub const ENCRYPTED_VALUE_PREFIX: &str = "enc:v1:";
pub const CREDENTIAL_CIPHER_IV_BYTES: usize = 12;
pub const CREDENTIAL_CIPHER_AUTH_TAG_BYTES: usize = 16;
pub const CREDENTIAL_SECRET_ENV_KEY: &str = "ZCODE_CREDENTIAL_SECRET";
pub const CREDENTIAL_DECRYPT_ERROR_CODE: &str = "CREDENTIAL_DECRYPT_ERROR";
pub const CREDENTIAL_DECRYPT_ERROR_PREFIX: &str = "Credential decrypt failed: ";

#[derive(Debug, Clone)]
pub struct CredentialCipherProvider {
    key_bytes: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct CredentialDecryptError {
    pub code: &'static str,
    pub message: String,
}
impl std::fmt::Display for CredentialDecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for CredentialDecryptError {}

fn derive_cipher_key(secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let out = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    key
}

fn default_credential_secret(
    env_secret: Option<&str>,
    platform: &str,
    home: &str,
    username: &str,
) -> String {
    if let Some(s) = env_secret {
        if !s.is_empty() {
            return s.to_string();
        }
    }
    format!("zcode-credential-fallback:{platform}:{home}:{username}")
}

pub fn default_secret_from_env() -> String {
    let env_secret = std::env::var(CREDENTIAL_SECRET_ENV_KEY).ok();
    let platform = std::env::consts::OS;
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    default_credential_secret(env_secret.as_deref(), platform, &home, &username)
}

impl CredentialCipherProvider {
    pub fn new(secret: &str) -> Self {
        Self {
            key_bytes: derive_cipher_key(secret),
        }
    }

    pub fn from_env() -> Self {
        Self::new(&default_secret_from_env())
    }

    pub fn encrypt(&self, value: &str) -> Result<String, String> {
        let rng = SystemRandom::new();
        let mut iv = [0u8; CREDENTIAL_CIPHER_IV_BYTES];
        rng.fill(&mut iv).map_err(|_| "random failed".to_string())?;
        let unbound =
            UnboundKey::new(&AES_256_GCM, &self.key_bytes).map_err(|_| "key failed".to_string())?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::assume_unique_for_key(iv);
        let mut in_out = value.as_bytes().to_vec();
        key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| "encrypt failed".to_string())?;
        // ring appends 16-byte tag
        if in_out.len() < CREDENTIAL_CIPHER_AUTH_TAG_BYTES {
            return Err("encrypt output too short".into());
        }
        let split = in_out.len() - CREDENTIAL_CIPHER_AUTH_TAG_BYTES;
        let ciphertext = &in_out[..split];
        let tag = &in_out[split..];
        Ok(format!(
            "{}{}.{}.{}",
            ENCRYPTED_VALUE_PREFIX,
            URL_SAFE_NO_PAD.encode(iv),
            URL_SAFE_NO_PAD.encode(tag),
            URL_SAFE_NO_PAD.encode(ciphertext)
        ))
    }

    pub fn decrypt(&self, value: &str) -> Result<String, CredentialDecryptError> {
        if !value.starts_with(ENCRYPTED_VALUE_PREFIX) {
            return Ok(value.to_string());
        }
        let payload = &value[ENCRYPTED_VALUE_PREFIX.len()..];
        let parts: Vec<&str> = payload.split('.').collect();
        if parts.len() != 3 {
            return Err(CredentialDecryptError {
                code: CREDENTIAL_DECRYPT_ERROR_CODE,
                message: format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}format invalid"),
            });
        }
        let iv = URL_SAFE_NO_PAD
            .decode(parts[0])
            .map_err(|_| CredentialDecryptError {
                code: CREDENTIAL_DECRYPT_ERROR_CODE,
                message: format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}iv decode"),
            })?;
        let tag = URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|_| CredentialDecryptError {
                code: CREDENTIAL_DECRYPT_ERROR_CODE,
                message: format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}tag decode"),
            })?;
        let ciphertext = URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|_| CredentialDecryptError {
                code: CREDENTIAL_DECRYPT_ERROR_CODE,
                message: format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}cipher decode"),
            })?;
        if iv.len() != CREDENTIAL_CIPHER_IV_BYTES {
            return Err(CredentialDecryptError {
                code: CREDENTIAL_DECRYPT_ERROR_CODE,
                message: format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}IV length invalid"),
            });
        }
        if tag.len() != CREDENTIAL_CIPHER_AUTH_TAG_BYTES {
            return Err(CredentialDecryptError {
                code: CREDENTIAL_DECRYPT_ERROR_CODE,
                message: format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}AuthTag length invalid"),
            });
        }
        let unbound =
            UnboundKey::new(&AES_256_GCM, &self.key_bytes).map_err(|_| CredentialDecryptError {
                code: CREDENTIAL_DECRYPT_ERROR_CODE,
                message: format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}key"),
            })?;
        let key = LessSafeKey::new(unbound);
        let mut nonce_bytes = [0u8; CREDENTIAL_CIPHER_IV_BYTES];
        nonce_bytes.copy_from_slice(&iv);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = ciphertext;
        in_out.extend_from_slice(&tag);
        let plain = key
            .open_in_place(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| CredentialDecryptError {
                code: CREDENTIAL_DECRYPT_ERROR_CODE,
                message: format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}decrypt/auth failed"),
            })?;
        String::from_utf8(plain.to_vec()).map_err(|_| CredentialDecryptError {
            code: CREDENTIAL_DECRYPT_ERROR_CODE,
            message: format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}utf8"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let c = CredentialCipherProvider::new("secret");
        let enc = c.encrypt("hello").unwrap();
        assert!(enc.starts_with(ENCRYPTED_VALUE_PREFIX));
        let dec = c.decrypt(&enc).unwrap();
        assert_eq!(dec, "hello");
    }

    #[test]
    fn plaintext_passthrough() {
        let c = CredentialCipherProvider::new("secret");
        assert_eq!(c.decrypt("plain").unwrap(), "plain");
    }

    #[test]
    fn invalid_format_errors() {
        let c = CredentialCipherProvider::new("secret");
        let err = c.decrypt("enc:v1:bad").unwrap_err();
        assert_eq!(err.code, CREDENTIAL_DECRYPT_ERROR_CODE);
    }
}
