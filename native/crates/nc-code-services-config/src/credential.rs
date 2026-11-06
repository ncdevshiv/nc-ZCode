//! `credential` channel — encrypted key/value store.
//!
//! File: `{appConfigDir}/credentials.json`. Values written today use
//! `enc:v1:<iv>.<tag>.<ciphertext>` (AES-256-GCM, base64url no padding); legacy plain
//! values are returned as-is. A corrupt store is backed up and the read fails — it is
//! never silently replaced, which would erase other credentials.

use std::path::Path;
use tokio::sync::Mutex;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use nc_code_channels::{CancelToken, Channel, ChannelContext, ChannelFailure};
use nc_code_rpc::WireValue;
use nc_code_service_core::atomic::{atomic_write_text, read_text_if_exists};
use nc_code_service_core::{AppPaths, ServiceArgs, ServiceError};
use rand::RngCore;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const ENCRYPTED_PREFIX: &str = "enc:v1:";
const IV_BYTES: usize = 12;
const AUTH_TAG_BYTES: usize = 16;
const SECRET_ENV: &str = "ZCODE_CREDENTIAL_SECRET";
pub const CREDENTIAL_DECRYPT_ERROR_CODE: &str = "ZCODE_CREDENTIAL_DECRYPT_FAILED";
pub const CREDENTIAL_DECRYPT_ERROR_PREFIX: &str = "Credential decryption failed:";

pub struct CredentialCipher {
    key: [u8; 32],
}

impl CredentialCipher {
    pub fn from_paths(paths: &AppPaths) -> Self {
        let secret = std::env::var(SECRET_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                let platform = match std::env::consts::OS {
                    "windows" => "win32",
                    "macos" => "darwin",
                    other => other,
                };
                let username = std::env::var("USERNAME")
                    .or_else(|_| std::env::var("USER"))
                    .unwrap_or_else(|_| "unknown".to_string());
                format!(
                    "zcode-credential-fallback:{platform}:{}:{username}",
                    paths.home_dir().to_string_lossy()
                )
            });
        Self::from_secret(&secret)
    }

    pub fn from_secret(secret: &str) -> Self {
        let digest = Sha256::digest(secret.as_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest);
        Self { key }
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.key).expect("32-byte key")
    }

    pub fn encrypt(&self, value: &str) -> String {
        let mut iv = [0u8; IV_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut iv);
        let nonce = Nonce::try_from(&iv[..]).expect("12-byte nonce");
        let encrypted = self
            .cipher()
            .encrypt(&nonce, value.as_bytes())
            .expect("AES-GCM encrypt");
        let (ciphertext, tag) = encrypted.split_at(encrypted.len() - AUTH_TAG_BYTES);
        format!(
            "{ENCRYPTED_PREFIX}{}.{}.{}",
            URL_SAFE_NO_PAD.encode(iv),
            URL_SAFE_NO_PAD.encode(tag),
            URL_SAFE_NO_PAD.encode(ciphertext)
        )
    }

    pub fn decrypt(&self, value: &str) -> Result<String, ServiceError> {
        let Some(payload) = value.strip_prefix(ENCRYPTED_PREFIX) else {
            return Ok(value.to_string());
        };
        let parts: Vec<&str> = payload.split('.').collect();
        if parts.len() != 3 {
            return Err(decrypt_error("Illegal ciphertext format"));
        }
        let iv = URL_SAFE_NO_PAD
            .decode(parts[0])
            .map_err(|_| decrypt_error("IV length is illegal"))?;
        let tag = URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|_| decrypt_error("AuthTag length is illegal"))?;
        let ciphertext = URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|_| decrypt_error("Illegal ciphertext format"))?;
        if iv.len() != IV_BYTES {
            return Err(decrypt_error("IV length is illegal"));
        }
        if tag.len() != AUTH_TAG_BYTES {
            return Err(decrypt_error("AuthTag length is illegal"));
        }
        let mut combined = ciphertext;
        combined.extend_from_slice(&tag);
        let nonce = Nonce::try_from(&iv[..]).map_err(|_| decrypt_error("IV length is illegal"))?;
        let plaintext = self
            .cipher()
            .decrypt(&nonce, combined.as_ref())
            .map_err(|_| decrypt_error("Key mismatch or ciphertext corrupted"))?;
        String::from_utf8(plaintext)
            .map_err(|_| decrypt_error("The decryption result is not UTF-8"))
    }
}

