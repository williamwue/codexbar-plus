//! JavaScript provider plugins.
//!
//! Windows host for upstream CodexBar's plugin contract
//! (`Sources/CodexBarCore/Plugins/**` + `Resources/Plugins/codexbar-plugin.d.ts`).
//! Upstream's bundled plugin scripts are vendored verbatim under `resources/plugins`, so
//! every provider upstream converts to JavaScript becomes available here by copying one
//! file — that is the whole point of implementing the contract instead of re-porting
//! provider logic by hand.

pub mod engine;
pub mod manifest;
pub mod snapshot;
mod timezone;

use std::collections::HashMap;

use time::OffsetDateTime;

pub use engine::{CookieResolver, HostConfig};
pub use manifest::{Capability, Manifest, Setting, SettingKind};

use crate::http::HttpClient;
use crate::model::UsageSnapshot;

/// Bundled provider plugins.
///
/// Files shared with upstream stay verbatim; Windows-first conversions are kept against
/// the same public plugin contract so they can be replaced by upstream versions later.
pub const BUNDLED: &[(&str, &str)] = &[
    ("aiand", include_str!("../../resources/plugins/aiand.js")),
    ("chutes", include_str!("../../resources/plugins/chutes.js")),
    (
        "clawrouter",
        include_str!("../../resources/plugins/clawrouter.js"),
    ),
    (
        "clinepass",
        include_str!("../../resources/plugins/clinepass.js"),
    ),
    ("crof", include_str!("../../resources/plugins/crof.js")),
    (
        "deepgram",
        include_str!("../../resources/plugins/deepgram.js"),
    ),
    (
        "deepinfra",
        include_str!("../../resources/plugins/deepinfra.js"),
    ),
    (
        "elevenlabs",
        include_str!("../../resources/plugins/elevenlabs.js"),
    ),
    (
        "fireworks",
        include_str!("../../resources/plugins/fireworks.js"),
    ),
    (
        "litellm",
        include_str!("../../resources/plugins/litellm.js"),
    ),
    (
        "llmproxy",
        include_str!("../../resources/plugins/llmproxy.js"),
    ),
    ("manus", include_str!("../../resources/plugins/manus.js")),
    (
        "moonshot",
        include_str!("../../resources/plugins/moonshot.js"),
    ),
    (
        "neuralwatt",
        include_str!("../../resources/plugins/neuralwatt.js"),
    ),
    ("openai", include_str!("../../resources/plugins/openai.js")),
    (
        "openrouter",
        include_str!("../../resources/plugins/openrouter.js"),
    ),
    (
        "perplexity",
        include_str!("../../resources/plugins/perplexity.js"),
    ),
    ("poe", include_str!("../../resources/plugins/poe.js")),
    ("qoder", include_str!("../../resources/plugins/qoder.js")),
    (
        "sub2api",
        include_str!("../../resources/plugins/sub2api.js"),
    ),
    (
        "synthetic",
        include_str!("../../resources/plugins/synthetic.js"),
    ),
    ("t3chat", include_str!("../../resources/plugins/t3chat.js")),
    ("venice", include_str!("../../resources/plugins/venice.js")),
    ("xai", include_str!("../../resources/plugins/xai.js")),
    ("zai", include_str!("../../resources/plugins/zai.js")),
    ("zenmux", include_str!("../../resources/plugins/zenmux.js")),
];

/// Source of a bundled plugin by provider id.
pub fn bundled_source(id: &str) -> Option<&'static str> {
    BUNDLED
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, source)| *source)
}

/// A failure the plugin classified itself through `ctx.fail.*`
/// (prelude marker `__CODEXBAR_FAILURE_V2__:<kind>:<retryAfter>:<message>`).
#[derive(Debug, Clone, PartialEq)]
pub struct PluginFailure {
    /// One of upstream's kinds: `authentication-expired`, `missing-credential`,
    /// `permission-denied`, `rate-limited`, `provider-unavailable`, `parse-failure`,
    /// `network-failure`, `api-failure`.
    pub kind: String,
    pub message: String,
    /// Present only for transient kinds; clamped to 10 seconds like upstream.
    pub retry_after_seconds: Option<f64>,
}

