//! Cookie acquisition for web-backed providers.
//!
//! Upstream resolves a cookie header from either a manually pasted value or a browser
//! import, controlled per provider by `cookieSource` (`off | manual | auto`,
//! `ProviderCookieSource`). This module is the Windows implementation of that seam and
//! feeds both the JS plugin bridge (`ctx.browser.cookieHeader`) and future native web
//! strategies.
//!
//! Reality check measured on this machine (2026-09): Chrome and Edge both carry
//! `os_crypt.app_bound_encrypted_key`, and every one of Edge's 200 stored cookies uses the
//! `v20` app-bound format, which a user-level process cannot decrypt. A running Chrome
//! also refuses to share its cookie file at all. So the honest order of preference is:
//! manual header first, Firefox next, Chromium `v10` last — and every failure says exactly
//! what to do instead.

pub mod chromium;
pub mod firefox;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One cookie, already decrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub host: String,
    pub name: String,
    pub value: String,
    pub path: String,
    pub secure: bool,
}

/// Where a provider's cookies come from (upstream `ProviderCookieSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CookieSource {
    /// Never read cookies for this provider.
    Off,
    /// Use only the header the user pasted.
    Manual,
    /// Manual header if present, otherwise import from a browser.
    #[default]
    Auto,
}

impl CookieSource {
    pub fn allows_browser(self) -> bool {
        self == CookieSource::Auto
    }

    pub fn allows_manual(self) -> bool {
        matches!(self, CookieSource::Manual | CookieSource::Auto)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CookieError {
    #[error("cookie import is disabled for this provider")]
    Disabled,
    #[error("no cookies found for {0}")]
    NoCookies(String),
    #[error("no browser with readable cookies was found")]
    NoBrowser,
    #[error(
        "this browser encrypts cookies with app-bound encryption (v20), which only the browser \
         itself can decrypt; paste the cookie header manually or use Firefox"
    )]
    AppBoundEncryption,
    #[error("{0} is locked by the running browser; close it or paste the cookie header manually")]
    BrowserLocked(PathBuf),
    #[error("browser master key is missing from Local State")]
    NoMasterKey,
    #[error("cookie value could not be decrypted")]
    Decrypt,
    #[error("DPAPI failed: {0}")]
    Dpapi(String),
    #[error("malformed cookie store: {0}")]
    Malformed(String),
    #[error("sqlite error: {0}")]
    Sqlite(String),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Whether a stored cookie host matches one of the requested domains.
///
/// Chromium stores host-only cookies as `t3.chat` and domain cookies as `.t3.chat`; both
/// must match a request for `t3.chat`, and a subdomain host matches its parent domain.
pub fn host_matches(host: &str, domains: &[String]) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    domains.iter().any(|domain| {
        let domain = domain.trim().trim_start_matches('.').to_lowercase();
        !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
    })
}

/// Serialises cookies into a `Cookie:` header value.
///
/// Later duplicates lose: the first match wins, which for our ordering means the most
/// specific host that was read first.
pub fn to_header(cookies: &[Cookie]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    for cookie in cookies {
        if cookie.name.is_empty() || cookie.value.is_empty() || seen.contains(&cookie.name.as_str())
        {
            continue;
        }
        seen.push(cookie.name.as_str());
        parts.push(format!("{}={}", cookie.name, cookie.value));
    }
    parts.join("; ")
}

/// Result of a browser import attempt, for diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportReport {
    pub browser: String,
    pub database: String,
    pub cookies: usize,
    pub error: Option<String>,
}

/// Imports cookies for `domains` from every readable browser, best first.
///
/// Returns the cookies from the first browser that yields any, plus a report per browser
/// so the UI can explain *why* an import found nothing.
pub fn import(domains: &[String]) -> (Vec<Cookie>, Vec<ImportReport>) {
    let mut reports = Vec::new();
    let mut collected = Vec::new();

    // Firefox first: plaintext values, so it is the path that still works.
    for database in firefox::profiles() {
        let entry = match firefox::read_cookies(&database, domains) {
            Ok(cookies) => {
                let count = cookies.len();
                if collected.is_empty() {
                    collected = cookies;
                }
                ImportReport {
                    browser: "firefox".into(),
                    database: database.display().to_string(),
                    cookies: count,
                    error: None,
                }
            }
            Err(err) => ImportReport {
                browser: "firefox".into(),
                database: database.display().to_string(),
                cookies: 0,
                error: Some(err.to_string()),
            },
        };
        reports.push(entry);
        if !collected.is_empty() {
            return (collected, reports);
        }
    }

    for browser in chromium::installed() {
        let key = match chromium::master_key(&browser) {
            Ok(key) => key,
            Err(err) => {
                reports.push(ImportReport {
                    browser: browser.id.into(),
                    database: browser.user_data.display().to_string(),
                    cookies: 0,
                    error: Some(err.to_string()),
                });
                continue;
            }
        };
        for database in chromium::cookie_databases(&browser) {
            let entry = match chromium::read_cookies(&database, &key, domains) {
                Ok(cookies) => {
                    let count = cookies.len();
                    if collected.is_empty() {
                        collected = cookies;
                    }
                    ImportReport {
                        browser: browser.id.into(),
                        database: database.display().to_string(),
                        cookies: count,
                        error: None,
                    }
                }
                Err(err) => ImportReport {
                    browser: browser.id.into(),
                    database: database.display().to_string(),
                    cookies: 0,
                    error: Some(err.to_string()),
                },
            };
            reports.push(entry);
            if !collected.is_empty() {
                return (collected, reports);
            }
        }
    }

    (collected, reports)
}

