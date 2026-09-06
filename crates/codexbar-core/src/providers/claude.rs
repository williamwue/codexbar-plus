//! Claude (Claude Code OAuth) provider.
//!
//! Ports the upstream OAuth strategy:
//! - credentials: `Providers/Claude/ClaudeOAuth/ClaudeOAuthCredentialModels.swift:1-146`
//! - paths:       `Providers/Claude/ClaudeConfigPaths.swift:4-75`
//! - refresh:     `Providers/Claude/ClaudeOAuth/ClaudeOAuthCredentials.swift:25-33,1439-1504,1697-1710`
//! - usage:       `Providers/Claude/ClaudeOAuth/ClaudeOAuthUsageFetcher.swift:60-127,268-398`
//! - mapping:     `Providers/Claude/ClaudeUsageFetcher.swift:1010-1188`
//!
//! Windows note: macOS keeps a Keychain copy of these credentials
//! (`Claude Code-credentials`); on Windows Claude Code only writes the plain file, so the
//! file path is the single source of truth and no secret-store shim is needed to read it.

use std::path::{Path, PathBuf};

use reqwest::Method;
use serde::Deserialize;
use time::{Duration as TimeDuration, OffsetDateTime};

use crate::http::{HttpClient, HttpError};
use crate::model::{
    Confidence, DetailRow, FetchKind, FetchResult, Identity, NamedRateWindow, RateWindow,
    UsageSnapshot,
};
use crate::paths;

pub const PROVIDER_ID: &str = "claude";
pub const DISPLAY_NAME: &str = "Claude";

const BASE_URL: &str = "https://api.anthropic.com";
const USAGE_PATH: &str = "/api/oauth/usage";
const PROFILE_PATH: &str = "/api/oauth/profile";
const BETA_HEADER: &str = "oauth-2025-04-20";
const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
const DEFAULT_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const FALLBACK_CLAUDE_VERSION: &str = "2.1.0";
const FIVE_HOUR_MINUTES: u32 = 5 * 60;
const WEEKLY_MINUTES: u32 = 7 * 24 * 60;
/// Refresh slightly ahead of expiry so a poll never races the token clock.
const REFRESH_SKEW: TimeDuration = TimeDuration::minutes(5);

#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    #[error("no Claude credentials: {0} not found. Run `claude` to sign in.")]
    MissingCredentialsFile(PathBuf),
    #[error("could not resolve %USERPROFILE%/.claude")]
    NoHome,
    #[error(".credentials.json is not valid JSON: {0}")]
    Malformed(String),
    #[error(".credentials.json has no `claudeAiOauth` section")]
    MissingOAuth,
    #[error(".credentials.json has an empty accessToken")]
    MissingAccessToken,
    #[error("Claude OAuth request unauthorized. Run `claude` to re-authenticate.")]
    Unauthorized,
    #[error("Claude refresh rejected: {0}. Run `claude` to re-authenticate.")]
    RefreshRejected(String),
    #[error("Claude returned no usage windows")]
    NoWindows,
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// MARK: - .credentials.json

#[derive(Debug, Clone, Deserialize)]
struct CredentialsFile {
    #[serde(default, rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OAuthSection>,
}

/// Camel-cased keys, exactly as Claude Code writes them.
#[derive(Debug, Clone, Deserialize)]
struct OAuthSection {
    #[serde(default, rename = "accessToken")]
    access_token: Option<String>,
    #[serde(default, rename = "refreshToken")]
    refresh_token: Option<String>,
    /// Milliseconds since the epoch (upstream divides by 1000).
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<f64>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default, rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
    #[serde(default, rename = "subscriptionType")]
    subscription_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<OffsetDateTime>,
    pub scopes: Vec<String>,
    pub rate_limit_tier: Option<String>,
    pub subscription_type: Option<String>,
    pub path: Option<PathBuf>,
}

