//! AWS Bedrock monthly spend.
//!
//! Named profiles are resolved through `aws configure export-credentials`, preserving
//! AWS SSO, assume-role and `credential_process` behavior. Cost Explorer requests are
//! SigV4-signed locally; secrets never enter command arguments or logs.

use std::collections::HashSet;

use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE, HOST};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::{Date, Month, OffsetDateTime};

use super::cli;
use crate::http::{HttpClient, HttpError};
use crate::model::{CostSnapshot, FetchKind, FetchResult, Identity, RateWindow, UsageSnapshot};
use crate::settings::Settings;

pub const PROVIDER_ID: &str = "bedrock";
pub const DISPLAY_NAME: &str = "AWS Bedrock";
const AWS_OVERRIDE: &str = "AWS_CLI_PATH";
const COST_URL: &str = "https://ce.us-east-1.amazonaws.com/";
const TARGET: &str = "AWSInsightsIndexService.GetCostAndUsage";
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ExportedCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

#[derive(Debug, Clone)]
struct Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum BedrockError {
    #[error("AWS credentials are not configured")]
    MissingCredentials,
    #[error("AWS CLI was not found; install AWS CLI v2 or set AWS_CLI_PATH")]
    BinaryNotFound,
    #[error("AWS profile `{0}` has expired; run `aws sso login --profile {0}`")]
    ProfileExpired(String),
    #[error(transparent)]
    Subprocess(#[from] crate::subprocess::SubprocessError),
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("could not parse AWS response: {0}")]
    Parse(String),
}

pub fn available(settings: &Settings) -> bool {
    direct_credentials(settings).is_some()
        || profile(settings).is_some_and(|_| {
            let environment = cli::environment();
            cli::resolve_binary("aws", AWS_OVERRIDE, &environment).is_some()
        })
}

pub async fn fetch(client: &HttpClient, settings: &Settings) -> Result<FetchResult, BedrockError> {
    let credentials = resolve_credentials(settings).await?;
    let now = OffsetDateTime::now_utc();
    let start = Date::from_calendar_date(now.year(), now.month(), 1)
        .map_err(|error| BedrockError::Parse(error.to_string()))?;
    let (next_year, next_month) = if now.month() == Month::December {
        (now.year() + 1, Month::January)
    } else {
        (
            now.year(),
            Month::try_from(now.month() as u8 + 1).expect("valid next month"),
        )
    };
    let end = Date::from_calendar_date(next_year, next_month, 1)
        .map_err(|error| BedrockError::Parse(error.to_string()))?;
    let query_end = now.date() + time::Duration::days(1);
    let spend = fetch_cost_pages(client, &credentials, start, query_end, now).await?;
    let budget = settings
        .setting(PROVIDER_ID, "monthlyBudget")
        .or_else(|| std::env::var("BEDROCK_MONTHLY_BUDGET").ok())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0);
    let mut snapshot = UsageSnapshot::new(now);
    let reset = end.with_hms(0, 0, 0).expect("midnight").assume_utc();
    if let Some(budget) = budget {
        let mut window = RateWindow::new((spend / budget * 100.0).clamp(0.0, 100.0));
        window.resets_at = Some(reset);
        window.reset_description = Some("Monthly budget".into());
        snapshot.primary = Some(window);
    }
    snapshot.cost = Some(CostSnapshot {
        used: spend,
        limit: budget,
        currency: "USD".into(),
        period: Some("Monthly".into()),
        resets_at: Some(reset),
        balance: budget.map(|value| value - spend),
    });
    snapshot.identity = Identity {
        plan: Some(match budget {
            Some(limit) => format!("Spend: ${spend:.2} - Budget: ${limit:.2}"),
            None => format!("Spend: ${spend:.2}"),
        }),
        ..Identity::default()
    };
    snapshot.source_label = Some("aws-cost-explorer".into());
    Ok(FetchResult {
        usage: snapshot,
        strategy_id: "bedrock.aws-cli-api".into(),
        strategy_kind: FetchKind::ApiToken,
    })
}
async fn fetch_cost_pages(
    client: &HttpClient,
    credentials: &Credentials,
    start: Date,
    end: Date,
    now: OffsetDateTime,
) -> Result<f64, BedrockError> {
    let endpoint = std::env::var("BEDROCK_COST_EXPLORER_URL").unwrap_or_else(|_| COST_URL.into());
    let mut next_page_token: Option<String> = None;
    let mut seen = HashSet::new();
    let mut total = 0.0;
    loop {
        let mut request = json!({
            "TimePeriod": { "Start": start.to_string(), "End": end.to_string() },
            "Granularity": "MONTHLY",
            "Metrics": ["UnblendedCost"],
            "GroupBy": [{ "Type": "DIMENSION", "Key": "SERVICE" }]
        });
        if let Some(token) = &next_page_token {
            request["NextPageToken"] = Value::String(token.clone());
        }
        let body =
            serde_json::to_vec(&request).map_err(|error| BedrockError::Parse(error.to_string()))?;
        let headers = signed_headers(&endpoint, &body, credentials, now)?;
        let response = client
            .send(Method::POST, || {
                client
                    .raw()
                    .post(&endpoint)
                    .headers(headers.clone())
                    .body(body.clone())
            })
            .await?;
        if response.status.as_u16() == 400 && is_data_unavailable(&response.body) {
            return Ok(total);
        }
        if let Some(error) = response.error_for_status() {
            return Err(error.into());
        }
        total += parse_cost(&response.body)?;
        next_page_token = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|value| {
                value
                    .get("NextPageToken")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|token| !token.trim().is_empty());
        let Some(token) = &next_page_token else {
            return Ok(total);
        };
        if !seen.insert(token.clone()) {
            return Err(BedrockError::Parse(
                "Cost Explorer returned a repeated NextPageToken".into(),
            ));
        }
    }
}