/// Imports and formats a header in one step.
pub fn header_for(domains: &[String]) -> Result<String, CookieError> {
    let (cookies, reports) = import(domains);
    if !cookies.is_empty() {
        return Ok(to_header(&cookies));
    }
    // Surface the most actionable failure instead of a generic "not found".
    for report in &reports {
        if let Some(error) = &report.error {
            if error.contains("app-bound") {
                return Err(CookieError::AppBoundEncryption);
            }
        }
    }
    for report in &reports {
        if let Some(error) = &report.error {
            if error.contains("locked") {
                return Err(CookieError::BrowserLocked(PathBuf::from(&report.database)));
            }
        }
    }
    if reports.is_empty() {
        return Err(CookieError::NoBrowser);
    }
    Err(CookieError::NoCookies(domains.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(host: &str, name: &str, value: &str) -> Cookie {
        Cookie {
            host: host.into(),
            name: name.into(),
            value: value.into(),
            path: "/".into(),
            secure: true,
        }
    }

    #[test]
    fn host_matching_covers_domain_and_subdomain_forms() {
        let domains = vec!["t3.chat".to_string()];
        assert!(host_matches("t3.chat", &domains));
        assert!(host_matches(".t3.chat", &domains), "domain cookie");
        assert!(host_matches("api.t3.chat", &domains), "subdomain");
        assert!(host_matches("API.T3.CHAT", &domains), "case-insensitive");
        assert!(
            !host_matches("nott3.chat", &domains),
            "suffix must be a label boundary"
        );
        assert!(!host_matches("t3.chat.evil.com", &domains));
        assert!(!host_matches("t3.chat", &[]), "no domains means no match");
        assert!(!host_matches("t3.chat", &["  ".to_string()]));
    }

    #[test]
    fn header_serialisation_dedupes_and_skips_empties() {
        let cookies = vec![
            cookie(".t3.chat", "session", "abc"),
            cookie("t3.chat", "csrf", "def"),
            cookie("api.t3.chat", "session", "SHADOWED"),
            cookie("t3.chat", "empty", ""),
        ];
        assert_eq!(to_header(&cookies), "session=abc; csrf=def");
        assert_eq!(to_header(&[]), "");
    }

    #[test]
    fn cookie_source_gates_manual_and_browser_paths() {
        assert!(!CookieSource::Off.allows_manual() && !CookieSource::Off.allows_browser());
        assert!(CookieSource::Manual.allows_manual() && !CookieSource::Manual.allows_browser());
        assert!(CookieSource::Auto.allows_manual() && CookieSource::Auto.allows_browser());
        assert_eq!(CookieSource::default(), CookieSource::Auto);
    }

    #[test]
    fn cookie_source_serialises_like_upstreams_config() {
        assert_eq!(
            serde_json::to_string(&CookieSource::Off).unwrap(),
            "\"off\""
        );
        assert_eq!(
            serde_json::to_string(&CookieSource::Manual).unwrap(),
            "\"manual\""
        );
        assert_eq!(
            serde_json::from_str::<CookieSource>("\"auto\"").unwrap(),
            CookieSource::Auto
        );
    }

    #[test]
    fn errors_name_the_way_out() {
        let app_bound = CookieError::AppBoundEncryption.to_string();
        assert!(app_bound.contains("v20"));
        assert!(app_bound.contains("manually") && app_bound.contains("Firefox"));

        let locked = CookieError::BrowserLocked(PathBuf::from(r"C:\p\Cookies")).to_string();
        assert!(locked.contains("close it") || locked.contains("Close"));
    }

    /// The real machine: no Firefox, Chrome locked, Edge on v20. The import must fail with
    /// an explanation rather than silently returning an empty header.
    #[test]
    fn importing_from_this_machine_either_finds_cookies_or_explains_itself() {
        let (cookies, reports) = import(&["t3.chat".to_string()]);
        if cookies.is_empty() {
            let header = header_for(&["t3.chat".to_string()]);
            assert!(header.is_err(), "an empty import must be an error");
            for report in &reports {
                println!("{}: {:?}", report.browser, report.error);
            }
        } else {
            assert!(!to_header(&cookies).is_empty());
        }
    }
}