impl std::fmt::Display for PluginFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl PluginFailure {
    /// Whether a later refresh could succeed without user action.
    pub fn is_transient(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "rate-limited" | "provider-unavailable" | "network-failure" | "api-failure"
        )
    }

    /// Whether the user must re-authenticate or supply a credential.
    pub fn needs_credentials(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "authentication-expired" | "missing-credential" | "permission-denied"
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin load failed: {0}")]
    Load(String),
    #[error("invalid plugin manifest: {0}")]
    InvalidManifest(String),
    #[error("network policy rejected the request: {0}")]
    NetworkPolicy(String),
    #[error("secret access denied: {0}")]
    SecretAccess(String),
    #[error("invalid snapshot: {0}")]
    InvalidSnapshot(String),
    #[error("plugin script failed: {0}")]
    Script(String),
    #[error("{0}")]
    Classified(PluginFailure),
    #[error("plugin timed out")]
    TimedOut,
    #[error("unknown plugin '{0}'")]
    Unknown(String),
}

/// Raw result of one plugin run.
#[derive(Debug)]
pub struct PluginOutcome {
    pub manifest: Manifest,
    pub snapshot_json: String,
    pub logs: Vec<String>,
}

/// Resolved credentials/settings for one plugin, as the app stores them.
#[derive(Debug, Clone, Default)]
pub struct PluginValues {
    pub settings: HashMap<String, String>,
    pub secrets: HashMap<String, String>,
}

impl PluginValues {
    /// Resolves declared settings from config values, then the environment.
    ///
    /// Upstream wires each first-party plugin to its provider config through a bespoke
    /// closure (`ScriptFetchStrategy` `resolveValues`). Here the rules are uniform:
    /// 1. an explicit `pluginSettings`/`pluginSecrets` entry,
    /// 2. the provider's generic `apiKey` when the plugin declares exactly one secret,
    /// 3. an environment variable named after the setting key.
    pub fn resolve(
        manifest: &Manifest,
        plugin_settings: &HashMap<String, String>,
        plugin_secrets: &HashMap<String, String>,
        api_key: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let mut values = PluginValues::default();
        let secure_keys: Vec<&str> = manifest
            .settings
            .iter()
            .filter(|s| s.kind == SettingKind::Secure)
            .map(|s| s.key.as_str())
            .collect();
        let single_secret = secure_keys.len() == 1;

        for setting in &manifest.settings {
            let configured = match setting.kind {
                SettingKind::Secure => plugin_secrets.get(&setting.key),
                SettingKind::Plain => plugin_settings.get(&setting.key),
            }
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

            let resolved = configured
                .or_else(|| {
                    if setting.kind == SettingKind::Secure && single_secret {
                        api_key
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(str::to_owned)
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    env(&setting.key)
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                });

            if let Some(value) = resolved {
                match setting.kind {
                    SettingKind::Secure => values.secrets.insert(setting.key.clone(), value),
                    SettingKind::Plain => values.settings.insert(setting.key.clone(), value),
                };
            }
        }
        values
    }

    /// True when every credential the plugin's auth requires is present.
    pub fn satisfies_auth(&self, manifest: &Manifest) -> bool {
        match &manifest.auth {
            Some(auth) => self
                .secrets
                .get(&auth.secret)
                .is_some_and(|v| !v.is_empty()),
            None => true,
        }
    }
}

/// Loads a bundled plugin's manifest.
pub fn load_bundled_manifest(id: &str) -> Result<Manifest, PluginError> {
    let source = bundled_source(id).ok_or_else(|| PluginError::Unknown(id.to_string()))?;
    engine::load_manifest(source)
}

/// Runs a bundled plugin and maps its snapshot.
///
/// Blocking: call from `spawn_blocking`. `handle` drives the HTTP the plugin requests.
pub fn run_bundled_blocking(
    id: &str,
    values: PluginValues,
    http: HttpClient,
    handle: tokio::runtime::Handle,
    now: OffsetDateTime,
    time_zone: &str,
    cookie_resolver: Option<CookieResolver>,
) -> Result<UsageSnapshot, PluginError> {
    let source = bundled_source(id).ok_or_else(|| PluginError::Unknown(id.to_string()))?;
    let mut config = HostConfig::new(now);
    config.settings = values.settings;
    config.secrets = values.secrets;
    config.time_zone = time_zone.to_string();
    config.cookie_resolver = cookie_resolver;

    let outcome = engine::run(source, config, http, handle)?;
    snapshot::map(&outcome.snapshot_json, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(settings_json: &str) -> Manifest {
        let mut manifest: Manifest = serde_json::from_str(&format!(
            r#"{{ "id": "demo", "name": "Demo", "endpoints": ["https://api.demo.test"],
                  "auth": {{ "type": "bearer", "secret": "DEMO_KEY" }}, "settings": {settings_json} }}"#
        ))
        .unwrap();
        manifest.validate().unwrap();
        manifest
    }