impl Credentials {
    /// Priority order (upstream `ClaudeOAuthCredentials.load`): explicit environment
    /// token first, then the credentials file. macOS-only Keychain tiers are dropped
    /// because Claude Code does not create them on Windows.
    pub fn load() -> Result<Self, ClaudeError> {
        if let Some(token) = std::env::var("CODEXBAR_CLAUDE_OAUTH_TOKEN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            return Ok(Self {
                access_token: token,
                refresh_token: None,
                expires_at: None,
                scopes: Vec::new(),
                rate_limit_tier: None,
                subscription_type: None,
                path: None,
            });
        }

        let path = paths::claude_credentials_file().ok_or(ClaudeError::NoHome)?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, ClaudeError> {
        if !path.exists() {
            return Err(ClaudeError::MissingCredentialsFile(path.to_path_buf()));
        }
        let raw = std::fs::read(path).map_err(|source| ClaudeError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&raw, Some(path.to_path_buf()))
    }

    pub fn parse(raw: &[u8], path: Option<PathBuf>) -> Result<Self, ClaudeError> {
        let file: CredentialsFile =
            serde_json::from_slice(raw).map_err(|e| ClaudeError::Malformed(e.to_string()))?;
        let oauth = file.claude_ai_oauth.ok_or(ClaudeError::MissingOAuth)?;
        let access_token = oauth
            .access_token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .ok_or(ClaudeError::MissingAccessToken)?;

        Ok(Self {
            access_token,
            refresh_token: oauth.refresh_token,
            expires_at: oauth.expires_at.and_then(|ms| {
                OffsetDateTime::from_unix_timestamp_nanos((ms * 1_000_000.0) as i128).ok()
            }),
            scopes: oauth.scopes.unwrap_or_default(),
            rate_limit_tier: oauth.rate_limit_tier,
            subscription_type: oauth.subscription_type,
            path,
        })
    }

    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        match self.expires_at {
            Some(at) => now >= at,
            // Upstream treats a missing expiry as expired; an env-provided token has no
            // refresh path, so we only use this to decide whether to try a refresh.
            None => false,
        }
    }

    pub fn needs_refresh(&self, now: OffsetDateTime) -> bool {
        match self.expires_at {
            Some(at) => now + REFRESH_SKEW >= at,
            None => false,
        }
    }
}

// MARK: - refresh

#[derive(Debug, Deserialize)]
struct TokenRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

fn client_id() -> String {
    std::env::var("CODEXBAR_CLAUDE_OAUTH_CLIENT_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
}

/// POST `https://platform.claude.com/v1/oauth/token`, form-encoded
/// (upstream `ClaudeOAuthCredentials.swift:1443-1504`).
pub async fn refresh(client: &HttpClient, creds: &Credentials) -> Result<Credentials, ClaudeError> {
    let refresh_token = creds.refresh_token.as_deref().ok_or_else(|| {
        ClaudeError::RefreshRejected("no refreshToken in .credentials.json".into())
    })?;
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", &client_id()),
    ];

    let resp = client
        .send(Method::POST, || {
            client
                .raw()
                .post(TOKEN_ENDPOINT)
                .header(reqwest::header::ACCEPT, "application/json")
                .form(&form)
        })
        .await?;

    if !resp.status.is_success() {
        let body: Option<serde_json::Value> = serde_json::from_slice(&resp.body).ok();
        let code = body
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown_error")
            .to_string();
        return Err(ClaudeError::RefreshRejected(format!(
            "HTTP {} {code}",
            resp.status.as_u16()
        )));
    }

    let parsed: TokenRefreshResponse = resp.json()?;
    let mut next = creds.clone();
    next.access_token = parsed.access_token;
    if let Some(rt) = parsed.refresh_token {
        next.refresh_token = Some(rt);
    }
    next.expires_at = Some(OffsetDateTime::now_utc() + TimeDuration::seconds(parsed.expires_in));
    if let Some(path) = &next.path {
        persist(path, &next)?;
    }
    Ok(next)
}

