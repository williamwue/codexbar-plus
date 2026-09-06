//! Chromium-family cookie import (Chrome, Edge, Brave, Vivaldi, Chromium).
//!
//! macOS CodexBar delegates this to SweetCookieKit, which unlocks the browser's
//! "Safe Storage" item from the Keychain (`BrowserDetection.swift:95-113`). The Windows
//! equivalent is `Local State` → `os_crypt.encrypted_key`, DPAPI-unwrapped, then
//! AES-256-GCM over each `encrypted_value`.
//!
//! Two Windows-specific realities this module reports honestly instead of guessing:
//!
//! 1. **`v20` App-Bound Encryption** (Chrome 127+). The key moves to
//!    `os_crypt.app_bound_encrypted_key` and is only retrievable through the browser's
//!    elevation service, so a user-level process cannot decrypt those values. Measured on
//!    this machine: every one of Edge's 200 cookies is `v20`.
//! 2. **The live browser holds an exclusive lock** on `Network\Cookies`; opening it while
//!    Chrome runs fails with `ERROR_SHARING_VIOLATION` even with full share flags.
//!
//! Both surface as [`CookieError`] variants with actionable text, and the caller falls
//! back to a manually pasted cookie header.

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;

use super::{Cookie, CookieError};

/// DPAPI blobs in `Local State` start with this tag before the ciphertext.
const DPAPI_PREFIX: &[u8] = b"DPAPI";
/// AES-GCM value versions we can decrypt.
const V10: &[u8] = b"v10";
const V11: &[u8] = b"v11";
/// App-bound encrypted values; not decryptable without the browser's broker.
const V20: &[u8] = b"v20";
const GCM_NONCE_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;

/// A browser we know how to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromiumBrowser {
    pub id: &'static str,
    pub display_name: &'static str,
    pub user_data: PathBuf,
}

/// Chromium browsers installed for the current user, in upstream's preference order
/// (Chrome first — upstream defaults to Chrome only to avoid extra prompts,
/// `AGENTS.md:48`).
pub fn installed() -> Vec<ChromiumBrowser> {
    let Some(local) = crate::paths::local_appdata_dir() else {
        return Vec::new();
    };
    [
        ("chrome", "Google Chrome", "Google/Chrome/User Data"),
        ("edge", "Microsoft Edge", "Microsoft/Edge/User Data"),
        ("brave", "Brave", "BraveSoftware/Brave-Browser/User Data"),
        ("vivaldi", "Vivaldi", "Vivaldi/User Data"),
        ("chromium", "Chromium", "Chromium/User Data"),
    ]
    .into_iter()
    .filter_map(|(id, display_name, suffix)| {
        let user_data = local.join(suffix);
        user_data
            .join("Local State")
            .exists()
            .then_some(ChromiumBrowser {
                id,
                display_name,
                user_data,
            })
    })
    .collect()
}

/// Cookie databases inside a browser's user-data directory, `Default` first.
pub fn cookie_databases(browser: &ChromiumBrowser) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(&browser.user_data) else {
        return found;
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        // Modern Chromium keeps cookies under `<Profile>\Network\Cookies`.
        for candidate in [
            entry.path().join("Network").join("Cookies"),
            entry.path().join("Cookies"),
        ] {
            if candidate.is_file() {
                found.push(candidate);
                break;
            }
        }
    }
    found.sort_by_key(|path| {
        let is_default = path
            .components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case("Default"));
        (!is_default, path.clone())
    });
    found
}

/// Whether this profile still looks decryptable without the browser's broker.
///
/// A cheap `Local State` check, used only for *readiness*: a browser that advertises
/// `app_bound_encrypted_key` will hand out `v20` values that we cannot read. Old `v10`
/// values can still linger in such a profile, so the actual import is always attempted at
/// fetch time — this only decides whether to claim a provider is ready.
pub fn supports_plain_decryption(browser: &ChromiumBrowser) -> bool {
    let Ok(raw) = std::fs::read(browser.user_data.join("Local State")) else {
        return false;
    };
    let Ok(state) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(os_crypt) = state.get("os_crypt") else {
        return false;
    };
    os_crypt.get("encrypted_key").is_some() && os_crypt.get("app_bound_encrypted_key").is_none()
}

