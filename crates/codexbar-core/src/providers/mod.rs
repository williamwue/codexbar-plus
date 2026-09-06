//! Provider registry.
//!
//! Mirrors upstream's descriptor registry (`Providers/ProviderDescriptor.swift:309-393`)
//! and generated manifest (`Providers/ProviderManifest.swift`): native descriptors first,
//! then every bundled JavaScript plugin, each owning an ordered fetch plan.

pub mod amp;
pub mod augment;
pub mod bedrock;
pub mod claude;
mod cli;
pub mod codex;
pub mod gemini;

use std::sync::LazyLock;

use serde::Serialize;
use time::OffsetDateTime;

use crate::http::HttpClient;
use crate::model::{FetchKind, FetchResult};
use crate::plugin::{self, PluginError, PluginValues};
use crate::settings::Settings;

/// Static provider metadata (subset of upstream `ProviderDescriptor`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderDescriptor {
    pub id: String,
    pub display_name: String,
    /// Brand accent used by the tray icon and cards, `#rrggbb`.
    pub accent: String,
    /// Strategies in priority order, matching upstream's `fetchPlan` pipeline.
    pub strategies: Vec<FetchKind>,
    pub status_page_url: Option<String>,
    /// True when the provider is implemented by a bundled JavaScript plugin.
    pub plugin: bool,
    /// Setting keys exposed by the provider; empty when no configuration UI is needed.
    pub settings: Vec<PluginSettingInfo>,
    /// Plugin needs browser cookies, which Windows cannot import yet.
    pub requires_cookies: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginSettingInfo {
    pub key: String,
    pub title: String,
    pub secure: bool,
}

fn native_descriptors() -> Vec<ProviderDescriptor> {
    vec![
        ProviderDescriptor {
            id: codex::PROVIDER_ID.into(),
            display_name: codex::DISPLAY_NAME.into(),
            accent: "#10a37f".into(),
            strategies: vec![FetchKind::Oauth],
            status_page_url: Some("https://status.openai.com/api/v2/summary.json".into()),
            plugin: false,
            settings: Vec::new(),
            requires_cookies: false,
        },
        ProviderDescriptor {
            id: claude::PROVIDER_ID.into(),
            display_name: claude::DISPLAY_NAME.into(),
            accent: "#d97757".into(),
            strategies: vec![FetchKind::Oauth],
            status_page_url: Some("https://status.anthropic.com/api/v2/summary.json".into()),
            plugin: false,
            settings: Vec::new(),
            requires_cookies: false,
        },
        ProviderDescriptor {
            id: amp::PROVIDER_ID.into(),
            display_name: amp::DISPLAY_NAME.into(),
            accent: "#f4a261".into(),
            strategies: vec![FetchKind::Cli],
            status_page_url: None,
            plugin: false,
            settings: Vec::new(),
            requires_cookies: false,
        },
        ProviderDescriptor {
            id: augment::PROVIDER_ID.into(),
            display_name: augment::DISPLAY_NAME.into(),
            accent: "#6c63ff".into(),
            strategies: vec![FetchKind::Cli],
            status_page_url: None,
            plugin: false,
            settings: Vec::new(),
            requires_cookies: false,
        },
        ProviderDescriptor {
            id: bedrock::PROVIDER_ID.into(),
            display_name: bedrock::DISPLAY_NAME.into(),
            accent: "#ff9900".into(),
            strategies: vec![FetchKind::ApiToken],
            status_page_url: Some("https://health.aws.amazon.com/health/status".into()),
            plugin: false,
            settings: vec![
                PluginSettingInfo {
                    key: "profile".into(),
                    title: "AWS profile".into(),
                    secure: false,
                },
                PluginSettingInfo {
                    key: "accessKeyId".into(),
                    title: "AWS access key ID".into(),
                    secure: true,
                },
                PluginSettingInfo {
                    key: "secretAccessKey".into(),
                    title: "AWS secret access key".into(),
                    secure: true,
                },
                PluginSettingInfo {
                    key: "sessionToken".into(),
                    title: "AWS session token".into(),
                    secure: true,
                },
                PluginSettingInfo {
                    key: "monthlyBudget".into(),
                    title: "Monthly budget (USD)".into(),
                    secure: false,
                },
            ],
            requires_cookies: false,
        },
        ProviderDescriptor {
            id: gemini::PROVIDER_ID.into(),
            display_name: gemini::DISPLAY_NAME.into(),
            accent: "#ab87ea".into(),
            strategies: vec![FetchKind::Oauth],
            status_page_url: None,
            plugin: false,
            settings: Vec::new(),
            requires_cookies: false,
        },
    ]
}