fn decrypt_error(reason: &str) -> ServiceError {
    ServiceError::new(format!("{CREDENTIAL_DECRYPT_ERROR_PREFIX}{reason}"))
        .with_code(CREDENTIAL_DECRYPT_ERROR_CODE)
}

pub struct CredentialService {
    paths: AppPaths,
    cipher: CredentialCipher,
    lock: Mutex<()>,
}

impl CredentialService {
    pub fn new(paths: AppPaths) -> Self {
        let cipher = CredentialCipher::from_paths(&paths);
        Self {
            paths,
            cipher,
            lock: Mutex::new(()),
        }
    }

    pub fn with_cipher(paths: AppPaths, cipher: CredentialCipher) -> Self {
        Self {
            paths,
            cipher,
            lock: Mutex::new(()),
        }
    }

    fn credentials_file(&self) -> std::path::PathBuf {
        self.paths.app_config_dir().join("credentials.json")
    }

    pub async fn load(&self, key: &str) -> Result<Option<String>, ServiceError> {
        let key = validate_key(key)?;
        let store = self.read_store().await?;
        match store.get(&key) {
            Some(value) => self
                .cipher
                .decrypt(value.as_str().unwrap_or_default())
                .map(Some),
            None => Ok(None),
        }
    }

    pub async fn save(&self, key: &str, value: &str) -> Result<(), ServiceError> {
        let key = validate_key(key)?;
        let encrypted = self.cipher.encrypt(value);
        let _guard = self.lock.lock().await;
        let mut store = self.read_store().await?;
        store.insert(key, Value::String(encrypted));
        self.write_store(&store).await
    }

    pub async fn delete(&self, key: &str) -> Result<(), ServiceError> {
        let key = validate_key(key)?;
        let _guard = self.lock.lock().await;
        let mut store = self.read_store().await?;
        store.remove(&key);
        self.write_store(&store).await
    }

    async fn read_store(&self) -> Result<Map<String, Value>, ServiceError> {
        let file = self.credentials_file();
        let Some(text) = read_text_if_exists(&file).await? else {
            return Ok(Map::new());
        };
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(_) => {
                self.backup_corrupt_store(&text).await;
                return Err(self.corrupt_store_error());
            }
        };
        let Some(object) = parsed.as_object() else {
            self.backup_corrupt_store(&text).await;
            return Err(self.corrupt_store_error());
        };
        if object.values().any(|value| !value.is_string()) {
            self.backup_corrupt_store(&text).await;
            return Err(self.corrupt_store_error());
        }
        Ok(object.clone())
    }

    async fn write_store(&self, store: &Map<String, Value>) -> Result<(), ServiceError> {
        let contents = format!(
            "{}\n",
            serde_json::to_string_pretty(&Value::Object(store.clone()))?
        );
        let file = self.credentials_file();
        atomic_write_text(&file, &contents).await?;
        set_private_permissions(&file).await;
        Ok(())
    }

    fn corrupt_store_error(&self) -> ServiceError {
        ServiceError::new(format!(
            "ZCode credentials are corrupt: {}",
            self.credentials_file().display()
        ))
    }

    async fn backup_corrupt_store(&self, contents: &str) {
        let file = self.credentials_file();
        let digest = Sha256::digest(contents.as_bytes());
        let content_id: String = digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "credentials.json".to_string());
        let backup = file.with_file_name(format!("{name}.corrupt-{content_id}.bak"));
        if !backup.exists() {
            let _ = tokio::fs::write(&backup, contents).await;
            set_private_permissions(&backup).await;
        }
    }
}

fn validate_key(key: &str) -> Result<String, ServiceError> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(ServiceError::invalid_params(
            "credential key must be a non-empty string",
        ));
    }
    Ok(trimmed.to_string())
}

