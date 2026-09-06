//! Config file: `%APPDATA%\CodexBar\config.json`.
//!
//! Schema-compatible with upstream (`Config/CodexBarConfig.swift:3-31`,
//! `Config/ProviderConfigCoding.swift:4-52`): `{ version, providers: [ { id, enabled,
//! apiKey, pluginSettings, pluginSecrets, … } ] }`. Unknown keys round-trip untouched so
//! a config shared with the macOS app is never damaged by this port.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not resolve a config location (%APPDATA% and %USERPROFILE% are both unset)")]
    NoLocation,
    #[error("config at {path} is not valid JSON: {source}")]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Anything this port does not model yet (hooks, sync, …).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            providers: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie_header: Option<String>,
    /// Plain plugin settings, keyed by the plugin's declared setting keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_settings: BTreeMap<String, String>,
    /// Secure plugin settings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_secrets: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ProviderConfig {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    /// Providers are opt-in only when explicitly disabled; absent means enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

impl Config {
    /// Reads the first existing candidate path (upstream resolution order plus
    /// `%APPDATA%`), or an empty config when none exists yet.
    pub fn load() -> Result<Self, ConfigError> {
        match paths::config_candidates().into_iter().find(|p| p.exists()) {
            Some(path) => Self::load_from(&path),
            None => Ok(Self::default()),
        }
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_slice(&raw).map_err(|source| ConfigError::Malformed {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Writes atomically and restricts the file to the current user, because it holds
    /// API keys (upstream uses `chmod 0600`; the Windows equivalent is an explicit DACL).
    pub fn save(&self) -> Result<PathBuf, ConfigError> {
        let path = paths::config_candidates()
            .into_iter()
            .find(|p| p.exists())
            .or_else(paths::default_config_path)
            .ok_or(ConfigError::NoLocation)?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut serialized =
            serde_json::to_vec_pretty(self).map_err(|source| ConfigError::Malformed {
                path: path.to_path_buf(),
                source,
            })?;
        serialized.push(b'\n');

        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &serialized).map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        crate::secure::restrict_to_current_user(path);
        Ok(())
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn provider_mut(&mut self, id: &str) -> &mut ProviderConfig {
        if let Some(index) = self.providers.iter().position(|p| p.id == id) {
            return &mut self.providers[index];
        }
        self.providers.push(ProviderConfig::new(id));
        self.providers.last_mut().expect("just pushed")
    }

    /// Absent providers count as enabled, matching upstream's default-on behaviour for
    /// providers the user has never touched.
    pub fn is_enabled(&self, id: &str) -> bool {
        self.provider(id)
            .map(ProviderConfig::is_enabled)
            .unwrap_or(true)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        self.provider_mut(id).enabled = Some(enabled);
    }

    /// Stores an API key and enables the provider, like `codexbar config set-api-key`.
    pub fn set_api_key(&mut self, id: &str, key: &str, enable: bool) {
        let provider = self.provider_mut(id);
        provider.api_key = Some(key.trim().to_string());
        if enable {
            provider.enabled = Some(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_unknown_keys() {
        let raw = r#"{
            "version": 1,
            "hooks": { "enabled": false },
            "providers": [
                { "id": "codex", "enabled": true, "source": "auto", "quotaWarnings": { "session": 90 } },
                { "id": "venice", "apiKey": "sk-1", "pluginSecrets": { "VENICE_API_KEY": "sk-2" } }
            ]
        }"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        assert_eq!(config.providers.len(), 2);
        assert!(config.extra.contains_key("hooks"));

        let written = serde_json::to_string(&config).unwrap();
        assert!(
            written.contains("\"quotaWarnings\""),
            "unknown provider keys survive"
        );
        assert!(written.contains("\"source\":\"auto\""));
        assert!(written.contains("\"hooks\""));
    }

    #[test]
    fn providers_default_to_enabled() {
        let config = Config::default();
        assert!(
            config.is_enabled("codex"),
            "unknown provider defaults to enabled"
        );

        let mut config = Config::default();
        config.set_enabled("codex", false);
        assert!(!config.is_enabled("codex"));
        config.set_enabled("codex", true);
        assert!(config.is_enabled("codex"));
    }

    #[test]
    fn setting_an_api_key_enables_the_provider() {
        let mut config = Config::default();
        config.set_enabled("venice", false);
        config.set_api_key("venice", "  sk-trimmed  ", true);
        let provider = config.provider("venice").unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("sk-trimmed"));
        assert!(provider.is_enabled());
    }

    #[test]
    fn saves_and_reloads_through_a_real_file() {
        let dir = std::env::temp_dir().join("codexbar-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut config = Config::default();
        config.set_api_key("venice", "sk-1", true);
        config
            .provider_mut("zai")
            .plugin_settings
            .insert("ZAI_REGION".into(), "international".into());
        config.save_to(&path).unwrap();

        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(
            reloaded.provider("venice").unwrap().api_key.as_deref(),
            Some("sk-1")
        );
        assert_eq!(
            reloaded
                .provider("zai")
                .unwrap()
                .plugin_settings
                .get("ZAI_REGION")
                .map(String::as_str),
            Some("international")
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_config_reports_its_path() {
        let dir = std::env::temp_dir().join("codexbar-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.json");
        std::fs::write(&path, b"{ not json").unwrap();

        let err = Config::load_from(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Malformed { .. }));
        let _ = std::fs::remove_file(&path);
    }
}
