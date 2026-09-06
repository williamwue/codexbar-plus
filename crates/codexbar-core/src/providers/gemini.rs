//! Gemini CLI OAuth state and Cloud Code quota API.
//!
//! Upstream does not obtain quota by executing `gemini`: the CLI owns
//! `%USERPROFILE%\.gemini\oauth_creds.json`; CodexBar reads that state and calls the same
//! Cloud Code APIs. Keeping that boundary avoids launching an interactive TUI.

use std::path::{Path, PathBuf};

use regex::Regex;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::http::{HttpClient, HttpError};
use crate::jwt;
use crate::model::{FetchKind, FetchResult, Identity, RateWindow, UsageSnapshot};

pub const PROVIDER_ID: &str = "gemini";
pub const DISPLAY_NAME: &str = "Gemini";
const QUOTA_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
const CODE_ASSIST_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

#[derive(Debug, thiserror::Error)]
pub enum GeminiError {
    #[error("Gemini CLI is not authenticated; run `gemini` to sign in")]
    NotAuthenticated,
    #[error("Gemini CLI uses unsupported `{0}` authentication; Google OAuth is required")]
    UnsupportedAuth(String),
    #[error("Gemini OAuth credentials cannot be refreshed; update Gemini CLI or set GEMINI_OAUTH_CLIENT_ID and GEMINI_OAUTH_CLIENT_SECRET")]
    MissingOAuthClient,
    #[error("could not read Gemini CLI state: {0}")]
    State(String),
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("could not parse Gemini quota: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Credentials {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expiry_date: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct QuotaResponse {
    buckets: Option<Vec<QuotaBucket>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaBucket {
    remaining_fraction: Option<f64>,
    reset_time: Option<String>,
    model_id: Option<String>,
}

pub fn available() -> bool {
    credentials_path().is_some_and(|path| path.is_file())
}

pub async fn fetch(client: &HttpClient) -> Result<FetchResult, GeminiError> {
    let path = credentials_path().ok_or(GeminiError::NotAuthenticated)?;
    reject_unsupported_auth(path.parent().unwrap_or(Path::new(".")))?;
    let mut credentials = load_credentials(&path)?;
    let now = OffsetDateTime::now_utc();
    if credentials.access_token.as_deref().unwrap_or("").is_empty()
        || credentials
            .expiry_date
            .is_some_and(|value| value <= now.unix_timestamp_nanos() as f64 / 1_000_000.0)
    {
        refresh_credentials(client, &path, &mut credentials).await?;
    }
    let token = credentials
        .access_token
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or(GeminiError::NotAuthenticated)?;
    let (project, plan) = load_code_assist(client, token).await.unwrap_or_default();
    let body = project.map_or_else(|| json!({}), |project| json!({ "project": project }));
    let response = client
        .send(Method::POST, || {
            client
                .raw()
                .post(endpoint("GEMINI_QUOTA_URL", QUOTA_URL))
                .bearer_auth(token)
                .json(&body)
        })
        .await?;
    if let Some(error) = response.error_for_status() {
        return Err(error.into());
    }
    let email = credentials
        .id_token
        .as_deref()
        .and_then(jwt::decode_payload)
        .and_then(|payload| {
            payload
                .get("email")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    Ok(FetchResult {
        usage: parse_quota(&response.body, email, plan, now)?,
        strategy_id: "gemini.oauth-api".into(),
        strategy_kind: FetchKind::Oauth,
    })
}

fn credentials_path() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("GEMINI_HOME").map(PathBuf::from) {
        return Some(directory.join("oauth_creds.json"));
    }
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|home| home.join(".gemini").join("oauth_creds.json"))
}

fn reject_unsupported_auth(gemini_dir: &Path) -> Result<(), GeminiError> {
    let path = gemini_dir.join("settings.json");
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(());
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(());
    };
    let auth = value
        .pointer("/security/auth/selectedType")
        .and_then(Value::as_str);
    match auth {
        Some("api-key") => Err(GeminiError::UnsupportedAuth("API key".into())),
        Some("vertex-ai") => Err(GeminiError::UnsupportedAuth("Vertex AI".into())),
        _ => Ok(()),
    }
}

fn load_credentials(path: &Path) -> Result<Credentials, GeminiError> {
    let bytes = std::fs::read(path).map_err(|error| GeminiError::State(error.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|error| GeminiError::State(error.to_string()))
}

async fn refresh_credentials(
    client: &HttpClient,
    path: &Path,
    credentials: &mut Credentials,
) -> Result<(), GeminiError> {
    let refresh_token = credentials
        .refresh_token
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or(GeminiError::NotAuthenticated)?;
    let (client_id, client_secret) = oauth_client().ok_or(GeminiError::MissingOAuthClient)?;
    let form = [
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("refresh_token", refresh_token.as_str()),
        ("grant_type", "refresh_token"),
    ];
    let response = client
        .send(Method::POST, || {
            client
                .raw()
                .post(endpoint("GEMINI_TOKEN_URL", TOKEN_URL))
                .form(&form)
        })
        .await?;
    if let Some(error) = response.error_for_status() {
        return Err(error.into());
    }
    let value: Value = response.json()?;
    credentials.access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(id_token) = value.get("id_token").and_then(Value::as_str) {
        credentials.id_token = Some(id_token.to_owned());
    }
    if let Some(expires_in) = value.get("expires_in").and_then(Value::as_f64) {
        credentials.expiry_date = Some(
            OffsetDateTime::now_utc().unix_timestamp_nanos() as f64 / 1_000_000.0
                + expires_in * 1000.0,
        );
    }
    let encoded = serde_json::to_vec_pretty(credentials)
        .map_err(|error| GeminiError::State(error.to_string()))?;
    std::fs::write(path, encoded).map_err(|error| GeminiError::State(error.to_string()))?;
    Ok(())
}

fn oauth_client() -> Option<(String, String)> {
    let from_env = std::env::var("GEMINI_OAUTH_CLIENT_ID")
        .ok()
        .zip(std::env::var("GEMINI_OAUTH_CLIENT_SECRET").ok());
    if from_env.is_some() {
        return from_env;
    }
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from)?;
    let candidates = [
        appdata.join("npm/node_modules/@google/gemini-cli/node_modules/@google/gemini-cli-core/dist/src/code_assist/oauth2.js"),
        appdata.join("npm/node_modules/@google/gemini-cli-core/dist/src/code_assist/oauth2.js"),
    ];
    candidates.iter().find_map(|path| {
        let content = std::fs::read_to_string(path).ok()?;
        parse_oauth_client(&content)
    })
}

fn parse_oauth_client(content: &str) -> Option<(String, String)> {
    let id =
        Regex::new(r#"(?:const|let|var)?\s*OAUTH_CLIENT_ID\s*=\s*['\"]([\w.\-]+)['\"]"#).ok()?;
    let secret =
        Regex::new(r#"(?:const|let|var)?\s*OAUTH_CLIENT_SECRET\s*=\s*['\"]([\w\-]+)['\"]"#).ok()?;
    Some((
        id.captures(content)?.get(1)?.as_str().to_owned(),
        secret.captures(content)?.get(1)?.as_str().to_owned(),
    ))
}

async fn load_code_assist(
    client: &HttpClient,
    token: &str,
) -> Result<(Option<String>, Option<String>), GeminiError> {
    let body = json!({ "metadata": { "ideType": "GEMINI_CLI", "pluginType": "GEMINI" } });
    let response = client
        .send(Method::POST, || {
            client
                .raw()
                .post(endpoint("GEMINI_CODE_ASSIST_URL", CODE_ASSIST_URL))
                .bearer_auth(token)
                .json(&body)
        })
        .await?;
    if let Some(error) = response.error_for_status() {
        return Err(error.into());
    }
    let value: Value = response.json()?;
    let project = value
        .get("cloudaicompanionProject")
        .and_then(|project| {
            project
                .as_str()
                .or_else(|| project.get("id").and_then(Value::as_str))
                .or_else(|| project.get("projectId").and_then(Value::as_str))
        })
        .map(str::to_owned);
    let plan = value
        .pointer("/paidTier/name")
        .or_else(|| value.pointer("/currentTier/id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((project, plan))
}

fn parse_quota(
    bytes: &[u8],
    email: Option<String>,
    plan: Option<String>,
    now: OffsetDateTime,
) -> Result<UsageSnapshot, GeminiError> {
    let response: QuotaResponse =
        serde_json::from_slice(bytes).map_err(|error| GeminiError::Parse(error.to_string()))?;
    let buckets = response
        .buckets
        .filter(|buckets| !buckets.is_empty())
        .ok_or_else(|| GeminiError::Parse("no quota buckets in response".into()))?;

    let mut pro: Option<RateWindow> = None;
    let mut flash: Option<RateWindow> = None;
    let mut flash_lite: Option<RateWindow> = None;
    for bucket in buckets {
        let (Some(model), Some(remaining)) = (bucket.model_id, bucket.remaining_fraction) else {
            continue;
        };
        let mut window = RateWindow::new((100.0 - remaining * 100.0).clamp(0.0, 100.0))
            .with_window_minutes(1440);
        window.resets_at = bucket.reset_time.as_deref().and_then(|value| {
            OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
        });
        window.reset_description = bucket.reset_time;
        let model = model.to_ascii_lowercase();
        let slot = if model.contains("flash-lite") {
            &mut flash_lite
        } else if model.contains("flash") {
            &mut flash
        } else if model.contains("pro") {
            &mut pro
        } else {
            continue;
        };
        if slot
            .as_ref()
            .is_none_or(|existing| window.used_percent > existing.used_percent)
        {
            *slot = Some(window);
        }
    }
    if pro.is_none() && flash.is_none() && flash_lite.is_none() {
        return Err(GeminiError::Parse("no recognized model quota".into()));
    }
    let mut snapshot = UsageSnapshot::new(now);
    snapshot.primary = pro;
    snapshot.secondary = flash;
    snapshot.tertiary = flash_lite;
    snapshot.identity = Identity {
        account: email,
        plan,
        ..Identity::default()
    };
    snapshot.source_label = Some("oauth-api".into());
    Ok(snapshot)
}

fn endpoint(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_oauth_bundle_constants() {
        let parsed = parse_oauth_client(
            "const OAUTH_CLIENT_ID = 'client.apps.googleusercontent.com';\nconst OAUTH_CLIENT_SECRET = \"secret-value\";",
        );
        assert_eq!(
            parsed,
            Some((
                "client.apps.googleusercontent.com".into(),
                "secret-value".into()
            ))
        );
    }

    #[test]
    fn groups_models_and_keeps_most_constrained_bucket() {
        let usage = parse_quota(
            br#"{"buckets":[{"modelId":"gemini-2.5-pro","remainingFraction":0.8},{"modelId":"gemini-2.5-pro","remainingFraction":0.4},{"modelId":"gemini-2.5-flash","remainingFraction":0.9},{"modelId":"gemini-2.5-flash-lite","remainingFraction":0.7}]}"#,
            Some("dev@example.com".into()),
            Some("standard-tier".into()),
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 60.0);
        assert_eq!(usage.secondary.unwrap().used_percent, 10.0);
        assert_eq!(usage.tertiary.unwrap().used_percent, 30.0);
    }
}
