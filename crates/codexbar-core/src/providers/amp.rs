//! Amp usage via the non-interactive `amp usage` command.

use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;
use time::{Duration, OffsetDateTime};

use super::cli;
use crate::model::{
    DetailRow, FetchKind, FetchResult, Identity, NamedRateWindow, RateWindow, UsageSnapshot,
};

pub const PROVIDER_ID: &str = "amp";
pub const DISPLAY_NAME: &str = "Amp";
const BINARY_OVERRIDE: &str = "AMP_CLI_PATH";

#[derive(Debug, thiserror::Error)]
pub enum AmpError {
    #[error("Amp CLI was not found; install `amp` or set AMP_CLI_PATH")]
    BinaryNotFound,
    #[error(transparent)]
    Subprocess(#[from] crate::subprocess::SubprocessError),
    #[error("Amp CLI is not authenticated; run `amp` to sign in")]
    NotAuthenticated,
    #[error("could not parse Amp usage: {0}")]
    Parse(String),
}

pub fn available() -> bool {
    let environment = cli::environment();
    cli::resolve_binary("amp", BINARY_OVERRIDE, &environment).is_some()
}

pub async fn fetch() -> Result<FetchResult, AmpError> {
    let environment = cli::environment();
    let binary = cli::resolve_binary("amp", BINARY_OVERRIDE, &environment)
        .ok_or(AmpError::BinaryNotFound)?;
    fetch_with(binary, environment).await
}

async fn fetch_with(
    binary: PathBuf,
    environment: HashMap<String, String>,
) -> Result<FetchResult, AmpError> {
    let result = cli::run(binary, vec!["usage".into()], environment).await?;
    let output = if result.stdout.trim().is_empty() {
        &result.stderr
    } else {
        &result.stdout
    };
    Ok(FetchResult {
        usage: parse(output, OffsetDateTime::now_utc())?,
        strategy_id: "amp.cli".into(),
        strategy_kind: FetchKind::Cli,
    })
}

pub(crate) fn parse(output: &str, now: OffsetDateTime) -> Result<UsageSnapshot, AmpError> {
    let ansi = Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("valid ANSI regex");
    let text = ansi.replace_all(output, "").replace("**", "");
    let lower = text.to_ascii_lowercase();
    if (lower.contains("sign in") || lower.contains("not logged in"))
        && !lower.contains("signed in as")
    {
        return Err(AmpError::NotAuthenticated);
    }

    let number = r"([0-9][0-9,]*(?:\.[0-9]+)?)";
    let identity = Regex::new(r"(?im)^\s*Signed in as\s+([^\s(]+)(?:\s+\(([^\r\n)]+)\))?\s*$")
        .expect("valid identity regex")
        .captures(&text);
    let free_amount = Regex::new(&format!(
        r"(?im)^\s*Amp Free:\s*\$?{number}\s*/\s*\$?{number}\s+remaining(?:\s*\(replenishes\s*\+\$?{number}\s*/\s*hour\))?"
    ))
    .expect("valid free amount regex")
    .captures(&text);
    let free_percent = Regex::new(&format!(
        r"(?im)^\s*Amp Free:\s*{number}\s*%\s+remaining(?:\s+today)?(?:\s*(\(resets\s+daily\)))?"
    ))
    .expect("valid free percent regex")
    .captures(&text);
    let subscription = Regex::new(&format!(
        r"(?im)^\s*(?:Subscription\s+(.+?)|Amp\s+(.+?)\s+Subscription):\s*{number}\s*%\s+other\s+usage\s+and\s+{number}\s*%\s+orb\s+usage\s+remaining\s*-\s*resets\s+upon\s+renewal\s+in\s+([0-9][0-9,]*)\s+(days?|months?)(?:\s+-\s+https?://\S+)?\s*$"
    ))
    .expect("valid subscription regex")
    .captures(&text);

    let mut snapshot = UsageSnapshot::new(now);
    snapshot.source_label = Some("cli".into());
    if let Some(captures) = identity {
        snapshot.identity.account = nonempty(captures.get(1).map(|value| value.as_str()));
        snapshot.identity.organization = nonempty(captures.get(2).map(|value| value.as_str()));
    }

    let free_window = if let Some(captures) = free_amount {
        let remaining = numeric(&captures[1])?;
        let quota = numeric(&captures[2])?;
        let replenishment = captures
            .get(3)
            .map(|value| numeric(value.as_str()))
            .transpose()?
            .unwrap_or(0.0);
        let used = (quota - remaining).max(0.0);
        let mut window = RateWindow::new(if quota > 0.0 {
            (used / quota * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        });
        if replenishment > 0.0 {
            window.window_minutes = Some(((quota / replenishment).round().max(1.0) * 60.0) as u32);
            window.resets_at = Some(now + Duration::seconds_f64(used / replenishment * 3600.0));
        }
        Some(window)
    } else if let Some(captures) = free_percent {
        let remaining = numeric(&captures[1])?.clamp(0.0, 100.0);
        let mut window = RateWindow::new(100.0 - remaining).with_window_minutes(1440);
        if captures.get(2).is_some() {
            window.reset_description = Some("resets daily".into());
        }
        Some(window)
    } else {
        None
    };

    if let Some(captures) = subscription {
        let plan = nonempty(captures.get(1).map(|value| value.as_str()))
            .or_else(|| nonempty(captures.get(2).map(|value| value.as_str())))
            .expect("subscription regex has a plan");
        let other_remaining = numeric(&captures[3])?.clamp(0.0, 100.0);
        let orb_remaining = numeric(&captures[4])?.clamp(0.0, 100.0);
        let renewal = numeric(&captures[5])? as i64;
        let unit = captures[6].to_ascii_lowercase();
        let days = if unit.starts_with("month") {
            renewal * 30
        } else {
            renewal
        };
        let reset = now + Duration::days(days);
        let description = format!(
            "renews in {renewal} {}{}",
            if unit.starts_with("month") {
                "month"
            } else {
                "day"
            },
            if renewal == 1 { "" } else { "s" }
        );
        let mut primary = RateWindow::new(100.0 - other_remaining).with_window_minutes(43_200);
        primary.resets_at = Some(reset);
        primary.reset_description = Some(description.clone());
        let mut secondary = RateWindow::new(100.0 - orb_remaining).with_window_minutes(43_200);
        secondary.resets_at = Some(reset);
        secondary.reset_description = Some(description);
        snapshot.primary = Some(primary);
        snapshot.secondary = Some(secondary);
        snapshot.identity.plan = Some(plan);
        if let Some(window) = free_window {
            snapshot.extra_rate_windows.push(NamedRateWindow {
                id: "amp-free".into(),
                title: "Amp Free".into(),
                window,
            });
        }
    } else {
        snapshot.primary = free_window;
        if snapshot.primary.is_some() {
            snapshot.identity.plan = Some("Amp Free".into());
        }
    }

    let individual = Regex::new(&format!(
        r"(?im)^\s*Individual credits:\s*\$?{number}\s+remaining"
    ))
    .expect("valid credits regex");
    if let Some(captures) = individual.captures(&text) {
        snapshot.push_rows(vec![DetailRow {
            label: "Individual credits".into(),
            value: format!("${:.2}", numeric(&captures[1])?),
            hint: None,
        }]);
    }
    let workspace = Regex::new(&format!(
        r"(?im)^\s*Workspace\s+(.+?):\s*\$?{number}\s+remaining"
    ))
    .expect("valid workspace regex");
    let rows: Result<Vec<_>, AmpError> = workspace
        .captures_iter(&text)
        .map(|captures| {
            Ok(DetailRow {
                label: format!("Workspace {}", captures[1].trim()),
                value: format!("${:.2}", numeric(&captures[2])?),
                hint: None,
            })
        })
        .collect();
    snapshot.push_rows(rows?);

    if snapshot.primary.is_none() && snapshot.secondary.is_none() && snapshot.details.is_empty() {
        return Err(AmpError::Parse("missing Amp usage data".into()));
    }
    if snapshot.identity.plan.is_none() {
        snapshot.identity = Identity {
            plan: Some("Amp".into()),
            ..snapshot.identity
        };
    }
    Ok(snapshot)
}

fn numeric(value: &str) -> Result<f64, AmpError> {
    value
        .replace(',', "")
        .parse()
        .map_err(|_| AmpError::Parse(format!("invalid number `{value}`")))
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subscription_free_tier_and_balances() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let usage = parse(
            "Signed in as dev@example.com (Acme)\nAmp Free: 75% remaining today (resets daily)\nSubscription Pro: 80% other usage and 55% orb usage remaining - resets upon renewal in 2 days\nIndividual credits: $12.50 remaining\nWorkspace Team: $4 remaining\n",
            now,
        )
        .unwrap();
        assert_eq!(usage.primary.as_ref().unwrap().used_percent, 20.0);
        assert_eq!(usage.secondary.as_ref().unwrap().used_percent, 45.0);
        assert_eq!(usage.extra_rate_windows[0].window.used_percent, 25.0);
        assert_eq!(usage.identity.account.as_deref(), Some("dev@example.com"));
        assert_eq!(usage.detail_rows().count(), 2);
    }

    #[test]
    fn rejects_signed_out_output() {
        assert!(matches!(
            parse("Please sign in to Amp", OffsetDateTime::UNIX_EPOCH),
            Err(AmpError::NotAuthenticated)
        ));
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn executes_real_fixture_process_end_to_end() {
        let result = fetch_with(crate::providers::cli::fixture_binary(), cli::environment())
            .await
            .unwrap();
        assert_eq!(result.strategy_id, "amp.cli");
        assert_eq!(result.usage.primary.unwrap().used_percent, 25.0);
    }
}