/// Every known provider: native first, then bundled plugins in manifest order.
///
/// A plugin whose manifest fails to load is skipped with a warning instead of taking the
/// whole registry down — one bad upstream file must not break the tray.
static REGISTRY: LazyLock<Vec<ProviderDescriptor>> = LazyLock::new(|| {
    let mut all = native_descriptors();
    for (id, source) in plugin::BUNDLED {
        match plugin::engine::load_manifest(source) {
            Ok(manifest) => all.push(ProviderDescriptor {
                id: manifest.id.clone(),
                display_name: manifest.name.clone(),
                accent: manifest
                    .icon
                    .tint
                    .clone()
                    .unwrap_or_else(|| "#6e5aff".to_string()),
                strategies: vec![FetchKind::ApiToken],
                status_page_url: None,
                plugin: true,
                settings: manifest
                    .settings
                    .iter()
                    .map(|s| PluginSettingInfo {
                        key: s.key.clone(),
                        title: s.title.clone(),
                        secure: s.kind == plugin::SettingKind::Secure,
                    })
                    .collect(),
                requires_cookies: manifest.has_capability(plugin::Capability::BrowserCookies),
            }),
            Err(err) => tracing::warn!(plugin = id, error = %err, "skipping unloadable plugin"),
        }
    }
    all
});

pub fn descriptors() -> &'static [ProviderDescriptor] {
    &REGISTRY
}

pub fn descriptor(id: &str) -> Option<&'static ProviderDescriptor> {
    descriptors().iter().find(|d| d.id == id)
}

pub fn ids() -> Vec<&'static str> {
    descriptors().iter().map(|d| d.id.as_str()).collect()
}

/// Providers that are enabled and have the credentials they need.
///
/// Plugins without a configured secret are skipped: showing 14 "missing API key" rows on
/// first launch is noise, not information.
pub fn active_ids(settings: &Settings) -> Vec<&'static str> {
    descriptors()
        .iter()
        .filter(|descriptor| settings.is_enabled(&descriptor.id))
        // A cookie-backed plugin is only ready once cookies can actually be produced:
        // a pasted header, or an `auto` source with a readable browser profile.
        .filter(|descriptor| {
            !descriptor.requires_cookies || settings.can_supply_cookies(&descriptor.id)
        })
        .filter(|descriptor| match descriptor.id.as_str() {
            amp::PROVIDER_ID => amp::available(),
            augment::PROVIDER_ID => augment::available(),
            bedrock::PROVIDER_ID => bedrock::available(settings),
            gemini::PROVIDER_ID => gemini::available(),
            _ => true,
        })
        .filter(|descriptor| {
            !descriptor.plugin || plugin_values(&descriptor.id, settings).is_some()
        })
        .map(|descriptor| descriptor.id.as_str())
        .collect()
}