    #[test]
    fn bundled_registry_is_exact() {
        let ids: Vec<_> = BUNDLED.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [
                "aiand",
                "chutes",
                "clawrouter",
                "clinepass",
                "crof",
                "deepgram",
                "deepinfra",
                "elevenlabs",
                "fireworks",
                "litellm",
                "llmproxy",
                "manus",
                "moonshot",
                "neuralwatt",
                "openai",
                "openrouter",
                "perplexity",
                "poe",
                "qoder",
                "sub2api",
                "synthetic",
                "t3chat",
                "venice",
                "xai",
                "zai",
                "zenmux",
            ]
        );
        for (id, source) in BUNDLED {
            assert!(source.contains("defineProvider"), "{id} is not a plugin");
            assert!(bundled_source(id).is_some());
        }
        assert!(bundled_source("nope").is_none());
    }

    #[test]
    fn single_secret_plugins_accept_the_generic_api_key() {
        let manifest =
            manifest_with(r#"[{ "key": "DEMO_KEY", "title": "Key", "type": "secure" }]"#);
        let values = PluginValues::resolve(
            &manifest,
            &HashMap::new(),
            &HashMap::new(),
            Some("sk-demo"),
            &|_| None,
        );
        assert_eq!(
            values.secrets.get("DEMO_KEY").map(String::as_str),
            Some("sk-demo")
        );
        assert!(values.satisfies_auth(&manifest));
    }

    #[test]
    fn multi_secret_plugins_require_explicit_entries() {
        let manifest = manifest_with(
            r#"[{ "key": "DEMO_KEY", "title": "Key", "type": "secure" },
                { "key": "OTHER_KEY", "title": "Other", "type": "secure" }]"#,
        );
        let values = PluginValues::resolve(
            &manifest,
            &HashMap::new(),
            &HashMap::new(),
            Some("sk-demo"),
            &|_| None,
        );
        assert!(
            values.secrets.is_empty(),
            "ambiguous api key must not be guessed"
        );
        assert!(!values.satisfies_auth(&manifest));
    }

    #[test]
    fn explicit_config_beats_environment() {
        let manifest =
            manifest_with(r#"[{ "key": "DEMO_KEY", "title": "Key", "type": "secure" }]"#);
        let mut secrets = HashMap::new();
        secrets.insert("DEMO_KEY".to_string(), "from-config".to_string());
        let values = PluginValues::resolve(&manifest, &HashMap::new(), &secrets, None, &|key| {
            (key == "DEMO_KEY").then(|| "from-env".to_string())
        });
        assert_eq!(values.secrets["DEMO_KEY"], "from-config");
    }

    #[test]
    fn environment_fills_undeclared_config() {
        let manifest = manifest_with(
            r#"[{ "key": "DEMO_KEY", "title": "Key", "type": "secure" },
                { "key": "DEMO_BASE", "title": "Base" }]"#,
        );
        let values =
            PluginValues::resolve(&manifest, &HashMap::new(), &HashMap::new(), None, &|key| {
                match key {
                    "DEMO_KEY" => Some("k".to_string()),
                    "DEMO_BASE" => Some("https://demo.test".to_string()),
                    _ => None,
                }
            });
        assert_eq!(values.secrets["DEMO_KEY"], "k");
        assert_eq!(values.settings["DEMO_BASE"], "https://demo.test");
    }

    #[test]
    fn failure_kinds_classify_retryability() {
        let transient = PluginFailure {
            kind: "rate-limited".into(),
            message: "slow".into(),
            retry_after_seconds: Some(5.0),
        };
        assert!(transient.is_transient() && !transient.needs_credentials());

        let auth = PluginFailure {
            kind: "missing-credential".into(),
            message: "no key".into(),
            retry_after_seconds: None,
        };
        assert!(auth.needs_credentials() && !auth.is_transient());
    }
}