async fn resolve_credentials(settings: &Settings) -> Result<Credentials, BedrockError> {
    if let Some(credentials) = direct_credentials(settings) {
        return Ok(credentials);
    }
    let profile = profile(settings).ok_or(BedrockError::MissingCredentials)?;
    let environment = cli::environment();
    let binary = cli::resolve_binary("aws", AWS_OVERRIDE, &environment)
        .ok_or(BedrockError::BinaryNotFound)?;
    resolve_profile_credentials(binary, environment, profile).await
}

async fn resolve_profile_credentials(
    binary: std::path::PathBuf,
    environment: std::collections::HashMap<String, String>,
    profile: String,
) -> Result<Credentials, BedrockError> {
    let arguments = vec![
        "configure".into(),
        "export-credentials".into(),
        "--profile".into(),
        profile.clone(),
        "--format".into(),
        "process".into(),
    ];
    let result = match cli::run(binary, arguments, environment).await {
        Err(crate::subprocess::SubprocessError::NonZeroExit { stderr, .. })
            if stderr.to_ascii_lowercase().contains("expired")
                || stderr.to_ascii_lowercase().contains("sso login") =>
        {
            return Err(BedrockError::ProfileExpired(profile));
        }
        other => other?,
    };
    let exported: ExportedCredentials = serde_json::from_str(&result.stdout)
        .map_err(|error| BedrockError::Parse(error.to_string()))?;
    if exported.access_key_id.trim().is_empty() || exported.secret_access_key.trim().is_empty() {
        return Err(BedrockError::Parse(
            "AWS CLI returned empty credentials".into(),
        ));
    }
    Ok(Credentials {
        access_key_id: exported.access_key_id,
        secret_access_key: exported.secret_access_key,
        session_token: exported
            .session_token
            .filter(|value| !value.trim().is_empty()),
    })
}

fn direct_credentials(settings: &Settings) -> Option<Credentials> {
    let access_key_id = setting_or_secret(settings, "accessKeyId")
        .or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok())?;
    let secret_access_key = setting_or_secret(settings, "secretAccessKey")
        .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok())?;
    let session_token = setting_or_secret(settings, "sessionToken")
        .or_else(|| std::env::var("AWS_SESSION_TOKEN").ok());
    Some(Credentials {
        access_key_id,
        secret_access_key,
        session_token,
    })
}

fn setting_or_secret(settings: &Settings, key: &str) -> Option<String> {
    settings
        .secrets
        .get(PROVIDER_ID, key)
        .ok()
        .flatten()
        .or_else(|| settings.setting(PROVIDER_ID, key))
        .filter(|value| !value.trim().is_empty())
}

fn profile(settings: &Settings) -> Option<String> {
    settings
        .setting(PROVIDER_ID, "profile")
        .or_else(|| std::env::var("AWS_PROFILE").ok())
        .filter(|value| !value.trim().is_empty())
}

