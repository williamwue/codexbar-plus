//! Windows secret store.
//!
//! macOS CodexBar keeps provider tokens in the Keychain
//! (`KeychainCacheStore.swift:17-19`, `ClaudeOAuthCredentials.swift:688-690`); the
//! documented Windows equivalent for a user-level app is DPAPI with the CurrentUser scope
//! (`CryptProtectData`). Secrets live in their own file so the shared
//! `config.json`, which the macOS app may also read, never has to carry ciphertext it
//! cannot decrypt.
//!
//! Threat model, stated plainly: DPAPI protects the file at rest against other users and
//! offline copies. It does not protect against malware running as this user — nor does
//! the macOS Keychain in a background app. Anything better requires a prompt per read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::paths;

const STORE_VERSION: u32 = 1;
const STORE_FILE: &str = "secrets.json";
/// Bound into the DPAPI entropy so a blob cannot be moved to another key or provider.
const ENTROPY_PREFIX: &str = "codexbar:secret:v1";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("could not resolve a secret store location")]
    NoLocation,
    #[error("secret store at {path} is not valid JSON: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("DPAPI {operation} failed: {message}")]
    Dpapi {
        operation: &'static str,
        message: String,
    },
    #[error("stored secret for '{0}' is not valid base64")]
    Corrupt(String),
}

/// DPAPI-encrypted secrets, keyed `<provider>/<setting key>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretStore {
    pub version: u32,
    /// Base64 of the DPAPI blob.
    #[serde(default)]
    pub entries: BTreeMap<String, String>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Default for SecretStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            entries: BTreeMap::new(),
            path: None,
        }
    }
}

fn entry_key(provider: &str, key: &str) -> String {
    format!("{provider}/{key}")
}

fn entropy_for(provider: &str, key: &str) -> Vec<u8> {
    format!("{ENTROPY_PREFIX}:{provider}:{key}").into_bytes()
}

impl SecretStore {
    /// Default location: next to the config file.
    pub fn default_path() -> Option<PathBuf> {
        paths::default_config_path()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .map(|dir| dir.join(STORE_FILE))
    }

    pub fn load() -> Result<Self, SecretError> {
        let path = Self::default_path().ok_or(SecretError::NoLocation)?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, SecretError> {
        if !path.exists() {
            return Ok(Self {
                path: Some(path.to_path_buf()),
                ..Self::default()
            });
        }
        let raw = std::fs::read(path).map_err(|source| SecretError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut store: Self =
            serde_json::from_slice(&raw).map_err(|source| SecretError::Malformed {
                path: path.to_path_buf(),
                source,
            })?;
        store.path = Some(path.to_path_buf());
        Ok(store)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn save(&self) -> Result<PathBuf, SecretError> {
        let path = self
            .path
            .clone()
            .or_else(Self::default_path)
            .ok_or(SecretError::NoLocation)?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), SecretError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SecretError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut serialized =
            serde_json::to_vec_pretty(self).map_err(|source| SecretError::Malformed {
                path: path.to_path_buf(),
                source,
            })?;
        serialized.push(b'\n');

        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &serialized).map_err(|source| SecretError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| SecretError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // Ciphertext still deserves the tight ACL: it keeps other accounts from copying
        // blobs out for offline attempts and matches the config file's protection.
        crate::secure::restrict_to_current_user(path);
        Ok(())
    }

    /// Encrypts and stores a secret. Empty values remove the entry.
    pub fn set(&mut self, provider: &str, key: &str, value: &str) -> Result<(), SecretError> {
        let value = value.trim();
        if value.is_empty() {
            self.entries.remove(&entry_key(provider, key));
            return Ok(());
        }
        let blob = protect(value.as_bytes(), &entropy_for(provider, key))?;
        self.entries.insert(
            entry_key(provider, key),
            base64::engine::general_purpose::STANDARD.encode(blob),
        );
        Ok(())
    }

    /// Decrypts a stored secret. `Ok(None)` means "not stored".
    pub fn get(&self, provider: &str, key: &str) -> Result<Option<String>, SecretError> {
        let Some(encoded) = self.entries.get(&entry_key(provider, key)) else {
            return Ok(None);
        };
        let blob = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| SecretError::Corrupt(entry_key(provider, key)))?;
        let plain = unprotect(&blob, &entropy_for(provider, key))?;
        Ok(String::from_utf8(plain).ok().map(|v| v.trim().to_string()))
    }

    pub fn remove(&mut self, provider: &str, key: &str) -> bool {
        self.entries.remove(&entry_key(provider, key)).is_some()
    }

    /// Setting keys stored for one provider.
    pub fn keys_for(&self, provider: &str) -> Vec<String> {
        let prefix = format!("{provider}/");
        self.entries
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(str::to_owned))
            .collect()
    }

    /// Every stored secret for one provider, decrypted.
    ///
    /// Entries that fail to decrypt (different user, restored profile) are skipped with a
    /// warning: one damaged blob must not hide the rest.
    pub fn secrets_for(&self, provider: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for key in self.keys_for(provider) {
            match self.get(provider, &key) {
                Ok(Some(value)) => {
                    out.insert(key, value);
                }
                Ok(None) => {}
                Err(err) => tracing::warn!(provider, key = %key, error = %err, "unreadable secret"),
            }
        }
        out
    }
}

/// Wraps bytes with DPAPI and no extra entropy — the shape Chromium uses for its
/// `os_crypt.encrypted_key`. Test-only: production code never writes such blobs.
#[cfg(test)]
pub(crate) fn protect_for_tests(plain: &[u8]) -> Result<Vec<u8>, SecretError> {
    protect(plain, &[])
}