/// Writes the rotated tokens back into `claudeAiOauth`, preserving every other key so
/// Claude Code keeps working against the same file.
fn persist(path: &Path, creds: &Credentials) -> Result<(), ClaudeError> {
    let existing = std::fs::read(path).map_err(|source| ClaudeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut root: serde_json::Value =
        serde_json::from_slice(&existing).map_err(|e| ClaudeError::Malformed(e.to_string()))?;
    let section = root
        .get_mut("claudeAiOauth")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or(ClaudeError::MissingOAuth)?;

    section.insert(
        "accessToken".into(),
        serde_json::Value::String(creds.access_token.clone()),
    );
    if let Some(rt) = &creds.refresh_token {
        section.insert("refreshToken".into(), serde_json::Value::String(rt.clone()));
    }
    if let Some(at) = creds.expires_at {
        let millis = (at.unix_timestamp_nanos() / 1_000_000) as i64;
        section.insert("expiresAt".into(), serde_json::Value::from(millis));
    }

    let serialized =
        serde_json::to_vec_pretty(&root).map_err(|e| ClaudeError::Malformed(e.to_string()))?;
    let tmp = path.with_extension("json.codexbar-tmp");
    std::fs::write(&tmp, &serialized).map_err(|source| ClaudeError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| ClaudeError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    crate::secure::restrict_to_current_user(path);
    Ok(())
}

// MARK: - usage wire models

/// `utilization` is USED percent (0–100); `resets_at` is an ISO-8601 instant.
#[derive(Debug, Clone, Deserialize)]
pub struct UsageWindow {
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageResponse {
    #[serde(default)]
    pub five_hour: Option<UsageWindow>,
    #[serde(default)]
    pub seven_day: Option<UsageWindow>,
    #[serde(default)]
    pub seven_day_oauth_apps: Option<UsageWindow>,
    #[serde(default)]
    pub seven_day_opus: Option<UsageWindow>,
    #[serde(default)]
    pub seven_day_sonnet: Option<UsageWindow>,
    /// Upstream accepts several spellings for this window
    /// (`ClaudeOAuthUsageFetcher.swift:290-298`).
    #[serde(
        default,
        alias = "seven_day_claude_routines",
        alias = "claude_routines",
        alias = "routines",
        alias = "routine",
        alias = "seven_day_cowork",
        alias = "cowork"
    )]
    pub seven_day_routines: Option<UsageWindow>,
    #[serde(default)]
    pub extra_usage: Option<ExtraUsage>,
    #[serde(default)]
    pub limits: Option<Vec<LimitEntry>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtraUsage {
    #[serde(default)]
    pub is_enabled: Option<bool>,
    #[serde(default)]
    pub used_credits: Option<f64>,
    #[serde(default)]
    pub monthly_limit: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub utilization: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitEntry {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub scope: Option<LimitScope>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitScope {
    #[serde(default)]
    pub model: Option<LimitScopeModel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitScopeModel {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "display_name")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfileResponse {
    #[serde(default)]
    pub account: Option<ProfileAccount>,
    #[serde(default)]
    pub organization: Option<ProfileOrganization>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileAccount {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, rename = "email_address")]
    pub email_address: Option<String>,
    #[serde(default, rename = "full_name")]
    pub full_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileOrganization {
    #[serde(default)]
    pub name: Option<String>,
}

// MARK: - fetch

fn user_agent() -> String {
    format!("claude-code/{FALLBACK_CLAUDE_VERSION} (external, cli)")
}

/// Origin override for tests and self-hosted gateways.
fn base_url() -> String {
    std::env::var("CODEXBAR_CLAUDE_BASE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| BASE_URL.to_string())
}

fn get(
    client: &HttpClient,
    url: String,
    token: String,
) -> impl Fn() -> reqwest::RequestBuilder + '_ {
    move || {
        client
            .raw()
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("anthropic-beta", BETA_HEADER)
            .header(reqwest::header::USER_AGENT, user_agent())
    }
}

pub async fn fetch_usage(
    client: &HttpClient,
    creds: &Credentials,
) -> Result<UsageResponse, ClaudeError> {
    let resp = client
        .send(
            Method::GET,
            get(
                client,
                format!("{}{USAGE_PATH}", base_url()),
                creds.access_token.clone(),
            ),
        )
        .await?;

    match resp.error_for_status() {
        None => Ok(resp.json()?),
        Some(HttpError::Unauthorized(_)) => Err(ClaudeError::Unauthorized),
        Some(err) => Err(ClaudeError::Http(err)),
    }
}

pub async fn fetch_profile(
    client: &HttpClient,
    creds: &Credentials,
) -> Result<ProfileResponse, ClaudeError> {
    let resp = client
        .send(
            Method::GET,
            get(
                client,
                format!("{}{PROFILE_PATH}", base_url()),
                creds.access_token.clone(),
            ),
        )
        .await?;

    match resp.error_for_status() {
        None => Ok(resp.json()?),
        Some(HttpError::Unauthorized(_)) => Err(ClaudeError::Unauthorized),
        Some(err) => Err(ClaudeError::Http(err)),
    }
}

// MARK: - mapping

fn parse_iso8601(raw: Option<&String>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw?.trim(), &time::format_description::well_known::Rfc3339).ok()
}

fn make_window(window: Option<&UsageWindow>, window_minutes: u32) -> Option<RateWindow> {
    let window = window?;
    let utilization = window.utilization?;
    let mut mapped = RateWindow::new(utilization);
    mapped.window_minutes = Some(window_minutes);
    mapped.resets_at = parse_iso8601(window.resets_at.as_ref());
    Some(mapped)
}

/// `UsageResponse` → canonical snapshot.
///
/// Precedence is upstream's (`ClaudeUsageFetcher.swift:1032-1076`):
/// primary = five_hour → seven_day → seven_day_oauth_apps → seven_day_sonnet → seven_day_opus;
/// secondary = seven_day; tertiary = seven_day_sonnet ?? seven_day_opus;
/// extras = model-scoped weekly limits first, Daily Routines last.
pub fn map_snapshot(
    usage: &UsageResponse,
    creds: &Credentials,
    profile: Option<&ProfileResponse>,
    now: OffsetDateTime,
) -> Result<UsageSnapshot, ClaudeError> {
    let mut snapshot = UsageSnapshot::new(now);

    snapshot.primary = make_window(usage.five_hour.as_ref(), FIVE_HOUR_MINUTES)
        .or_else(|| make_window(usage.seven_day.as_ref(), WEEKLY_MINUTES))
        .or_else(|| make_window(usage.seven_day_oauth_apps.as_ref(), WEEKLY_MINUTES))
        .or_else(|| make_window(usage.seven_day_sonnet.as_ref(), WEEKLY_MINUTES))
        .or_else(|| make_window(usage.seven_day_opus.as_ref(), WEEKLY_MINUTES));

    let spend_limit = spend_limit_window(usage.extra_usage.as_ref());

    if snapshot.primary.is_none() {
        // Enterprise/extra-usage accounts expose only a spend cap.
        let Some(window) = spend_limit.clone() else {
            return Err(ClaudeError::NoWindows);
        };
        snapshot.primary = Some(window);
    } else {
        snapshot.secondary = make_window(usage.seven_day.as_ref(), WEEKLY_MINUTES);
        snapshot.tertiary = make_window(usage.seven_day_sonnet.as_ref(), WEEKLY_MINUTES)
            .or_else(|| make_window(usage.seven_day_opus.as_ref(), WEEKLY_MINUTES));
    }

    for entry in usage.limits.iter().flatten() {
        // `is_active` is deliberately not a filter: enforceable scoped limits report false.
        let Some(percent) = entry.percent else {
            continue;
        };
        let model_name = entry
            .scope
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .and_then(|m| m.display_name.clone().or_else(|| m.id.clone()));
        let Some(title) = model_name else { continue };
        let id = entry
            .scope
            .as_ref()
            .and_then(|s| s.model.as_ref())
            .and_then(|m| m.id.clone())
            .unwrap_or_else(|| title.to_lowercase());
        let mut window = RateWindow::new(percent);
        window.window_minutes = Some(WEEKLY_MINUTES);
        window.resets_at = parse_iso8601(entry.resets_at.as_ref());
        snapshot.extra_rate_windows.push(NamedRateWindow {
            id,
            title: format!("{title} weekly"),
            window,
        });
    }

    if let Some(routines) = make_window(usage.seven_day_routines.as_ref(), WEEKLY_MINUTES) {
        snapshot.extra_rate_windows.push(NamedRateWindow {
            id: "claude-routines".into(),
            title: "Daily Routines".into(),
            window: routines,
        });
    }

    if let Some(extra) = usage
        .extra_usage
        .as_ref()
        .filter(|e| e.is_enabled == Some(true))
    {
        // Upstream: OAuth extra-usage amounts arrive in cents.
        if let (Some(used), Some(limit)) = (extra.used_credits, extra.monthly_limit) {
            snapshot.push_rows(vec![DetailRow {
                label: "Extra usage".into(),
                value: format!(
                    "{} / {}",
                    crate::format::currency(used / 100.0, extra.currency.as_deref()),
                    crate::format::currency(limit / 100.0, extra.currency.as_deref())
                ),
                hint: None,
            }]);
        }
    }

    snapshot.identity = Identity {
        account: profile.and_then(|p| {
            p.account
                .as_ref()
                .and_then(|a| a.email.clone().or_else(|| a.email_address.clone()))
        }),
        plan: creds.subscription_type.clone().map(prettify_plan),
        account_id: None,
        organization: profile.and_then(|p| p.organization.as_ref().and_then(|o| o.name.clone())),
    };
    snapshot.confidence = Confidence::Exact;
    snapshot.source_label = Some("oauth".into());
    Ok(snapshot)
}

/// Spend-limit lane for accounts without quota windows
/// (upstream `oauthSpendLimitWindow`, `ClaudeUsageFetcher.swift:1117-1132`).
fn spend_limit_window(extra: Option<&ExtraUsage>) -> Option<RateWindow> {
    let extra = extra.filter(|e| e.is_enabled == Some(true))?;
    let used = extra.used_credits? / 100.0;
    let limit = extra.monthly_limit? / 100.0;
    if limit <= 0.0 {
        return None;
    }
    let used_percent = extra.utilization.unwrap_or((used / limit) * 100.0);
    let mut window = RateWindow::new(used_percent.clamp(0.0, 100.0));
    window.reset_description = Some(format!(
        "Spend limit: {} / {}",
        crate::format::currency(used, extra.currency.as_deref()),
        crate::format::currency(limit, extra.currency.as_deref())
    ));
    Some(window)
}

fn prettify_plan(raw: String) -> String {
    match raw.to_lowercase().as_str() {
        "pro" => "Pro".into(),
        "max" => "Max".into(),
        "max_5x" => "Max 5×".into(),
        "max_20x" => "Max 20×".into(),
        "team" => "Team".into(),
        "enterprise" => "Enterprise".into(),
        "free" => "Free".into(),
        _ => raw,
    }
}

/// Full provider run: load credentials, refresh when near expiry, fetch usage + profile.
pub async fn fetch(client: &HttpClient) -> Result<FetchResult, ClaudeError> {
    let mut creds = Credentials::load()?;
    let now = OffsetDateTime::now_utc();
    if creds.needs_refresh(now) && creds.refresh_token.is_some() {
        match refresh(client, &creds).await {
            Ok(next) => creds = next,
            Err(err) => {
                tracing::warn!(error = %err, "claude token refresh failed; trying existing token")
            }
        }
    }

    let usage = match fetch_usage(client, &creds).await {
        Ok(usage) => usage,
        Err(ClaudeError::Unauthorized) if creds.refresh_token.is_some() => {
            creds = refresh(client, &creds).await?;
            fetch_usage(client, &creds).await?
        }
        Err(err) => return Err(err),
    };

    let profile = match fetch_profile(client, &creds).await {
        Ok(p) => Some(p),
        Err(err) => {
            tracing::debug!(error = %err, "claude profile unavailable");
            None
        }
    };

    Ok(FetchResult {
        usage: map_snapshot(&usage, &creds, profile.as_ref(), OffsetDateTime::now_utc())?,
        strategy_id: "claude.oauth".into(),
        strategy_kind: FetchKind::Oauth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn creds() -> Credentials {
        Credentials {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: Some(datetime!(2026-09-02 12:00 UTC)),
            scopes: vec!["user:inference".into()],
            rate_limit_tier: Some("default".into()),
            subscription_type: Some("max_20x".into()),
            path: None,
        }
    }

    #[test]
    fn credentials_parse_camel_keys_and_millisecond_expiry() {
        let raw = br#"{
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-xyz",
                "refreshToken": "sk-ant-ort01-xyz",
                "expiresAt": 1788000000000,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max_20x"
            },
            "mcpOAuth": {}
        }"#;
        let parsed = Credentials::parse(raw, None).unwrap();
        assert_eq!(parsed.access_token, "sk-ant-oat01-xyz");
        assert_eq!(parsed.scopes.len(), 2);
        assert_eq!(
            parsed.expires_at,
            Some(OffsetDateTime::from_unix_timestamp(1_788_000_000).unwrap())
        );
        assert_eq!(parsed.subscription_type.as_deref(), Some("max_20x"));
    }

    #[test]
    fn credentials_without_oauth_section_are_rejected() {
        let err = Credentials::parse(br#"{"mcpOAuth": {"x": 1}}"#, None).unwrap_err();
        assert!(matches!(err, ClaudeError::MissingOAuth));
        let err =
            Credentials::parse(br#"{"claudeAiOauth": {"accessToken": "  "}}"#, None).unwrap_err();
        assert!(matches!(err, ClaudeError::MissingAccessToken));
    }

    #[test]
    fn refresh_is_scheduled_five_minutes_before_expiry() {
        let c = creds();
        assert!(!c.needs_refresh(datetime!(2026-09-02 11:50 UTC)));
        assert!(c.needs_refresh(datetime!(2026-09-02 11:56 UTC)));
        assert!(c.is_expired(datetime!(2026-09-02 12:00 UTC)));
    }

    #[test]
    fn maps_session_weekly_and_model_lanes_with_used_percent_semantics() {
        let usage: UsageResponse = serde_json::from_str(
            r#"{
                "five_hour":  { "utilization": 11.0, "resets_at": "2026-09-01T16:12:00Z" },
                "seven_day":  { "utilization": 2.0,  "resets_at": "2026-09-08T11:00:00Z" },
                "seven_day_opus":   { "utilization": 64.5, "resets_at": "2026-09-08T11:00:00Z" },
                "seven_day_sonnet": { "utilization": 30.0, "resets_at": "2026-09-08T11:00:00Z" },
                "routines": { "utilization": 5.0, "resets_at": "2026-09-08T11:00:00Z" }
            }"#,
        )
        .unwrap();

        let now = datetime!(2026-09-01 12:00 UTC);
        let snap = map_snapshot(&usage, &creds(), None, now).unwrap();

        let primary = snap.primary.unwrap();
        assert_eq!(primary.used_percent, 11.0);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(
            crate::format::duration_short(primary.time_until_reset(now).unwrap()),
            "4h 12m"
        );

        assert_eq!(snap.secondary.unwrap().used_percent, 2.0);
        assert_eq!(
            snap.tertiary.unwrap().used_percent,
            30.0,
            "sonnet wins over opus"
        );
        assert_eq!(
            snap.extra_rate_windows.last().unwrap().id,
            "claude-routines"
        );
        assert_eq!(snap.identity.plan.as_deref(), Some("Max 20×"));
    }

    #[test]
    fn primary_falls_back_through_weekly_lanes() {
        let usage: UsageResponse =
            serde_json::from_str(r#"{ "seven_day_oauth_apps": { "utilization": 42.0 } }"#).unwrap();
        let snap = map_snapshot(&usage, &creds(), None, datetime!(2026-09-01 12:00 UTC)).unwrap();
        assert_eq!(snap.primary.unwrap().used_percent, 42.0);
        assert_eq!(snap.secondary, None);
    }

    #[test]
    fn scoped_limits_become_named_lanes_even_when_inactive() {
        let usage: UsageResponse = serde_json::from_str(
            r#"{
                "five_hour": { "utilization": 1.0 },
                "limits": [
                    { "kind": "weekly_scoped", "group": "weekly", "percent": 77.0, "is_active": false,
                      "resets_at": "2026-09-08T11:00:00Z",
                      "scope": { "model": { "id": "claude-opus-4", "display_name": "Opus" } } }
                ]
            }"#,
        )
        .unwrap();
        let snap = map_snapshot(&usage, &creds(), None, datetime!(2026-09-01 12:00 UTC)).unwrap();
        let lane = &snap.extra_rate_windows[0];
        assert_eq!(lane.id, "claude-opus-4");
        assert_eq!(lane.title, "Opus weekly");
        assert_eq!(lane.window.used_percent, 77.0);
    }

    #[test]
    fn spend_limit_only_accounts_get_a_primary_lane_in_dollars() {
        let usage: UsageResponse = serde_json::from_str(
            r#"{ "extra_usage": { "is_enabled": true, "used_credits": 4300, "monthly_limit": 10000, "currency": "USD" } }"#,
        )
        .unwrap();
        let snap = map_snapshot(&usage, &creds(), None, datetime!(2026-09-01 12:00 UTC)).unwrap();
        let primary = snap.primary.unwrap();
        assert_eq!(primary.used_percent, 43.0);
        assert_eq!(
            primary.reset_description.as_deref(),
            Some("Spend limit: $43.00 / $100.00")
        );
    }

    #[test]
    fn empty_payload_is_an_error_not_a_fake_zero() {
        let usage = UsageResponse::default();
        assert!(matches!(
            map_snapshot(&usage, &creds(), None, datetime!(2026-09-01 12:00 UTC)),
            Err(ClaudeError::NoWindows)
        ));
    }

    #[test]
    fn profile_supplies_account_and_organization() {
        let usage: UsageResponse =
            serde_json::from_str(r#"{ "five_hour": { "utilization": 1.0 } }"#).unwrap();
        let profile: ProfileResponse = serde_json::from_str(
            r#"{ "account": { "email_address": "dev@example.com" }, "organization": { "name": "Acme" } }"#,
        )
        .unwrap();
        let snap = map_snapshot(
            &usage,
            &creds(),
            Some(&profile),
            datetime!(2026-09-01 12:00 UTC),
        )
        .unwrap();
        assert_eq!(snap.identity.account.as_deref(), Some("dev@example.com"));
        assert_eq!(snap.identity.organization.as_deref(), Some("Acme"));
    }
}