fn signed_headers(
    endpoint: &str,
    body: &[u8],
    credentials: &Credentials,
    now: OffsetDateTime,
) -> Result<HeaderMap, BedrockError> {
    let url =
        reqwest::Url::parse(endpoint).map_err(|error| BedrockError::Parse(error.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| BedrockError::Parse("endpoint has no host".into()))?;
    let date = now.date().to_string().replace('-', "");
    let timestamp = format!(
        "{}T{:02}{:02}{:02}Z",
        date,
        now.hour(),
        now.minute(),
        now.second()
    );
    let payload_hash = hex(&Sha256::digest(body));
    let mut canonical = vec![
        ("content-type", "application/x-amz-json-1.1".to_string()),
        ("host", host.to_string()),
        ("x-amz-date", timestamp.clone()),
        ("x-amz-target", TARGET.to_string()),
    ];
    if let Some(token) = &credentials.session_token {
        canonical.push(("x-amz-security-token", token.clone()));
    }
    canonical.sort_by_key(|(name, _)| *name);
    let canonical_headers = canonical
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect::<String>();
    let signed_names = canonical
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "POST\n{}\n{}\n{canonical_headers}\n{signed_names}\n{payload_hash}",
        if url.path().is_empty() {
            "/"
        } else {
            url.path()
        },
        url.query().unwrap_or("")
    );
    let scope = format!("{date}/us-east-1/ce/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let date_key = hmac(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let region_key = hmac(&date_key, b"us-east-1");
    let service_key = hmac(&region_key, b"ce");
    let signing_key = hmac(&service_key, b"aws4_request");
    let signature = hex(&hmac(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}",
        credentials.access_key_id
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-amz-json-1.1"),
    );
    headers.insert(HOST, HeaderValue::from_str(host).map_err(header_error)?);
    headers.insert(
        "x-amz-date",
        HeaderValue::from_str(&timestamp).map_err(header_error)?,
    );
    headers.insert("x-amz-target", HeaderValue::from_static(TARGET));
    headers.insert(
        "authorization",
        HeaderValue::from_str(&authorization).map_err(header_error)?,
    );
    if let Some(token) = &credentials.session_token {
        headers.insert(
            HeaderName::from_static("x-amz-security-token"),
            HeaderValue::from_str(token).map_err(header_error)?,
        );
    }
    Ok(headers)
}

fn header_error(error: reqwest::header::InvalidHeaderValue) -> BedrockError {
    BedrockError::Parse(error.to_string())
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn parse_cost(bytes: &[u8]) -> Result<f64, BedrockError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| BedrockError::Parse(error.to_string()))?;
    let results = value
        .get("ResultsByTime")
        .and_then(Value::as_array)
        .ok_or_else(|| BedrockError::Parse("missing ResultsByTime".into()))?;
    let mut total = 0.0;
    for result in results {
        if let Some(groups) = result.get("Groups").and_then(Value::as_array) {
            for group in groups {
                let is_bedrock = group
                    .get("Keys")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .any(|value| value.to_ascii_lowercase().contains("bedrock"));
                if is_bedrock {
                    total += amount_at(group.pointer("/Metrics/UnblendedCost/Amount"))?;
                }
            }
        }
    }
    Ok(total)
}
fn is_data_unavailable(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    let unavailable = [
        value.get("__type"),
        value.get("code"),
        value.get("Code"),
        value.pointer("/Error/Code"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .any(|code| {
        code.rsplit('#')
            .next()
            .is_some_and(|code| code == "DataUnavailableException")
    });
    unavailable
}

fn amount_at(value: Option<&Value>) -> Result<f64, BedrockError> {
    value
        .and_then(Value::as_str)
        .unwrap_or("0")
        .parse::<f64>()
        .map_err(|error| BedrockError::Parse(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_bedrock_service_groups() {
        let cost = parse_cost(br#"{"ResultsByTime":[{"Groups":[{"Keys":["Amazon Bedrock"],"Metrics":{"UnblendedCost":{"Amount":"12.34"}}},{"Keys":["Amazon EC2"],"Metrics":{"UnblendedCost":{"Amount":"99"}}}]}]}"#).unwrap();
        assert_eq!(cost, 12.34);
    }

    #[test]
    fn sigv4_contains_session_token_in_signed_headers() {
        let headers = signed_headers(
            COST_URL,
            b"{}",
            &Credentials {
                access_key_id: "AKID".into(),
                secret_access_key: "secret".into(),
                session_token: Some("token".into()),
            },
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        )
        .unwrap();
        assert!(headers["authorization"]
            .to_str()
            .unwrap()
            .contains("x-amz-security-token"));
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn resolves_profile_through_real_fixture_process() {
        let credentials = resolve_profile_credentials(
            crate::providers::cli::fixture_binary(),
            cli::environment(),
            "fixture".into(),
        )
        .await
        .unwrap();
        assert_eq!(credentials.access_key_id, "AKID");
        assert_eq!(credentials.secret_access_key, "secret");
        assert_eq!(credentials.session_token.as_deref(), Some("token"));
    }
}