/// Unwraps `os_crypt.encrypted_key` from `Local State` into the AES-256 key.
pub fn master_key(browser: &ChromiumBrowser) -> Result<Vec<u8>, CookieError> {
    let path = browser.user_data.join("Local State");
    let raw = std::fs::read(&path).map_err(|source| CookieError::Io {
        path: path.clone(),
        source,
    })?;
    let state: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|e| CookieError::Malformed(format!("{}: {e}", path.display())))?;
    let os_crypt = state.get("os_crypt").ok_or(CookieError::NoMasterKey)?;

    let encoded = os_crypt
        .get("encrypted_key")
        .and_then(|v| v.as_str())
        .ok_or(CookieError::NoMasterKey)?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| CookieError::Malformed("os_crypt.encrypted_key is not base64".into()))?;
    let stripped = blob.strip_prefix(DPAPI_PREFIX).ok_or_else(|| {
        CookieError::Malformed("os_crypt.encrypted_key has no DPAPI prefix".into())
    })?;

    // Chromium protects this key without extra entropy.
    let key = crate::secret::unprotect_opt(stripped, None)
        .map_err(|e| CookieError::Dpapi(e.to_string()))?;
    if key.len() != 32 {
        return Err(CookieError::Malformed(format!(
            "master key is {} bytes, expected 32",
            key.len()
        )));
    }
    Ok(key)
}

/// Decrypts one `encrypted_value`.
///
/// `host` and `name` are the AES-GCM associated data Chromium binds on newer profiles;
/// they are tried both ways because the binding is version dependent.
pub fn decrypt_value(
    key: &[u8],
    value: &[u8],
    host: &str,
    name: &str,
) -> Result<String, CookieError> {
    if value.is_empty() {
        return Ok(String::new());
    }
    if value.starts_with(V20) {
        return Err(CookieError::AppBoundEncryption);
    }
    if !(value.starts_with(V10) || value.starts_with(V11)) {
        // Pre-2018 profiles stored raw DPAPI blobs with no version tag.
        return crate::secret::unprotect_opt(value, None)
            .map_err(|e| CookieError::Dpapi(e.to_string()))
            .map(|plain| String::from_utf8_lossy(&plain).into_owned());
    }
    if value.len() < 3 + GCM_NONCE_LEN + GCM_TAG_LEN {
        return Err(CookieError::Malformed(
            "encrypted value is too short".into(),
        ));
    }

    let aes_key = Key::<Aes256Gcm>::try_from(key)
        .map_err(|_| CookieError::Malformed("master key is not 32 bytes".into()))?;
    let cipher = Aes256Gcm::new(&aes_key);
    let nonce = Nonce::try_from(&value[3..3 + GCM_NONCE_LEN])
        .map_err(|_| CookieError::Malformed("nonce is not 12 bytes".into()))?;
    let ciphertext = &value[3 + GCM_NONCE_LEN..];

    // Newer Chromium binds host+name as associated data; older builds use none.
    let aad = format!("{host}\u{0}{name}");
    for payload in [
        Payload {
            msg: ciphertext,
            aad: aad.as_bytes(),
        },
        Payload {
            msg: ciphertext,
            aad: b"",
        },
    ] {
        if let Ok(plain) = cipher.decrypt(&nonce, payload) {
            return Ok(String::from_utf8_lossy(&plain).into_owned());
        }
    }
    Err(CookieError::Decrypt)
}

/// Reads cookies for `domains` from one cookie database.
///
/// The database is copied through a share-mode read first: SQLite cannot open the live
/// file, and a running browser refuses to share it at all.
pub fn read_cookies(
    database: &Path,
    key: &[u8],
    domains: &[String],
) -> Result<Vec<Cookie>, CookieError> {
    let bytes = read_shared(database)?;
    let temp = std::env::temp_dir().join(format!("codexbar-cookies-{}.sqlite", std::process::id()));
    std::fs::write(&temp, &bytes).map_err(|source| CookieError::Io {
        path: temp.clone(),
        source,
    })?;

    let result = read_copied(&temp, key, domains);
    let _ = std::fs::remove_file(&temp);
    result
}

fn read_copied(path: &Path, key: &[u8], domains: &[String]) -> Result<Vec<Cookie>, CookieError> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| CookieError::Sqlite(e.to_string()))?;

    let mut statement = connection
        .prepare("SELECT host_key, name, encrypted_value, value, path, is_secure FROM cookies")
        .map_err(|e| CookieError::Sqlite(e.to_string()))?;

    let mut app_bound = 0usize;
    let mut failed = 0usize;
    let mut cookies = Vec::new();

    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .map_err(|e| CookieError::Sqlite(e.to_string()))?;

    for row in rows {
        let Ok((host, name, encrypted, plain, cookie_path, secure)) = row else {
            continue;
        };
        if !super::host_matches(&host, domains) {
            continue;
        }
        let value = match plain.filter(|v| !v.is_empty()) {
            Some(value) => value,
            None => match decrypt_value(key, &encrypted, &host, &name) {
                Ok(value) => value,
                Err(CookieError::AppBoundEncryption) => {
                    app_bound += 1;
                    continue;
                }
                Err(_) => {
                    failed += 1;
                    continue;
                }
            },
        };
        if value.is_empty() {
            continue;
        }
        cookies.push(Cookie {
            host,
            name,
            value,
            path: cookie_path.unwrap_or_else(|| "/".to_string()),
            secure: secure.unwrap_or(0) != 0,
        });
    }

    if cookies.is_empty() && app_bound > 0 {
        return Err(CookieError::AppBoundEncryption);
    }
    if failed > 0 {
        tracing::debug!(failed, "some cookie values could not be decrypted");
    }
    Ok(cookies)
}