async fn set_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[async_trait]
impl Channel for CredentialService {
    async fn call(
        &self,
        _ctx: &ChannelContext,
        command: &str,
        args: Vec<WireValue>,
        _cancel: CancelToken,
    ) -> Result<WireValue, ChannelFailure> {
        let args = ServiceArgs::new(args);
        match command {
            "load" => {
                let key = args.required_string(0, "key")?;
                let value = self.load(&key).await?;
                Ok(match value {
                    Some(value) => WireValue::String(value),
                    None => WireValue::Json(Value::Null),
                })
            }
            "save" => {
                let key = args.required_string(0, "key")?;
                let value = args.required_string(1, "value")?;
                self.save(&key, &value).await?;
                Ok(WireValue::Undefined)
            }
            "delete" => {
                let key = args.required_string(0, "key")?;
                self.delete(&key).await?;
                Ok(WireValue::Undefined)
            }
            other => Err(ChannelFailure::method_not_found(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_paths(tag: &str) -> (AppPaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zcode-credential-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        (AppPaths::new(root.join("home"), root.join("data")), root)
    }

    fn service(paths: &AppPaths) -> CredentialService {
        CredentialService::with_cipher(paths.clone(), CredentialCipher::from_secret("test-secret"))
    }

    #[tokio::test]
    async fn encrypted_values_round_trip_and_are_not_plaintext_on_disk() {
        let (paths, root) = temp_paths("roundtrip");
        let service = service(&paths);
        service
            .save("oauth:zai:access_token", "secret-token")
            .await
            .expect("save");
        let raw =
            std::fs::read_to_string(paths.app_config_dir().join("credentials.json")).expect("read");
        assert!(raw.contains("enc:v1:"));
        assert!(!raw.contains("secret-token"));
        assert_eq!(
            service.load("oauth:zai:access_token").await.expect("load"),
            Some("secret-token".to_string())
        );
        assert_eq!(service.load("missing").await.expect("load"), None);
        service
            .delete("oauth:zai:access_token")
            .await
            .expect("delete");
        assert_eq!(
            service.load("oauth:zai:access_token").await.expect("load"),
            None
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn legacy_plain_values_are_read() {
        let (paths, root) = temp_paths("legacy");
        std::fs::create_dir_all(paths.app_config_dir()).expect("dir");
        std::fs::write(
            paths.app_config_dir().join("credentials.json"),
            "{\"plain\":\"value\"}\n",
        )
        .expect("write");
        assert_eq!(
            service(&paths).load("plain").await.expect("load"),
            Some("value".to_string())
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn corrupt_store_fails_closed_without_overwriting() {
        let (paths, root) = temp_paths("corrupt");
        std::fs::create_dir_all(paths.app_config_dir()).expect("dir");
        let file = paths.app_config_dir().join("credentials.json");
        std::fs::write(&file, "{ nope").expect("write");
        let error = service(&paths)
            .load("key")
            .await
            .expect_err("corrupt store must fail");
        assert!(error.message.contains("corrupt"), "{}", error.message);
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "{ nope");
        service(&paths)
            .save("key", "value")
            .await
            .expect_err("save must fail");
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "{ nope");
        let names: Vec<String> = std::fs::read_dir(paths.app_config_dir())
            .expect("entries")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        println!("entries: {names:?}");
        assert_eq!(
            names
                .iter()
                .filter(|name| name.contains(".corrupt-"))
                .count(),
            1
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn tampered_ciphertext_reports_the_protocol_error_code() {
        let cipher = CredentialCipher::from_secret("test-secret");
        let encrypted = cipher.encrypt("value");
        let mut tampered = encrypted.clone().into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        let error = cipher
            .decrypt(&String::from_utf8(tampered).expect("utf8"))
            .expect_err("tamper must fail");
        assert_eq!(
            error.code.as_deref(),
            Some(&json!(CREDENTIAL_DECRYPT_ERROR_CODE))
        );
        assert!(error.message.starts_with(CREDENTIAL_DECRYPT_ERROR_PREFIX));
    }

    #[test]
    fn decrypts_typescript_produced_ciphertexts() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../fixtures/credential-cipher.json"))
                .expect("credential fixture");
        let secret = fixture["secret"].as_str().expect("secret");
        let cipher = CredentialCipher::from_secret(secret);
        for vector in fixture["vectors"].as_array().expect("vectors") {
            let plaintext = vector["plaintext"].as_str().expect("plaintext");
            let encrypted = vector["encrypted"].as_str().expect("encrypted");
            assert_eq!(
                cipher.decrypt(encrypted).expect("decrypt"),
                plaintext,
                "vector {plaintext:?}"
            );
        }
    }

    #[test]
    fn keys_are_trimmed_and_empty_keys_rejected() {
        assert_eq!(validate_key("  key  ").expect("key"), "key");
        assert!(validate_key("   ").is_err());
    }
}
