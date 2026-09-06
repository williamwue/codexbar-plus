//! Resolved user state: the config file plus the DPAPI secret store.
//!
//! Upstream keeps provider toggles in `config.json` and tokens in the Keychain
//! (`Config/CodexBarConfig.swift`, `KeychainCacheStore.swift`). This type is the Windows
//! equivalent seam, and the single place that knows the credential resolution order.

use std::collections::HashMap;

use crate::config::{Config, ProviderConfig};
use crate::cookies::{self, CookieError, CookieSource};
use crate::plugin::{Manifest, PluginValues, SettingKind};
use crate::secret::SecretStore;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Secret(#[from] crate::secret::SecretError),
}

#[derive(Debug, Default)]
pub struct Settings {
    pub config: Config,
    pub secrets: SecretStore,
}

impl Settings {
    /// Loads both files, tolerating a missing or damaged secret store: a provider toggle
    /// must still work when DPAPI cannot read a blob written by another profile.
    pub fn load() -> Self {
        let config = match Config::load() {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(error = %err, "using default config");
                Config::default()
            }
        };
        let secrets = match SecretStore::load() {
            Ok(store) => store,
            Err(err) => {
                tracing::warn!(error = %err, "using empty secret store");
                SecretStore::default()
            }
        };
        Self { config, secrets }
    }

    pub fn save(&self) -> Result<(), SettingsError> {
        self.config.save()?;
        self.secrets.save()?;
        Ok(())
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.config.is_enabled(id)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        self.config.set_enabled(id, enabled);
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.config.provider(id)
    }

    /// Stores a secret encrypted and drops any plaintext copy from the config.
    pub fn set_secret(
        &mut self,
        provider: &str,
        key: &str,
        value: &str,
    ) -> Result<(), SettingsError> {
        self.secrets.set(provider, key, value)?;
        let entry = self.config.provider_mut(provider);
        entry.plugin_secrets.remove(key);
        // A single-secret plugin may have been configured through the generic `apiKey`.
        if entry.api_key.is_some() {
            entry.api_key = None;
        }
        if !value.trim().is_empty() {
            entry.enabled = Some(true);
        }
        Ok(())
    }

    /// Reserved secret key for a manually pasted cookie header.
    ///
    /// Cookie headers are credentials, so they live in the DPAPI store like API keys
    /// rather than in `config.json` (upstream keeps `cookieHeader` in the config file;
    /// this port deliberately does not).
    pub const COOKIE_HEADER_KEY: &'static str = "__cookie_header";

    pub fn cookie_source(&self, provider: &str) -> CookieSource {
        self.provider(provider)
            .and_then(|entry| {
                entry
                    .extra
                    .get("cookieSource")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
            })
            .unwrap_or_default()
    }

    pub fn set_cookie_source(&mut self, provider: &str, source: CookieSource) {
        let value = serde_json::to_value(source).unwrap_or(serde_json::Value::Null);
        self.config
            .provider_mut(provider)
            .extra
            .insert("cookieSource".to_string(), value);
    }

    /// Stores a manually pasted `Cookie:` header, encrypted. Empty clears it.
    pub fn set_cookie_header(&mut self, provider: &str, header: &str) -> Result<(), SettingsError> {
        let trimmed = header.trim();
        self.secrets
            .set(provider, Self::COOKIE_HEADER_KEY, trimmed)?;
        if !trimmed.is_empty() {
            let entry = self.config.provider_mut(provider);
            entry.enabled = Some(true);
            entry.cookie_header = None; // never keep a plaintext copy
        }
        Ok(())
    }

    pub fn has_cookie_header(&self, provider: &str) -> bool {
        self.has_secret(provider, Self::COOKIE_HEADER_KEY)
            || self
                .provider(provider)
                .and_then(|p| p.cookie_header.as_deref())
                .is_some_and(|v| !v.trim().is_empty())
    }

    /// Manual header, decrypted; falls back to a plaintext config value for interop.
    pub fn manual_cookie_header(&self, provider: &str) -> Option<String> {
        if let Ok(Some(value)) = self.secrets.get(provider, Self::COOKIE_HEADER_KEY) {
            if !value.trim().is_empty() {
                return Some(value);
            }
        }
        self.provider(provider)
            .and_then(|p| p.cookie_header.clone())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// Resolves a cookie header for `domains`, honouring the provider's cookie source.
    ///
    /// Manual first (cheap, always works), browser import second (only in `auto`).
    pub fn cookie_header(&self, provider: &str, domains: &[String]) -> Result<String, CookieError> {
        let source = self.cookie_source(provider);
        if source == CookieSource::Off {
            return Err(CookieError::Disabled);
        }
        if source.allows_manual() {
            if let Some(header) = self.manual_cookie_header(provider) {
                return Ok(header);
            }
        }
        if !source.allows_browser() {
            return Err(CookieError::NoCookies(domains.join(", ")));
        }
        cookies::header_for(domains)
    }

    /// Whether this provider could produce a cookie header at all.
    ///
    /// Cheap by design: a pasted header is a definite yes, `auto` counts when a browser
    /// profile exists at all. Actually reading the profile is left to fetch time, because
    /// probing every browser during a readiness check would stall the popover.
    pub fn can_supply_cookies(&self, provider: &str) -> bool {
        let source = self.cookie_source(provider);
        if source == CookieSource::Off {
            return false;
        }
        if source.allows_manual() && self.has_cookie_header(provider) {
            return true;
        }
        source.allows_browser()
            && (!cookies::firefox::profiles().is_empty()
                || cookies::chromium::installed()
                    .iter()
                    .any(cookies::chromium::supports_plain_decryption))
    }

    /// A cookie resolver for the plugin host, bound to this provider's settings.
    pub fn cookie_resolver_for(&self, provider: &str) -> Option<crate::plugin::CookieResolver> {
        let source = self.cookie_source(provider);
        if source == CookieSource::Off {
            return None;
        }
        let manual = self.manual_cookie_header(provider);
        let allows_browser = source.allows_browser();
        Some(Box::new(move |domain: &str| {
            if let Some(header) = &manual {
                return Ok(header.clone());
            }
            if !allows_browser {
                return Err(format!("no cookie header stored for {domain}"));
            }
            cookies::header_for(&[domain.to_string()]).map_err(|e| e.to_string())
        }))
    }

    pub fn set_setting(&mut self, provider: &str, key: &str, value: &str) {
        let entry = self.config.provider_mut(provider);
        let value = value.trim();
        if value.is_empty() {
            entry.plugin_settings.remove(key);
        } else {
            entry
                .plugin_settings
                .insert(key.to_string(), value.to_string());
        }
    }

    pub fn setting(&self, provider: &str, key: &str) -> Option<String> {
        self.provider(provider)?.plugin_settings.get(key).cloned()
    }

    /// Whether a secret is present, without decrypting it into the caller.
    pub fn has_secret(&self, provider: &str, key: &str) -> bool {
        if self.secrets.keys_for(provider).iter().any(|k| k == key) {
            return true;
        }
        let Some(entry) = self.provider(provider) else {
            return false;
        };
        entry
            .plugin_secrets
            .get(key)
            .is_some_and(|v| !v.trim().is_empty())
            || entry
                .api_key
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
    }

    /// Resolves a plugin's declared settings and secrets.
    ///
    /// Order: DPAPI store → plaintext `pluginSecrets`/`pluginSettings` → generic `apiKey`
    /// (single-secret plugins only) → environment variable of the same name.
    pub fn plugin_values(&self, manifest: &Manifest) -> PluginValues {
        let provider = manifest.id.as_str();
        let stored = self.secrets.secrets_for(provider);
        let entry = self.provider(provider);

        let mut settings: HashMap<String, String> = entry
            .map(|p| {
                p.plugin_settings
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let mut secrets: HashMap<String, String> = entry
            .map(|p| {
                p.plugin_secrets
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // Encrypted values win over any plaintext leftovers.
        for (key, value) in stored {
            secrets.insert(key, value);
        }
        settings.retain(|_, v| !v.trim().is_empty());
        secrets.retain(|_, v| !v.trim().is_empty());

        PluginValues::resolve(
            manifest,
            &settings,
            &secrets,
            entry.and_then(|p| p.api_key.as_deref()),
            &|key| std::env::var(key).ok(),
        )
    }

    /// Moves plaintext credentials out of `config.json` into the encrypted store.
    ///
    /// Returns how many values were migrated. Called on save paths so a config written by
    /// an older build (or by hand) upgrades itself instead of lingering in plaintext.
    pub fn migrate_plaintext_secrets(&mut self, manifests: &[Manifest]) -> usize {
        let mut migrated = 0;

        for manifest in manifests {
            let id = manifest.id.clone();
            let Some(entry) = self.config.provider(&id) else {
                continue;
            };

            let mut pending: Vec<(String, String)> = entry
                .plugin_secrets
                .iter()
                .filter(|(_, v)| !v.trim().is_empty())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            // A generic `apiKey` maps onto the single secure setting, like upstream's
            // per-provider resolvers do.
            let secure: Vec<&str> = manifest
                .settings
                .iter()
                .filter(|s| s.kind == SettingKind::Secure)
                .map(|s| s.key.as_str())
                .collect();
            if let (Some(key), [only]) = (entry.api_key.clone(), secure.as_slice()) {
                if !key.trim().is_empty() && !pending.iter().any(|(k, _)| k == only) {
                    pending.push((only.to_string(), key));
                }
            }

            for (key, value) in pending {
                match self.set_secret(&id, &key, &value) {
                    Ok(()) => migrated += 1,
                    Err(err) => {
                        tracing::warn!(provider = %id, key = %key, error = %err, "secret migration failed")
                    }
                }
            }
        }

        migrated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(json: &str) -> Manifest {
        let mut manifest: Manifest = serde_json::from_str(json).unwrap();
        manifest.validate().unwrap();
        manifest
    }

    fn venice() -> Manifest {
        manifest(
            r#"{ "id": "venice", "name": "Venice", "endpoints": ["https://api.venice.ai"],
                 "auth": { "type": "bearer", "secret": "VENICE_API_KEY" },
                 "settings": [{ "key": "VENICE_API_KEY", "title": "API key", "type": "secure" }] }"#,
        )
    }

    fn sub2api() -> Manifest {
        manifest(
            r#"{ "id": "sub2api", "name": "sub2api",
                 "endpoints": [{ "setting": "SUB2API_BASE_URL", "policy": "https-or-loopback-http" }],
                 "auth": { "type": "bearer", "secret": "SUB2API_API_KEY" },
                 "settings": [{ "key": "SUB2API_API_KEY", "title": "API key", "type": "secure" },
                              { "key": "SUB2API_BASE_URL", "title": "Base URL" }] }"#,
        )
    }

    #[test]
    fn storing_a_secret_encrypts_it_and_enables_the_provider() {
        let mut settings = Settings::default();
        settings
            .set_secret("venice", "VENICE_API_KEY", "sk-live")
            .unwrap();

        assert!(settings.is_enabled("venice"));
        assert!(settings.has_secret("venice", "VENICE_API_KEY"));
        // No plaintext anywhere in the config half.
        let serialized = serde_json::to_string(&settings.config).unwrap();
        assert!(!serialized.contains("sk-live"));

        let values = settings.plugin_values(&venice());
        assert_eq!(
            values.secrets.get("VENICE_API_KEY").map(String::as_str),
            Some("sk-live")
        );
        assert!(values.satisfies_auth(&venice()));
    }

    #[test]
    fn encrypted_values_win_over_plaintext_leftovers() {
        let mut settings = Settings::default();
        settings
            .config
            .provider_mut("venice")
            .plugin_secrets
            .insert("VENICE_API_KEY".into(), "old-plaintext".into());
        settings
            .secrets
            .set("venice", "VENICE_API_KEY", "new-encrypted")
            .unwrap();

        let values = settings.plugin_values(&venice());
        assert_eq!(
            values.secrets.get("VENICE_API_KEY").map(String::as_str),
            Some("new-encrypted")
        );
    }

    #[test]
    fn migration_moves_plaintext_and_generic_api_keys_into_the_store() {
        let mut settings = Settings::default();
        settings.config.set_api_key("venice", "sk-generic", true);
        settings
            .config
            .provider_mut("sub2api")
            .plugin_secrets
            .insert("SUB2API_API_KEY".into(), "sk-explicit".into());

        let migrated = settings.migrate_plaintext_secrets(&[venice(), sub2api()]);
        assert_eq!(migrated, 2);

        let serialized = serde_json::to_string(&settings.config).unwrap();
        assert!(!serialized.contains("sk-generic"));
        assert!(!serialized.contains("sk-explicit"));

        assert_eq!(
            settings
                .secrets
                .get("venice", "VENICE_API_KEY")
                .unwrap()
                .as_deref(),
            Some("sk-generic")
        );
        assert_eq!(
            settings
                .secrets
                .get("sub2api", "SUB2API_API_KEY")
                .unwrap()
                .as_deref(),
            Some("sk-explicit")
        );
        assert_eq!(
            settings.migrate_plaintext_secrets(&[venice(), sub2api()]),
            0,
            "migration is idempotent"
        );
    }

    #[test]
    fn plain_settings_stay_in_the_config() {
        let mut settings = Settings::default();
        settings.set_setting("sub2api", "SUB2API_BASE_URL", " http://127.0.0.1:8899 ");
        assert_eq!(
            settings.setting("sub2api", "SUB2API_BASE_URL").as_deref(),
            Some("http://127.0.0.1:8899"),
            "values are trimmed"
        );

        let values = settings.plugin_values(&sub2api());
        assert_eq!(
            values.settings.get("SUB2API_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:8899")
        );

        settings.set_setting("sub2api", "SUB2API_BASE_URL", "");
        assert!(settings.setting("sub2api", "SUB2API_BASE_URL").is_none());
    }

    #[test]
    fn manual_cookie_headers_are_encrypted_and_resolve_first() {
        let mut settings = Settings::default();
        assert!(!settings.has_cookie_header("t3chat"));

        settings
            .set_cookie_header("t3chat", "  session=abc; csrf=def  ")
            .unwrap();
        assert!(settings.has_cookie_header("t3chat"));
        assert!(
            settings.is_enabled("t3chat"),
            "pasting a header opts the provider in"
        );

        let serialized = serde_json::to_string(&settings.config).unwrap();
        assert!(
            !serialized.contains("session=abc"),
            "no plaintext in config.json"
        );

        let header = settings
            .cookie_header("t3chat", &["t3.chat".to_string()])
            .expect("manual header resolves");
        assert_eq!(header, "session=abc; csrf=def");
    }

    #[test]
    fn cookie_source_off_refuses_before_touching_a_browser() {
        let mut settings = Settings::default();
        settings.set_cookie_header("t3chat", "session=abc").unwrap();
        settings.set_cookie_source("t3chat", CookieSource::Off);

        assert!(matches!(
            settings.cookie_header("t3chat", &["t3.chat".to_string()]),
            Err(CookieError::Disabled)
        ));
        assert_eq!(settings.cookie_source("t3chat"), CookieSource::Off);
    }

    #[test]
    fn manual_only_source_never_falls_back_to_a_browser() {
        let mut settings = Settings::default();
        settings.set_cookie_source("t3chat", CookieSource::Manual);
        let err = settings
            .cookie_header("t3chat", &["t3.chat".to_string()])
            .expect_err("no manual header stored");
        assert!(matches!(err, CookieError::NoCookies(_)));
    }

    #[test]
    fn cookie_source_defaults_to_auto_and_round_trips() {
        let mut settings = Settings::default();
        assert_eq!(settings.cookie_source("t3chat"), CookieSource::Auto);
        settings.set_cookie_source("t3chat", CookieSource::Manual);
        assert_eq!(settings.cookie_source("t3chat"), CookieSource::Manual);

        // Survives a config serialisation round trip through the `extra` passthrough.
        let json = serde_json::to_string(&settings.config).unwrap();
        let config: crate::config::Config = serde_json::from_str(&json).unwrap();
        let reloaded = Settings {
            config,
            secrets: crate::secret::SecretStore::default(),
        };
        assert_eq!(reloaded.cookie_source("t3chat"), CookieSource::Manual);
    }

    #[test]
    fn clearing_a_cookie_header_removes_it() {
        let mut settings = Settings::default();
        settings.set_cookie_header("t3chat", "session=abc").unwrap();
        settings.set_cookie_header("t3chat", "").unwrap();
        assert!(!settings.has_cookie_header("t3chat"));
        assert!(settings.manual_cookie_header("t3chat").is_none());
    }

    #[test]
    fn clearing_a_secret_removes_it() {
        let mut settings = Settings::default();
        settings
            .set_secret("venice", "VENICE_API_KEY", "sk-1")
            .unwrap();
        assert!(settings.has_secret("venice", "VENICE_API_KEY"));
        settings.set_secret("venice", "VENICE_API_KEY", "").unwrap();
        assert!(!settings.has_secret("venice", "VENICE_API_KEY"));
        assert!(!settings.plugin_values(&venice()).satisfies_auth(&venice()));
    }

    #[test]
    fn disabled_providers_stay_disabled_until_a_secret_is_stored() {
        let mut settings = Settings::default();
        settings.set_enabled("venice", false);
        assert!(!settings.is_enabled("venice"));
        settings
            .set_secret("venice", "VENICE_API_KEY", "sk-1")
            .unwrap();
        assert!(
            settings.is_enabled("venice"),
            "storing a key opts the provider in"
        );
    }
}