/// Resolves a plugin's settings/secrets, returning `None` when its auth is unsatisfied.
fn plugin_values(id: &str, settings: &Settings) -> Option<PluginValues> {
    let manifest = plugin::load_bundled_manifest(id).ok()?;
    let values = settings.plugin_values(&manifest);
    values.satisfies_auth(&manifest).then_some(values)
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("unknown provider `{0}`")]
    UnknownProvider(String),
    #[error("provider `{0}` has no configured credential")]
    MissingCredential(String),
    #[error(transparent)]
    Codex(#[from] codex::CodexError),
    #[error(transparent)]
    Claude(#[from] claude::ClaudeError),
    #[error(transparent)]
    Amp(#[from] amp::AmpError),
    #[error(transparent)]
    Augment(#[from] augment::AugmentError),
    #[error(transparent)]
    Bedrock(#[from] bedrock::BedrockError),
    #[error(transparent)]
    Gemini(#[from] gemini::GeminiError),
    #[error(transparent)]
    Plugin(#[from] PluginError),
    #[error("plugin task panicked: {0}")]
    Join(String),
}

/// Runs one provider's fetch plan.
pub async fn fetch(client: &HttpClient, provider_id: &str) -> Result<FetchResult, FetchError> {
    fetch_with_settings(client, provider_id, &Settings::load()).await
}

pub async fn fetch_with_settings(
    client: &HttpClient,
    provider_id: &str,
    settings: &Settings,
) -> Result<FetchResult, FetchError> {
    match provider_id {
        codex::PROVIDER_ID => Ok(codex::fetch(client).await?),
        claude::PROVIDER_ID => Ok(claude::fetch(client).await?),
        amp::PROVIDER_ID => Ok(amp::fetch().await?),
        augment::PROVIDER_ID => Ok(augment::fetch().await?),
        bedrock::PROVIDER_ID => Ok(bedrock::fetch(client, settings).await?),
        gemini::PROVIDER_ID => Ok(gemini::fetch(client).await?),
        other => {
            let descriptor =
                descriptor(other).ok_or_else(|| FetchError::UnknownProvider(other.to_string()))?;
            if !descriptor.plugin {
                return Err(FetchError::UnknownProvider(other.to_string()));
            }
            let values = plugin_values(other, settings)
                .ok_or_else(|| FetchError::MissingCredential(other.to_string()))?;
            let resolver = settings.cookie_resolver_for(other);
            run_plugin(client.clone(), other.to_string(), values, resolver).await
        }
    }
}

/// Plugins execute synchronously against QuickJS, so they run on a blocking thread and
/// drive their HTTP through the current runtime handle.
async fn run_plugin(
    client: HttpClient,
    id: String,
    values: PluginValues,
    cookie_resolver: Option<plugin::CookieResolver>,
) -> Result<FetchResult, FetchError> {
    let handle = tokio::runtime::Handle::current();
    let strategy_id = format!("{id}.js");
    let snapshot = tokio::task::spawn_blocking(move || {
        plugin::run_bundled_blocking(
            &id,
            values,
            client,
            handle,
            OffsetDateTime::now_utc(),
            &local_time_zone(),
            cookie_resolver,
        )
    })
    .await
    .map_err(|e| FetchError::Join(e.to_string()))??;

    Ok(FetchResult {
        usage: snapshot,
        strategy_id,
        strategy_kind: FetchKind::ApiToken,
    })
}

/// IANA zone exposed to plugins as `ctx.env.timeZone`.
///
/// Windows reports Windows zone ids, which are not IANA names, so honour an explicit
/// override and otherwise fall back to UTC — plugins use this only for day boundaries.
fn local_time_zone() -> String {
    std::env::var("TZ")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && time_tz::timezones::get_by_name(v).is_some())
        .unwrap_or_else(|| "UTC".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_includes_native_providers_and_every_bundled_plugin() {
        let ids = ids();
        for native in ["codex", "claude", "amp", "augment", "bedrock", "gemini"] {
            assert!(ids.contains(&native), "missing native provider {native}");
        }
        assert!(ids.contains(&"venice"), "bundled plugins join the registry");
        assert_eq!(
            ids.len(),
            6 + plugin::BUNDLED.len(),
            "every native provider and bundled plugin manifest must load"
        );
    }

    #[test]
    fn registry_ids_are_unique_and_resolvable() {
        let mut ids = ids();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate provider id in registry");
        for id in ids {
            assert!(descriptor(id).is_some());
        }
    }

    #[test]
    fn plugin_descriptors_expose_their_settings() {
        let venice = descriptor("venice").expect("venice descriptor");
        assert!(venice.plugin);
        assert_eq!(venice.settings.len(), 1);
        assert!(venice.settings[0].secure);
        assert_eq!(venice.settings[0].key, "VENICE_API_KEY");
    }

    #[test]
    fn cookie_backed_plugins_are_held_back_until_cookie_import_exists() {
        let perplexity = descriptor("perplexity").expect("perplexity descriptor");
        assert!(
            perplexity.requires_cookies,
            "perplexity authenticates with cookies"
        );
        assert!(
            !active_ids(&Settings::default()).contains(&"perplexity"),
            "a provider that cannot authenticate must not be polled"
        );
    }

    #[test]
    fn active_ids_skip_plugins_without_credentials() {
        let mut config = Settings::default();
        // Nothing configured: native providers stay, credential-less plugins drop out.
        let active = active_ids(&config);
        assert!(active.contains(&"codex"));
        assert!(!active.contains(&"venice"));

        config
            .set_secret("venice", "VENICE_API_KEY", "sk-test")
            .unwrap();
        assert!(active_ids(&config).contains(&"venice"));

        config.set_enabled("venice", false);
        assert!(!active_ids(&config).contains(&"venice"));
    }

    #[tokio::test]
    async fn unknown_provider_is_rejected_before_any_network_call() {
        let client = HttpClient::new().unwrap();
        let err = fetch_with_settings(&client, "nope", &Settings::default())
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::UnknownProvider(id) if id == "nope"));
    }

    #[tokio::test]
    async fn plugins_without_credentials_fail_before_running() {
        let client = HttpClient::new().unwrap();
        let err = fetch_with_settings(&client, "venice", &Settings::default())
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::MissingCredential(id) if id == "venice"));
    }
}
