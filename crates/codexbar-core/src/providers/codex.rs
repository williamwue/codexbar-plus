//! Codex (ChatGPT/OpenAI Codex) provider.
//!
//! Ports the upstream OAuth strategy:
//! - credentials: `Providers/Codex/CodexOAuth/CodexOAuthCredentials.swift:21-62,268-312,490-518`
//! - refresh:     `Providers/Codex/CodexOAuth/CodexTokenRefresher.swift:6-118`
//! - usage:       `Providers/Codex/CodexOAuth/CodexOAuthUsageFetcher.swift:6-360,430-807`
//! - mapping:     `Providers/Codex/CodexReconciledState.swift:45-64,157-175`
//! - monthly cap: `Providers/Codex/CodexSpendControlsMonthlyUsage.swift:3-83`

use std::path::{Path, PathBuf};

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Duration as TimeDuration, OffsetDateTime};

use crate::http::{HttpClient, HttpError};
use crate::jwt;
use crate::model::{
    Confidence, CreditsSnapshot, DetailRow, FetchKind, FetchResult, Identity, NamedRateWindow,
    RateWindow, UsageSnapshot,
};
use crate::paths;

pub const PROVIDER_ID: &str = "codex";
pub const DISPLAY_NAME: &str = "Codex";

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const USER_AGENT: &str = "CodexBar";
/// Upstream treats an access token as stale 5 minutes before expiry when the credential
/// lives in `CODEX_HOME` (`CodexOAuthCredentials.swift:21-62`).
const REFRESH_SKEW: TimeDuration = TimeDuration::minutes(5);
/// Without an explicit expiry, upstream refreshes when `last_refresh` is older than 8 days.
const STALE_LAST_REFRESH: TimeDuration = TimeDuration::days(8);

#[derive(Debug, thiserror::Error)]
pub enum CodexError {
    #[error("no Codex credentials: {0} not found. Run `codex login`.")]
    MissingAuthFile(PathBuf),
    #[error("could not resolve %USERPROFILE%/.codex")]
    NoHome,
    #[error("auth.json is not valid JSON: {0}")]
    MalformedAuthFile(String),
    #[error("auth.json has no OAuth tokens (only an API key?)")]
    NoOAuthTokens,
    #[error("Codex refresh token rejected ({0}). Run `codex login`.")]
    RefreshRejected(String),
    #[error("Codex request unauthorized. Run `codex login`.")]
    Unauthorized,
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// MARK: - auth.json

/// `~/.codex/auth.json`.
///
/// Upstream parses this as a loose dictionary (`auth_mode` is never consumed), so the
/// model stays permissive: unknown keys are ignored and every token field is optional
/// except `access_token`.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthFile {
    #[serde(default)]
    pub tokens: Option<AuthTokens>,
    #[serde(default, rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,
    #[serde(default)]
    pub last_refresh: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    /// Snake or camel case, matching upstream's dual lookup.
    #[serde(default, alias = "accountId")]
    pub account_id: Option<String>,
}

/// Resolved OAuth credential plus the identity claims mined from `id_token`.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub account_id: Option<String>,
    pub last_refresh: Option<OffsetDateTime>,
    pub email: Option<String>,
    pub plan_from_token: Option<String>,
    pub path: PathBuf,
}