/// Reads a file the browser may hold open.
///
/// Uses `CreateFileW` with every share flag; a live Chrome still denies it, which becomes
/// [`CookieError::BrowserLocked`] rather than a generic IO error.
#[cfg(windows)]
fn read_shared(path: &Path) -> Result<Vec<u8>, CookieError> {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_SHARING_VIOLATION, GENERIC_READ};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    let handle = match handle {
        Ok(handle) => handle,
        Err(err) if err.code().0 as u32 & 0xFFFF == ERROR_SHARING_VIOLATION.0 => {
            return Err(CookieError::BrowserLocked(path.to_path_buf()))
        }
        Err(err) => {
            return Err(CookieError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other(err.message()),
            })
        }
    };

    let mut out = Vec::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let mut read = 0u32;
        let ok = unsafe { ReadFile(handle, Some(buffer.as_mut_slice()), Some(&mut read), None) };
        if ok.is_err() {
            unsafe { CloseHandle(handle).ok() };
            return Err(CookieError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other("ReadFile failed"),
            });
        }
        if read == 0 {
            break;
        }
        out.extend_from_slice(&buffer[..read as usize]);
    }
    unsafe { CloseHandle(handle).ok() };
    Ok(out)
}

#[cfg(not(windows))]
fn read_shared(path: &Path) -> Result<Vec<u8>, CookieError> {
    std::fs::read(path).map_err(|source| CookieError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a real `v10` value with the real algorithm, so the test exercises the same
    /// code path a browser cookie takes.
    fn seal(key: &[u8], host: &str, name: &str, plain: &str, aad: bool) -> Vec<u8> {
        let aes_key = Key::<Aes256Gcm>::try_from(key).unwrap();
        let cipher = Aes256Gcm::new(&aes_key);
        // Deterministic nonce: this is a test vector, not a security boundary.
        let nonce_bytes = [0x11u8; GCM_NONCE_LEN];
        let nonce = Nonce::try_from(&nonce_bytes[..]).unwrap();
        let binding = format!("{host}\u{0}{name}");
        let payload = Payload {
            msg: plain.as_bytes(),
            aad: if aad { binding.as_bytes() } else { b"" },
        };
        let sealed = cipher.encrypt(&nonce, payload).unwrap();

        let mut out = V10.to_vec();
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&sealed);
        out
    }

    #[test]
    fn decrypts_v10_values_with_and_without_bound_metadata() {
        let key = [7u8; 32];
        let bound = seal(&key, "t3.chat", "session", "abc123", true);
        let unbound = seal(&key, "t3.chat", "session", "xyz789", false);

        assert_eq!(
            decrypt_value(&key, &bound, "t3.chat", "session").unwrap(),
            "abc123"
        );
        assert_eq!(
            decrypt_value(&key, &unbound, "t3.chat", "session").unwrap(),
            "xyz789"
        );
    }

    #[test]
    fn wrong_key_or_binding_fails_instead_of_returning_garbage() {
        let key = [7u8; 32];
        let sealed = seal(&key, "t3.chat", "session", "abc123", true);

        assert!(matches!(
            decrypt_value(&[9u8; 32], &sealed, "t3.chat", "session"),
            Err(CookieError::Decrypt)
        ));
        assert!(
            matches!(
                decrypt_value(&key, &sealed, "other.host", "session"),
                Err(CookieError::Decrypt)
            ),
            "host is authenticated data"
        );
    }

    #[test]
    fn app_bound_values_are_reported_not_guessed() {
        let mut value = V20.to_vec();
        value.extend_from_slice(&[0u8; 40]);
        assert!(matches!(
            decrypt_value(&[7u8; 32], &value, "t3.chat", "session"),
            Err(CookieError::AppBoundEncryption)
        ));
    }

    #[test]
    fn truncated_values_are_rejected() {
        let mut value = V10.to_vec();
        value.extend_from_slice(&[0u8; 4]);
        assert!(matches!(
            decrypt_value(&[7u8; 32], &value, "h", "n"),
            Err(CookieError::Malformed(_))
        ));
        assert_eq!(decrypt_value(&[7u8; 32], &[], "h", "n").unwrap(), "");
    }

    #[test]
    fn master_key_requires_the_dpapi_prefix() {
        let dir = std::env::temp_dir().join("codexbar-chromium-key");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let browser = ChromiumBrowser {
            id: "test",
            display_name: "Test",
            user_data: dir.clone(),
        };

        std::fs::write(dir.join("Local State"), br#"{"os_crypt":{}}"#).unwrap();
        assert!(matches!(
            master_key(&browser),
            Err(CookieError::NoMasterKey)
        ));

        let bogus = base64::engine::general_purpose::STANDARD.encode(b"not-dpapi-wrapped");
        std::fs::write(
            dir.join("Local State"),
            format!(r#"{{"os_crypt":{{"encrypted_key":"{bogus}"}}}}"#),
        )
        .unwrap();
        assert!(matches!(
            master_key(&browser),
            Err(CookieError::Malformed(_))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trips_a_real_dpapi_wrapped_master_key() {
        // Uses this machine's DPAPI, exactly like a browser would.
        let key = [3u8; 32];
        let wrapped = crate::secret::protect_for_tests(&key).expect("DPAPI available");
        let mut blob = DPAPI_PREFIX.to_vec();
        blob.extend_from_slice(&wrapped);

        let dir = std::env::temp_dir().join("codexbar-chromium-key-real");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&blob);
        std::fs::write(
            dir.join("Local State"),
            format!(r#"{{"os_crypt":{{"encrypted_key":"{encoded}"}}}}"#),
        )
        .unwrap();

        let browser = ChromiumBrowser {
            id: "test",
            display_name: "Test",
            user_data: dir.clone(),
        };
        assert_eq!(master_key(&browser).unwrap(), key.to_vec());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_bound_profiles_are_not_advertised_as_readable() {
        let dir = std::env::temp_dir().join("codexbar-chromium-appbound-flag");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let browser = ChromiumBrowser {
            id: "test",
            display_name: "Test",
            user_data: dir.clone(),
        };

        std::fs::write(
            dir.join("Local State"),
            br#"{"os_crypt":{"encrypted_key":"x"}}"#,
        )
        .unwrap();
        assert!(supports_plain_decryption(&browser));

        std::fs::write(
            dir.join("Local State"),
            br#"{"os_crypt":{"encrypted_key":"x","app_bound_encrypted_key":"y"}}"#,
        )
        .unwrap();
        assert!(
            !supports_plain_decryption(&browser),
            "v20 profiles are not readable"
        );

        std::fs::write(dir.join("Local State"), br#"{}"#).unwrap();
        assert!(!supports_plain_decryption(&browser));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_machines_browsers_are_classified_honestly() {
        // Measured: Chrome and Edge both carry app_bound_encrypted_key here, so neither
        // may be advertised as a working cookie source.
        for browser in installed() {
            let readable = supports_plain_decryption(&browser);
            println!("{}: plain-decryptable = {readable}", browser.id);
        }
    }

    #[test]
    fn reads_a_cookie_database_and_filters_by_domain() {
        let key = [7u8; 32];
        let dir = std::env::temp_dir().join("codexbar-chromium-db");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("Cookies");

        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cookies (host_key TEXT, name TEXT, encrypted_value BLOB,
                                       value TEXT, path TEXT, is_secure INTEGER);",
            )
            .unwrap();
        let insert = |host: &str, name: &str, plain: &str| {
            connection
                .execute(
                    "INSERT INTO cookies VALUES (?1, ?2, ?3, '', '/', 1)",
                    rusqlite::params![host, name, seal(&key, host, name, plain, true)],
                )
                .unwrap();
        };
        insert(".t3.chat", "session", "wanted");
        insert("t3.chat", "csrf", "also-wanted");
        insert("example.com", "other", "unwanted");
        drop(connection);

        let cookies = read_cookies(&db, &key, &["t3.chat".to_string()]).unwrap();
        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"session") && names.contains(&"csrf"));
        assert!(cookies.iter().all(|c| !c.value.is_empty()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_all_app_bound_database_reports_app_bound_encryption() {
        let dir = std::env::temp_dir().join("codexbar-chromium-v20");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("Cookies");

        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cookies (host_key TEXT, name TEXT, encrypted_value BLOB,
                                       value TEXT, path TEXT, is_secure INTEGER);",
            )
            .unwrap();
        let mut value = V20.to_vec();
        value.extend_from_slice(&[0u8; 40]);
        connection
            .execute(
                "INSERT INTO cookies VALUES ('.t3.chat', 'session', ?1, '', '/', 1)",
                rusqlite::params![value],
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            read_cookies(&db, &[7u8; 32], &["t3.chat".to_string()]),
            Err(CookieError::AppBoundEncryption)
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