// MARK: - DPAPI

#[cfg(windows)]
fn protect(plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SecretError> {
    use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};

    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let entropy_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    unsafe {
        CryptProtectData(
            &input,
            windows::core::PCWSTR::null(),
            Some(&entropy_blob),
            None,
            None,
            0,
            &mut output,
        )
        .map_err(|e| SecretError::Dpapi {
            operation: "CryptProtectData",
            message: e.message(),
        })?;
    }
    Ok(take_blob(output))
}

#[cfg(windows)]
pub(crate) fn unprotect(blob: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SecretError> {
    unprotect_opt(blob, (!entropy.is_empty()).then_some(entropy))
}

/// `entropy` `None` decrypts blobs written without extra entropy, which is how Chromium
/// protects its `os_crypt.encrypted_key`.
#[cfg(windows)]
pub(crate) fn unprotect_opt(blob: &[u8], entropy: Option<&[u8]>) -> Result<Vec<u8>, SecretError> {
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let entropy_blob = entropy.map(|bytes| CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    });
    let mut output = CRYPT_INTEGER_BLOB::default();

    unsafe {
        CryptUnprotectData(
            &input,
            None,
            entropy_blob.as_ref().map(|b| b as *const _),
            None,
            None,
            0,
            &mut output,
        )
        .map_err(|e| SecretError::Dpapi {
            operation: "CryptUnprotectData",
            message: e.message(),
        })?;
    }
    Ok(take_blob(output))
}

/// Copies a DPAPI output blob and frees the OS allocation.
#[cfg(windows)]
fn take_blob(blob: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};

    let bytes = unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(blob.pbData as *mut _)));
    }
    bytes
}

#[cfg(not(windows))]
fn protect(_plain: &[u8], _entropy: &[u8]) -> Result<Vec<u8>, SecretError> {
    Err(SecretError::Dpapi {
        operation: "CryptProtectData",
        message: "DPAPI is only available on Windows".into(),
    })
}

#[cfg(not(windows))]
pub(crate) fn unprotect_opt(_blob: &[u8], _entropy: Option<&[u8]>) -> Result<Vec<u8>, SecretError> {
    Err(SecretError::Dpapi {
        operation: "CryptUnprotectData",
        message: "DPAPI is only available on Windows".into(),
    })
}

#[cfg(not(windows))]
pub(crate) fn unprotect(_blob: &[u8], _entropy: &[u8]) -> Result<Vec<u8>, SecretError> {
    Err(SecretError::Dpapi {
        operation: "CryptUnprotectData",
        message: "DPAPI is only available on Windows".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("codexbar-secret-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn round_trips_a_secret_through_dpapi() {
        let mut store = SecretStore::default();
        store
            .set("venice", "VENICE_API_KEY", "sk-plain-value")
            .unwrap();

        // The stored form must not contain the plaintext.
        let encoded = store.entries.get("venice/VENICE_API_KEY").unwrap();
        assert!(!encoded.contains("sk-plain-value"));
        let blob = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&blob).contains("sk-plain-value"),
            "ciphertext must not embed the plaintext"
        );

        assert_eq!(
            store.get("venice", "VENICE_API_KEY").unwrap().as_deref(),
            Some("sk-plain-value")
        );
    }

    #[test]
    fn entropy_binds_a_blob_to_its_provider_and_key() {
        let mut store = SecretStore::default();
        store.set("venice", "VENICE_API_KEY", "sk-1").unwrap();

        // Move the blob under a different key: decryption must fail, not silently work.
        let blob = store.entries.get("venice/VENICE_API_KEY").unwrap().clone();
        store.entries.insert("xai/XAI_API_KEY".into(), blob);
        assert!(store.get("xai", "XAI_API_KEY").is_err());
    }

    #[test]
    fn empty_values_remove_the_entry() {
        let mut store = SecretStore::default();
        store.set("venice", "K", "sk-1").unwrap();
        assert!(store.get("venice", "K").unwrap().is_some());
        store.set("venice", "K", "   ").unwrap();
        assert!(store.get("venice", "K").unwrap().is_none());
        assert!(store.entries.is_empty());
    }

    #[test]
    fn survives_a_save_and_reload() {
        let path = temp_path("roundtrip.json");
        let _ = std::fs::remove_file(&path);

        let mut store = SecretStore::load_from(&path).unwrap();
        store.set("sub2api", "SUB2API_API_KEY", "sk-live").unwrap();
        store.set("zai", "Z_AI_API_KEY", "zai-key").unwrap();
        store.save_to(&path).unwrap();

        let reloaded = SecretStore::load_from(&path).unwrap();
        assert_eq!(
            reloaded
                .get("sub2api", "SUB2API_API_KEY")
                .unwrap()
                .as_deref(),
            Some("sk-live")
        );
        assert_eq!(reloaded.keys_for("zai"), vec!["Z_AI_API_KEY".to_string()]);
        assert_eq!(reloaded.secrets_for("sub2api").len(), 1);

        // The file on disk holds no plaintext.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("sk-live"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_entries_report_instead_of_panicking() {
        let mut store = SecretStore::default();
        store
            .entries
            .insert("venice/K".into(), "not base64!!".into());
        assert!(matches!(
            store.get("venice", "K"),
            Err(SecretError::Corrupt(_))
        ));
        assert!(store.secrets_for("venice").is_empty());
    }

    #[test]
    fn missing_store_loads_empty() {
        let path = temp_path("absent.json");
        let _ = std::fs::remove_file(&path);
        let store = SecretStore::load_from(&path).unwrap();
        assert!(store.entries.is_empty());
        assert_eq!(store.path(), Some(path.as_path()));
    }
}