impl Credentials {
    pub fn load() -> Result<Self, CodexError> {
        let path = paths::codex_auth_file().ok_or(CodexError::NoHome)?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, CodexError> {
        if !path.exists() {
            return Err(CodexError::MissingAuthFile(path.to_path_buf()));
        }
        let raw = std::fs::read(path).map_err(|source| CodexError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let file: AuthFile = serde_json::from_slice(&raw)
            .map_err(|e| CodexError::MalformedAuthFile(e.to_string()))?;
        Self::from_auth_file(file, path)
    }

    pub fn from_auth_file(file: AuthFile, path: &Path) -> Result<Self, CodexError> {
        let tokens = file.tokens.ok_or(CodexError::NoOAuthTokens)?;
        let claims: Option<Value> = tokens
            .id_token
            .as_deref()
            .and_then(jwt::decode_payload)
            .or_else(|| jwt::decode_payload(&tokens.access_token));

        let email = claims.as_ref().and_then(jwt::openai_email);
        let plan_from_token = claims.as_ref().and_then(jwt::chatgpt_plan_type);
        let account_id = tokens
            .account_id
            .clone()
            .or_else(|| claims.as_ref().and_then(jwt::chatgpt_account_id));

        Ok(Self {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            id_token: tokens.id_token,
            account_id,
            last_refresh: file.last_refresh.as_deref().and_then(parse_iso8601),
            email,
            plan_from_token,
            path: path.to_path_buf(),
        })
    }

    /// Upstream `needsRefresh` (`CodexOAuthCredentials.swift:21-62`): expiry from the JWT
    /// `exp` claim minus a 5-minute skew, else `last_refresh` older than 8 days, else
    /// refresh because we cannot prove freshness.
    pub fn needs_refresh(&self, now: OffsetDateTime) -> bool {
        if let Some(exp) = self.access_token_expiry() {
            return now + REFRESH_SKEW >= exp;
        }
        match self.last_refresh {
            Some(last) => now - last > STALE_LAST_REFRESH,
            None => true,
        }
    }

    fn access_token_expiry(&self) -> Option<OffsetDateTime> {
        let payload = jwt::decode_payload(&self.access_token)?;
        let exp = payload.get("exp")?.as_i64()?;
        OffsetDateTime::from_unix_timestamp(exp).ok()
    }
}

fn parse_iso8601(raw: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).ok()
}

// MARK: - token refresh

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
    scope: &'a str,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// POST `https://auth.openai.com/oauth/token` with the JSON body upstream sends
/// (`CodexTokenRefresher.swift:6-118`), then rewrites `auth.json` in place.
pub async fn refresh(client: &HttpClient, creds: &Credentials) -> Result<Credentials, CodexError> {
    let refresh_token = creds
        .refresh_token
        .as_deref()
        .ok_or_else(|| CodexError::RefreshRejected("no refresh_token in auth.json".into()))?;

    let body = RefreshRequest {
        client_id: OAUTH_CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token,
        scope: "openid profile email",
    };

    let resp = client
        .send(Method::POST, || {
            client
                .raw()
                .post(TOKEN_ENDPOINT)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .json(&body)
        })
        .await?;

    if !resp.status.is_success() {
        let parsed: Option<OAuthErrorBody> = serde_json::from_slice(&resp.body).ok();
        let code = parsed
            .as_ref()
            .and_then(|b| b.error.clone())
            .unwrap_or_else(|| format!("HTTP {}", resp.status.as_u16()));
        let detail = parsed.and_then(|b| b.error_description).unwrap_or_default();
        return Err(CodexError::RefreshRejected(if detail.is_empty() {
            code
        } else {
            format!("{code}: {detail}")
        }));
    }

    let refreshed: RefreshResponse = resp.json()?;
    let mut next = creds.clone();
    next.access_token = refreshed.access_token;
    if let Some(rt) = refreshed.refresh_token {
        next.refresh_token = Some(rt);
    }
    if refreshed.id_token.is_some() {
        next.id_token = refreshed.id_token;
    }
    next.last_refresh = Some(OffsetDateTime::now_utc());
    persist(&next)?;
    Ok(next)
}

/// Rewrites only the fields we own, preserving unknown keys in `auth.json`.
fn persist(creds: &Credentials) -> Result<(), CodexError> {
    let existing = std::fs::read(&creds.path).map_err(|source| CodexError::Io {
        path: creds.path.clone(),
        source,
    })?;
    let mut root: Value = serde_json::from_slice(&existing)
        .map_err(|e| CodexError::MalformedAuthFile(e.to_string()))?;

    let tokens = root
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or(CodexError::NoOAuthTokens)?;
    tokens.insert(
        "access_token".into(),
        Value::String(creds.access_token.clone()),
    );
    if let Some(rt) = &creds.refresh_token {
        tokens.insert("refresh_token".into(), Value::String(rt.clone()));
    }
    if let Some(id) = &creds.id_token {
        tokens.insert("id_token".into(), Value::String(id.clone()));
    }
    if let Some(last) = creds.last_refresh {
        let formatted = last
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        root.as_object_mut()
            .map(|o| o.insert("last_refresh".into(), Value::String(formatted)));
    }

    let serialized = serde_json::to_vec_pretty(&root)
        .map_err(|e| CodexError::MalformedAuthFile(e.to_string()))?;
    let tmp = creds.path.with_extension("json.codexbar-tmp");
    std::fs::write(&tmp, &serialized).map_err(|source| CodexError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &creds.path).map_err(|source| CodexError::Io {
        path: creds.path.clone(),
        source,
    })?;
    crate::secure::restrict_to_current_user(&creds.path);
    Ok(())
}

// MARK: - usage wire models
//
// Verified against a live `GET https://chatgpt.com/backend-api/wham/usage` response
// (Pro account, 2026-09-01). Upstream has no fixture for this endpoint, so the shape
// below is ground truth from the wire, with upstream's decoding quirks preserved:
// `credits.balance` arrives as a STRING, and `additional_rate_limits[].rate_limit` is a
// full rate-limit object with its own primary/secondary windows, not a bare window.

#[derive(Debug, Clone, Deserialize)]
pub struct UsageResponse {
    #[serde(default, alias = "accountId")]
    pub account_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    /// The usage payload carries the account email directly; no JWT parsing needed.
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitDetails>,
    #[serde(default)]
    pub code_review_rate_limit: Option<RateLimitDetails>,
    #[serde(default)]
    pub credits: Option<CreditDetails>,
    #[serde(default, alias = "individualLimit")]
    pub individual_limit: Option<SpendControlLimitSnapshot>,
    #[serde(default, alias = "spendControl")]
    pub spend_control: Option<SpendControl>,
    #[serde(default)]
    pub additional_rate_limits: Option<Vec<AdditionalRateLimit>>,
    /// Inline reset-credit counters; the dedicated endpoint adds per-credit detail.
    #[serde(default)]
    pub rate_limit_reset_credits: Option<ResetCreditCounters>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResetCreditCounters {
    #[serde(default)]
    pub available_count: Option<i64>,
    #[serde(default)]
    pub applicable_available_count: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitDetails {
    #[serde(default)]
    pub primary_window: Option<WindowSnapshot>,
    #[serde(default)]
    pub secondary_window: Option<WindowSnapshot>,
    #[serde(default, alias = "individualLimit")]
    pub individual_limit: Option<SpendControlLimitSnapshot>,
}

/// `used_percent` is USED percent; `reset_at` is Unix seconds; `limit_window_seconds`
/// converts to `window_minutes` by integer division (upstream
/// `CodexAdditionalRateLimitMapper.swift:75-82`). `reset_after_seconds` is the relative
/// form the API also sends and is used when `reset_at` is missing or non-positive.
#[derive(Debug, Clone, Deserialize)]
pub struct WindowSnapshot {
    #[serde(default)]
    pub used_percent: Option<f64>,
    #[serde(default)]
    pub reset_at: Option<i64>,
    #[serde(default)]
    pub reset_after_seconds: Option<i64>,
    #[serde(default)]
    pub limit_window_seconds: Option<i64>,
    #[serde(default)]
    pub allowed: Option<bool>,
    #[serde(default)]
    pub limit_reached: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpendControl {
    #[serde(default, alias = "individualLimit")]
    pub individual_limit: Option<SpendControlLimitSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpendControlLimitSnapshot {
    #[serde(default)]
    pub limit: Option<f64>,
    #[serde(default)]
    pub used: Option<f64>,
    #[serde(default, alias = "remainingPercent")]
    pub remaining_percent: Option<f64>,
    #[serde(default, alias = "reset_at", alias = "resetsAt")]
    pub resets_at: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreditDetails {
    #[serde(default)]
    pub has_credits: Option<bool>,
    #[serde(default)]
    pub unlimited: Option<bool>,
    /// The live API sends this as a JSON string (`"0"`); upstream accepts both
    /// (`CodexOAuthUsageFetcher.swift:351-364`).
    #[serde(default, deserialize_with = "de_number_or_string")]
    pub balance: Option<f64>,
    #[serde(default)]
    pub overage_limit_reached: Option<bool>,
}

/// Accepts `12.5`, `"12.5"`, `null` and absent.
fn de_number_or_string<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(Value::String(s)) => Ok(s.trim().parse::<f64>().ok()),
        Some(_) => Ok(None),
    }
}

/// A model-specific limit (e.g. GPT-5.3-Codex-Spark). `rate_limit` is a nested
/// rate-limit object with its own windows, not a bare window.
#[derive(Debug, Clone, Deserialize)]
pub struct AdditionalRateLimit {
    #[serde(default)]
    pub limit_name: Option<String>,
    #[serde(default)]
    pub metered_feature: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitDetails>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResetCreditsResponse {
    #[serde(default)]
    pub credits: Option<Vec<ResetCredit>>,
    #[serde(default)]
    pub available_count: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResetCredit {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub reset_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub granted_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

// MARK: - fetch

/// Base URL override for tests and self-hosted proxies (upstream reads it from settings).
pub fn base_url() -> String {
    std::env::var("CODEXBAR_CODEX_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// Upstream picks `/wham/usage` for the `backend-api` origin and `/api/codex/usage`
/// otherwise (`CodexOAuthUsageFetcher.swift:391-395`).
fn usage_url(base: &str) -> String {
    if base.contains("/backend-api") {
        format!("{}/wham/usage", base.trim_end_matches('/'))
    } else {
        format!("{}/api/codex/usage", base.trim_end_matches('/'))
    }
}

pub async fn fetch_usage(
    client: &HttpClient,
    creds: &Credentials,
) -> Result<UsageResponse, CodexError> {
    let base = base_url();
    let url = usage_url(&base);
    let token = creds.access_token.clone();
    let account = creds.account_id.clone();

    let resp = client
        .send(Method::GET, || {
            let mut req = client
                .raw()
                .get(&url)
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .header(reqwest::header::ACCEPT, "application/json");
            if let Some(id) = &account {
                req = req.header("ChatGPT-Account-Id", id);
            }
            req
        })
        .await?;

    match resp.error_for_status() {
        None => Ok(resp.json()?),
        Some(HttpError::Unauthorized(_)) => Err(CodexError::Unauthorized),
        Some(err) => Err(CodexError::Http(err)),
    }
}

/// `/wham/rate-limit-reset-credits` — note the differing header spellings upstream uses
/// on this endpoint (`ChatGPT-Account-ID`, `OpenAI-Beta`, `originator`).
pub async fn fetch_reset_credits(
    client: &HttpClient,
    creds: &Credentials,
) -> Result<ResetCreditsResponse, CodexError> {
    let base = base_url();
    let url = format!(
        "{}/wham/rate-limit-reset-credits",
        base.trim_end_matches('/')
    );
    let token = creds.access_token.clone();
    let account = creds.account_id.clone();

    let resp = client
        .send(Method::GET, || {
            let mut req = client
                .raw()
                .get(&url)
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .header(reqwest::header::ACCEPT, "application/json")
                .header("OpenAI-Beta", "codex-1")
                .header("originator", "Codex Desktop");
            if let Some(id) = &account {
                req = req.header("ChatGPT-Account-ID", id);
            }
            req
        })
        .await?;

    match resp.error_for_status() {
        None => Ok(resp.json()?),
        Some(HttpError::Unauthorized(_)) => Err(CodexError::Unauthorized),
        Some(err) => Err(CodexError::Http(err)),
    }
}

// MARK: - mapping

fn map_window(snapshot: &WindowSnapshot, now: OffsetDateTime) -> Option<RateWindow> {
    let used = snapshot.used_percent?;
    let mut window = RateWindow::new(used);
    if let Some(seconds) = snapshot.limit_window_seconds.filter(|s| *s > 0) {
        window.window_minutes = Some((seconds / 60) as u32);
    }
    window.resets_at = match snapshot.reset_at.filter(|v| *v > 0) {
        Some(at) => OffsetDateTime::from_unix_timestamp(at).ok(),
        // The API also reports the window relatively; use it when the absolute form is absent.
        None => snapshot
            .reset_after_seconds
            .filter(|v| *v > 0)
            .map(|s| now + TimeDuration::seconds(s)),
    };
    Some(window)
}

/// Stable ids/titles for the Spark lanes, matching upstream so settings and layouts
/// keep working (`CodexAdditionalRateLimitMapper.swift:13-16,105-119`).
const SPARK_WINDOW_ID: &str = "codex-spark";
const SPARK_WEEKLY_WINDOW_ID: &str = "codex-spark-weekly";

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_dash = false;
    for ch in value.to_lowercase().chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn first_non_empty(values: &[Option<&String>]) -> Option<String> {
    values
        .iter()
        .flatten()
        .map(|v| v.trim())
        .find(|v| !v.is_empty())
        .map(str::to_owned)
}

fn is_spark(entry: &AdditionalRateLimit) -> bool {
    [entry.limit_name.as_ref(), entry.metered_feature.as_ref()]
        .into_iter()
        .flatten()
        .any(|v| v.to_lowercase().contains("spark"))
}

/// A window ≤6h is the 5-hour lane, ≥6d is the weekly lane, otherwise keep the
/// positional fallback (upstream `sparkWindowKind`).
fn spark_lane(snapshot: &WindowSnapshot, fallback_weekly: bool) -> (&'static str, &'static str) {
    let minutes = snapshot.limit_window_seconds.unwrap_or(0) / 60;
    let weekly = if minutes > 0 && minutes <= 6 * 60 {
        false
    } else if minutes >= 6 * 24 * 60 {
        true
    } else {
        fallback_weekly
    };
    if weekly {
        (SPARK_WEEKLY_WINDOW_ID, "Codex Spark Weekly")
    } else {
        (SPARK_WINDOW_ID, "Codex Spark 5-hour")
    }
}

/// `additional_rate_limits` → named lanes (upstream `CodexAdditionalRateLimitMapper`).
fn map_additional_limits(
    entries: &[AdditionalRateLimit],
    now: OffsetDateTime,
) -> Vec<NamedRateWindow> {
    let mut used_ids: Vec<String> = Vec::new();
    let mut out = Vec::new();

    for entry in entries {
        let Some(details) = entry.rate_limit.as_ref() else {
            continue;
        };

        if is_spark(entry) {
            // Spark reports both its 5-hour and weekly lanes; surface each once.
            for (snapshot, fallback_weekly) in [
                (details.primary_window.as_ref(), false),
                (details.secondary_window.as_ref(), true),
            ] {
                let Some(snapshot) = snapshot else { continue };
                let Some(window) = map_window(snapshot, now) else {
                    continue;
                };
                let (id, title) = spark_lane(snapshot, fallback_weekly);
                if used_ids.iter().any(|existing| existing == id) {
                    continue;
                }
                used_ids.push(id.to_string());
                out.push(NamedRateWindow {
                    id: id.to_string(),
                    title: title.to_string(),
                    window,
                });
            }
            continue;
        }

        // Other model limits report utilization in the primary window.
        let snapshot = details
            .primary_window
            .as_ref()
            .or(details.secondary_window.as_ref());
        let Some(window) = snapshot.and_then(|s| map_window(s, now)) else {
            continue;
        };
        let Some(source) =
            first_non_empty(&[entry.metered_feature.as_ref(), entry.limit_name.as_ref()])
        else {
            continue;
        };
        let slugged = slug(&source);
        if slugged.is_empty() {
            continue;
        }
        let id = format!("codex-{slugged}");
        if used_ids.contains(&id) {
            continue;
        }
        used_ids.push(id.clone());
        let title = first_non_empty(&[entry.limit_name.as_ref(), entry.metered_feature.as_ref()])
            .unwrap_or_else(|| "Codex extra limit".to_string());
        out.push(NamedRateWindow { id, title, window });
    }

    out
}

/// `UsageResponse` → canonical snapshot.
///
/// primary = `rate_limit.primary_window`, secondary = `rate_limit.secondary_window`
/// (upstream `CodexReconciledState.swift:45-64`). Credits come from `credits.balance`;
/// the monthly cap is resolved root → rate_limit → spend_control.
pub fn map_snapshot(
    response: &UsageResponse,
    creds: &Credentials,
    reset_credits: Option<&ResetCreditsResponse>,
    now: OffsetDateTime,
) -> UsageSnapshot {
    let mut snapshot = UsageSnapshot::new(now);

    if let Some(limits) = &response.rate_limit {
        snapshot.primary = limits
            .primary_window
            .as_ref()
            .and_then(|w| map_window(w, now));
        snapshot.secondary = limits
            .secondary_window
            .as_ref()
            .and_then(|w| map_window(w, now));
    }

    if let Some(limit) = resolve_individual_limit(response) {
        if let Some(window) = map_spend_limit(limit) {
            snapshot.tertiary = Some(window);
        }
    }

    if let Some(entries) = response.additional_rate_limits.as_deref() {
        snapshot.extra_rate_windows = map_additional_limits(entries, now);
    }

    if let Some(review) = response
        .code_review_rate_limit
        .as_ref()
        .and_then(|d| d.primary_window.as_ref())
        .and_then(|w| map_window(w, now))
    {
        snapshot.extra_rate_windows.push(NamedRateWindow {
            id: "codex-code-review".into(),
            title: "Code review".into(),
            window: review,
        });
    }

    if let Some(credits) = &response.credits {
        if credits.unlimited == Some(true) {
            snapshot.push_rows(vec![DetailRow {
                label: "Credits".into(),
                value: "Unlimited".into(),
                hint: None,
            }]);
        } else if let Some(balance) = credits.balance {
            snapshot.credits = Some(CreditsSnapshot {
                remaining: balance,
                currency: None,
                events: Vec::new(),
                updated_at: Some(now),
            });
        }
    }

    // The usage payload carries inline counters; the dedicated endpoint adds expiry detail.
    let inline_available = response
        .rate_limit_reset_credits
        .as_ref()
        .and_then(|c| c.available_count)
        .unwrap_or(0);
    let available = reset_credits
        .and_then(|r| r.available_count)
        .unwrap_or(inline_available)
        .max(0);
    if available > 0 {
        snapshot.push_rows(vec![DetailRow {
            label: "Limit Reset Credits".into(),
            value: format!("{available} available"),
            hint: reset_credits.and_then(|r| next_expiry_hint(r, now)),
        }]);
    }

    snapshot.identity = Identity {
        account: response.email.clone().or_else(|| creds.email.clone()),
        plan: response
            .plan_type
            .clone()
            .or_else(|| creds.plan_from_token.clone())
            .map(prettify_plan),
        account_id: response
            .account_id
            .clone()
            .or_else(|| creds.account_id.clone()),
        organization: None,
    };
    snapshot.confidence = if snapshot.primary.is_some() {
        Confidence::Exact
    } else {
        Confidence::IdentityOnly
    };
    snapshot.source_label = Some("oauth".into());
    snapshot
}

fn resolve_individual_limit(response: &UsageResponse) -> Option<&SpendControlLimitSnapshot> {
    response
        .individual_limit
        .as_ref()
        .or_else(|| response.rate_limit.as_ref()?.individual_limit.as_ref())
        .or_else(|| response.spend_control.as_ref()?.individual_limit.as_ref())
}

/// Upstream computes `remaining_percent = 100 - used/limit*100` clamped to `0..=100`
/// (`CodexSpendControlsMonthlyUsage.swift` mapping) — we store the used side.
fn map_spend_limit(limit: &SpendControlLimitSnapshot) -> Option<RateWindow> {
    if let Some(remaining) = limit.remaining_percent {
        return Some(RateWindow::new((100.0 - remaining).clamp(0.0, 100.0)));
    }
    let cap = limit.limit.filter(|v| *v > 0.0)?;
    let used = limit.used.unwrap_or(0.0).max(0.0);
    Some(RateWindow::new(((used / cap) * 100.0).clamp(0.0, 100.0)))
}

fn next_expiry_hint(resets: &ResetCreditsResponse, now: OffsetDateTime) -> Option<String> {
    let soonest = resets
        .credits
        .iter()
        .flatten()
        .filter(|c| c.status.as_deref() != Some("redeemed"))
        .filter_map(|c| c.expires_at.as_deref().and_then(parse_iso8601))
        .filter(|at| *at > now)
        .min()?;
    let delta = soonest - now;
    Some(format!(
        "next expires in {}",
        crate::format::duration_short(delta)
    ))
}

fn prettify_plan(raw: String) -> String {
    match raw.as_str() {
        "plus" => "Plus".into(),
        "pro" => "Pro".into(),
        "free" => "Free".into(),
        "team" => "Team".into(),
        "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        "edu" | "education" => "Education".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Full provider run: load credentials, refresh if stale, fetch usage (+ reset credits),
/// map to the canonical snapshot.
pub async fn fetch(client: &HttpClient) -> Result<FetchResult, CodexError> {
    let mut creds = Credentials::load()?;
    let now = OffsetDateTime::now_utc();
    if creds.needs_refresh(now) && creds.refresh_token.is_some() {
        match refresh(client, &creds).await {
            Ok(next) => creds = next,
            Err(err) => {
                tracing::warn!(error = %err, "codex token refresh failed; trying existing token")
            }
        }
    }

    let usage = match fetch_usage(client, &creds).await {
        Ok(usage) => usage,
        Err(CodexError::Unauthorized) if creds.refresh_token.is_some() => {
            // A 401 on a token we thought was fresh means rotate-then-retry once.
            creds = refresh(client, &creds).await?;
            fetch_usage(client, &creds).await?
        }
        Err(err) => return Err(err),
    };

    let reset_credits = match fetch_reset_credits(client, &creds).await {
        Ok(v) => Some(v),
        Err(err) => {
            tracing::debug!(error = %err, "codex reset credits unavailable");
            None
        }
    };

    Ok(FetchResult {
        usage: map_snapshot(
            &usage,
            &creds,
            reset_credits.as_ref(),
            OffsetDateTime::now_utc(),
        ),
        strategy_id: "codex.oauth".into(),
        strategy_kind: FetchKind::Oauth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const SAMPLE: &str = r#"{
        "account_id": "acct-1",
        "plan_type": "plus",
        "rate_limit": {
            "primary_window": { "used_percent": 11, "reset_at": 1788000000, "limit_window_seconds": 18000 },
            "secondary_window": { "used_percent": 2, "reset_at": 1788400000, "limit_window_seconds": 604800 }
        },
        "credits": { "has_credits": true, "unlimited": false, "balance": 12.5 },
        "additional_rate_limits": [
            { "limit_name": "Code review", "metered_feature": "code_review",
              "rate_limit": {
                  "primary_window": { "used_percent": 40, "reset_at": 1788000000, "limit_window_seconds": 86400 }
              } }
        ]
    }"#;

    /// Captured from a live `GET /backend-api/wham/usage` (Pro account), ids redacted.
    const LIVE_FIXTURE: &str = include_str!("../../tests/fixtures/codex-wham-usage.json");

    fn creds() -> Credentials {
        Credentials {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            id_token: None,
            account_id: Some("acct-1".into()),
            last_refresh: Some(datetime!(2026-09-01 00:00 UTC)),
            email: Some("dev@example.com".into()),
            plan_from_token: None,
            path: PathBuf::from("auth.json"),
        }
    }

    #[test]
    fn maps_windows_percent_units_and_reset_instants() {
        let parsed: UsageResponse = serde_json::from_str(SAMPLE).unwrap();
        let snap = map_snapshot(&parsed, &creds(), None, datetime!(2026-09-01 12:00 UTC));

        let primary = snap.primary.expect("primary window");
        assert_eq!(primary.used_percent, 11.0);
        assert_eq!(primary.remaining_percent(), 89.0);
        assert_eq!(primary.window_minutes, Some(300), "18000s == 5h == 300min");
        assert_eq!(
            primary.resets_at,
            Some(OffsetDateTime::from_unix_timestamp(1788000000).unwrap())
        );

        let secondary = snap.secondary.expect("weekly window");
        assert_eq!(secondary.window_minutes, Some(10080), "604800s == 7d");

        assert_eq!(snap.extra_rate_windows.len(), 1);
        assert_eq!(snap.extra_rate_windows[0].id, "codex-code-review");
        assert_eq!(snap.extra_rate_windows[0].window.used_percent, 40.0);

        assert_eq!(snap.credits.as_ref().unwrap().remaining, 12.5);
        assert_eq!(snap.identity.plan.as_deref(), Some("Plus"));
        assert_eq!(snap.identity.account.as_deref(), Some("dev@example.com"));
        assert_eq!(snap.confidence, Confidence::Exact);
    }

    #[test]
    fn decodes_the_live_wham_usage_payload_end_to_end() {
        let parsed: UsageResponse =
            serde_json::from_str(LIVE_FIXTURE).expect("live payload must decode");
        let now = datetime!(2026-09-01 12:00 UTC);
        let snap = map_snapshot(&parsed, &creds(), None, now);

        // Pro accounts report a single weekly primary window and no secondary lane.
        let primary = snap.primary.clone().expect("primary window");
        assert_eq!(primary.used_percent, 24.0);
        assert_eq!(primary.window_minutes, Some(10080));
        assert!(primary.resets_at.is_some());
        assert!(snap.secondary.is_none());

        // `credits.balance` is the string "0" on the wire.
        assert_eq!(snap.credits.as_ref().map(|c| c.remaining), Some(0.0));

        // Spark contributes both of its lanes with stable ids.
        let ids: Vec<&str> = snap
            .extra_rate_windows
            .iter()
            .map(|w| w.id.as_str())
            .collect();
        assert_eq!(ids, vec!["codex-spark", "codex-spark-weekly"]);
        assert_eq!(snap.extra_rate_windows[0].window.window_minutes, Some(300));
        assert_eq!(
            snap.extra_rate_windows[1].window.window_minutes,
            Some(10080)
        );

        // Inline reset-credit counters surface without the dedicated endpoint.
        let row = snap
            .detail_rows()
            .find(|d| d.label == "Limit Reset Credits")
            .expect("reset credit row");
        assert_eq!(row.value, "1 available");

        assert_eq!(snap.identity.plan.as_deref(), Some("Pro"));
        assert_eq!(snap.identity.account.as_deref(), Some("dev@example.com"));
    }

    #[test]
    fn unlimited_credits_render_as_detail_not_balance() {
        let parsed: UsageResponse = serde_json::from_str(
            r#"{ "credits": { "unlimited": true }, "rate_limit": { "primary_window": { "used_percent": 5 } } }"#,
        )
        .unwrap();
        let snap = map_snapshot(&parsed, &creds(), None, datetime!(2026-09-01 12:00 UTC));
        assert!(snap.credits.is_none());
        assert_eq!(snap.detail_rows().next().unwrap().value, "Unlimited");
    }

    #[test]
    fn identity_only_when_provider_returns_no_windows() {
        let parsed: UsageResponse = serde_json::from_str(r#"{ "plan_type": "pro" }"#).unwrap();
        let snap = map_snapshot(&parsed, &creds(), None, datetime!(2026-09-01 12:00 UTC));
        assert_eq!(snap.confidence, Confidence::IdentityOnly);
        assert!(snap.primary.is_none());
    }

    #[test]
    fn spend_control_limit_prefers_remaining_percent_then_ratio() {
        let with_remaining: UsageResponse =
            serde_json::from_str(r#"{ "individual_limit": { "remaining_percent": 25 } }"#).unwrap();
        let snap = map_snapshot(
            &with_remaining,
            &creds(),
            None,
            datetime!(2026-09-01 12:00 UTC),
        );
        assert_eq!(snap.tertiary.unwrap().used_percent, 75.0);

        let with_amounts: UsageResponse = serde_json::from_str(
            r#"{ "spend_control": { "individual_limit": { "limit": 200, "used": 50 } } }"#,
        )
        .unwrap();
        let snap = map_snapshot(
            &with_amounts,
            &creds(),
            None,
            datetime!(2026-09-01 12:00 UTC),
        );
        assert_eq!(snap.tertiary.unwrap().used_percent, 25.0);
    }

    #[test]
    fn reset_credits_detail_reports_soonest_expiry() {
        let parsed: UsageResponse = serde_json::from_str(SAMPLE).unwrap();
        let resets: ResetCreditsResponse = serde_json::from_str(
            r#"{ "available_count": 1, "credits": [
                { "id": "a", "status": "available", "expires_at": "2026-09-29T09:00:00Z" },
                { "id": "b", "status": "redeemed",  "expires_at": "2026-09-02T09:00:00Z" }
            ] }"#,
        )
        .unwrap();
        let snap = map_snapshot(
            &parsed,
            &creds(),
            Some(&resets),
            datetime!(2026-09-01 12:00 UTC),
        );
        let row = snap
            .detail_rows()
            .find(|d| d.label == "Limit Reset Credits")
            .unwrap();
        assert_eq!(row.value, "1 available");
        assert!(
            row.hint.as_deref().unwrap().contains("27d"),
            "{:?}",
            row.hint
        );
    }

    #[test]
    fn usage_path_depends_on_origin_shape() {
        assert_eq!(
            usage_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(
            usage_url("https://proxy.internal/"),
            "https://proxy.internal/api/codex/usage"
        );
    }

    #[test]
    fn auth_file_parses_real_shape_and_ignores_unknown_keys() {
        let raw = r#"{
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": { "id_token": "a.b.c", "access_token": "at", "refresh_token": "rt", "account_id": "acct-1" },
            "last_refresh": "2026-08-30T10:00:00Z",
            "future_field": 1
        }"#;
        let file: AuthFile = serde_json::from_str(raw).unwrap();
        let creds = Credentials::from_auth_file(file, Path::new("auth.json")).unwrap();
        assert_eq!(creds.access_token, "at");
        assert_eq!(creds.account_id.as_deref(), Some("acct-1"));
        assert_eq!(creds.last_refresh, Some(datetime!(2026-08-30 10:00 UTC)));
    }

    #[test]
    fn refresh_decision_uses_expiry_then_last_refresh_age() {
        let now = datetime!(2026-09-01 12:00 UTC);

        let mut fresh = creds();
        fresh.last_refresh = Some(now - TimeDuration::days(1));
        assert!(
            !fresh.needs_refresh(now),
            "1-day-old credential is still usable"
        );

        let mut stale = creds();
        stale.last_refresh = Some(now - TimeDuration::days(9));
        assert!(stale.needs_refresh(now), "older than 8 days must refresh");

        let mut unknown = creds();
        unknown.last_refresh = None;
        assert!(
            unknown.needs_refresh(now),
            "unprovable freshness must refresh"
        );
    }
}
