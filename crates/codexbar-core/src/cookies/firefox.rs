//! Firefox cookie import.
//!
//! Firefox stores cookie values in plaintext in `cookies.sqlite` (`moz_cookies`), so no
//! key unwrapping is involved — which is exactly why it is the reliable Windows path now
//! that Chromium has moved to app-bound encryption. Upstream treats Firefox the same way
//! (`BrowserCookieImportOrder.swift:39-59` marks it as needing no keychain).

use std::path::{Path, PathBuf};

use super::{Cookie, CookieError};

/// Firefox profile directories that contain a cookie database.
pub fn profiles() -> Vec<PathBuf> {
    let Some(roaming) = crate::paths::appdata_dir() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for root in [
        roaming.join("Mozilla").join("Firefox").join("Profiles"),
        // Forks keep the same layout.
        roaming.join("zen").join("Profiles"),
        roaming.join("waterfox").join("Profiles"),
    ] {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let database = entry.path().join("cookies.sqlite");
            if database.is_file() {
                found.push(database);
            }
        }
    }
    // Default-release profiles first: they are the ones a user is signed in to.
    found.sort_by_key(|path| {
        let name = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        (
            !name.contains("default-release"),
            !name.contains("default"),
            name,
        )
    });
    found
}

/// Reads cookies for `domains` from one `cookies.sqlite`.
pub fn read_cookies(database: &Path, domains: &[String]) -> Result<Vec<Cookie>, CookieError> {
    // Firefox keeps the database open with WAL; copying avoids both the lock and any
    // chance of us touching the live file.
    let bytes = std::fs::read(database).map_err(|source| CookieError::Io {
        path: database.to_path_buf(),
        source,
    })?;
    let temp = std::env::temp_dir().join(format!("codexbar-firefox-{}.sqlite", std::process::id()));
    std::fs::write(&temp, &bytes).map_err(|source| CookieError::Io {
        path: temp.clone(),
        source,
    })?;

    let result = read_copied(&temp, domains);
    let _ = std::fs::remove_file(&temp);
    result
}

fn read_copied(path: &Path, domains: &[String]) -> Result<Vec<Cookie>, CookieError> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| CookieError::Sqlite(e.to_string()))?;

    let mut statement = connection
        .prepare("SELECT host, name, value, path, isSecure FROM moz_cookies")
        .map_err(|e| CookieError::Sqlite(e.to_string()))?;
    let rows = statement
        .query_map([], |row| {
            Ok(Cookie {
                host: row.get::<_, String>(0)?,
                name: row.get::<_, String>(1)?,
                value: row.get::<_, String>(2)?,
                path: row
                    .get::<_, Option<String>>(3)?
                    .unwrap_or_else(|| "/".to_string()),
                secure: row.get::<_, Option<i64>>(4)?.unwrap_or(0) != 0,
            })
        })
        .map_err(|e| CookieError::Sqlite(e.to_string()))?;

    Ok(rows
        .flatten()
        .filter(|cookie| super::host_matches(&cookie.host, domains) && !cookie.value.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_profile(dir: &Path, rows: &[(&str, &str, &str)]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let database = dir.join("cookies.sqlite");
        let connection = rusqlite::Connection::open(&database).unwrap();
        // Real Firefox schema subset.
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (
                     id INTEGER PRIMARY KEY, originAttributes TEXT, name TEXT, value TEXT,
                     host TEXT, path TEXT, expiry INTEGER, lastAccessed INTEGER,
                     creationTime INTEGER, isSecure INTEGER, isHttpOnly INTEGER,
                     inBrowserElement INTEGER, sameSite INTEGER);",
            )
            .unwrap();
        for (host, name, value) in rows {
            connection
                .execute(
                    "INSERT INTO moz_cookies (originAttributes, name, value, host, path, expiry,
                                              lastAccessed, creationTime, isSecure, isHttpOnly,
                                              inBrowserElement, sameSite)
                     VALUES ('', ?1, ?2, ?3, '/', 0, 0, 0, 1, 1, 0, 0)",
                    rusqlite::params![name, value, host],
                )
                .unwrap();
        }
        database
    }

    #[test]
    fn reads_plaintext_values_and_filters_by_domain() {
        let dir = std::env::temp_dir().join("codexbar-firefox-read");
        let _ = std::fs::remove_dir_all(&dir);
        let database = make_profile(
            &dir,
            &[
                (".t3.chat", "session", "ff-session"),
                ("t3.chat", "csrf", "ff-csrf"),
                ("example.com", "other", "nope"),
                (".t3.chat", "empty", ""),
            ],
        );

        let cookies = read_cookies(&database, &["t3.chat".to_string()]).unwrap();
        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"session"));
        assert!(!names.contains(&"empty"), "empty values are dropped");
        assert_eq!(cookies[0].value, "ff-session", "no decryption needed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_database_is_an_io_error_naming_the_path() {
        let missing = std::env::temp_dir().join("codexbar-firefox-missing/cookies.sqlite");
        let err = read_cookies(&missing, &["t3.chat".into()]).unwrap_err();
        assert!(matches!(err, CookieError::Io { .. }));
        assert!(err.to_string().contains("cookies.sqlite"));
    }

    #[test]
    fn profile_discovery_prefers_default_release() {
        // Ordering is what matters; the machine may have no Firefox at all.
        let mut paths = vec![
            PathBuf::from(r"C:\p\abc.dev-edition\cookies.sqlite"),
            PathBuf::from(r"C:\p\xyz.default-release\cookies.sqlite"),
            PathBuf::from(r"C:\p\mno.default\cookies.sqlite"),
        ];
        paths.sort_by_key(|path| {
            let name = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            (
                !name.contains("default-release"),
                !name.contains("default"),
                name,
            )
        });
        assert!(paths[0].to_string_lossy().contains("default-release"));
    }
}
