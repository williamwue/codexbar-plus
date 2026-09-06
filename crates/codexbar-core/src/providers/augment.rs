//! Augment usage via `auggie account status`.

use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use super::cli;
use crate::model::{FetchKind, FetchResult, Identity, RateWindow, UsageSnapshot};

pub const PROVIDER_ID: &str = "augment";
pub const DISPLAY_NAME: &str = "Augment";
const BINARY_OVERRIDE: &str = "AUGGIE_CLI_PATH";

#[derive(Debug, thiserror::Error)]
pub enum AugmentError {
    #[error("Auggie CLI was not found; install `auggie` or set AUGGIE_CLI_PATH")]
    BinaryNotFound,
    #[error(transparent)]
    Subprocess(#[from] crate::subprocess::SubprocessError),
    #[error("Auggie CLI is not authenticated; run `auggie login`")]
    NotAuthenticated,
    #[error("could not parse Augment usage: {0}")]
    Parse(String),
}

pub fn available() -> bool {
    let environment = cli::environment();
    cli::resolve_binary("auggie", BINARY_OVERRIDE, &environment).is_some()
}

pub async fn fetch() -> Result<FetchResult, AugmentError> {
    let environment = cli::environment();
    let binary = cli::resolve_binary("auggie", BINARY_OVERRIDE, &environment)
        .ok_or(AugmentError::BinaryNotFound)?;
    fetch_with(binary, environment).await
}

async fn fetch_with(
    binary: PathBuf,
    environment: HashMap<String, String>,
) -> Result<FetchResult, AugmentError> {
    let result = cli::run(binary, vec!["account".into(), "status".into()], environment).await?;
    let output = if result.stdout.trim().is_empty() {
        &result.stderr
    } else {
        &result.stdout
    };
    Ok(FetchResult {
        usage: parse(output, OffsetDateTime::now_utc())?,
        strategy_id: "augment.cli".into(),
        strategy_kind: FetchKind::Cli,
    })
}

pub(crate) fn parse(output: &str, now: OffsetDateTime) -> Result<UsageSnapshot, AugmentError> {
    let lower = output.to_ascii_lowercase();
    if lower.contains("authentication failed") || lower.contains("auggie login") {
        return Err(AugmentError::NotAuthenticated);
    }

    let monthly = Regex::new(r"(?i)([\d,]+)\s+credits\s*/\s*month").unwrap();
    let remaining_current = Regex::new(r"(?i)([\d,]+)\s+credits\s+remaining").unwrap();
    let remaining_legacy = Regex::new(r"(?i)([\d,]+)\s+remaining").unwrap();
    let used_legacy = Regex::new(r"(?i)([\d,]+)\s*/\s*([\d,]+)\s+credits used").unwrap();
    let end_date = Regex::new(r"(?i)ends\s+(\d{1,2})/(\d{1,2})/(\d{4})").unwrap();

    let limit = monthly
        .captures(output)
        .map(|captures| integer(&captures[1]))
        .transpose()?;
    let remaining = remaining_current
        .captures(output)
        .or_else(|| remaining_legacy.captures(output))
        .map(|captures| integer(&captures[1]))
        .transpose()?
        .ok_or_else(|| AugmentError::Parse("missing remaining credits".into()))?;
    let legacy = used_legacy.captures(output);
    let used = legacy
        .as_ref()
        .map(|captures| integer(&captures[1]))
        .transpose()?;
    let total = legacy
        .as_ref()
        .map(|captures| integer(&captures[2]))
        .transpose()?
        .or(limit)
        .ok_or_else(|| AugmentError::Parse("missing credit limit".into()))?;
    let used = used.unwrap_or_else(|| total.saturating_sub(remaining));

    let resets_at = end_date.captures(output).and_then(|captures| {
        let month = captures[1]
            .parse::<u8>()
            .ok()
            .and_then(|value| Month::try_from(value).ok())?;
        let day = captures[2].parse::<u8>().ok()?;
        let year = captures[3].parse::<i32>().ok()?;
        let date = Date::from_calendar_date(year, month, day).ok()?;
        Some(PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc())
    });

    let mut window = RateWindow::new(if total > 0 {
        (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    });
    window.resets_at = resets_at;
    if let Some(reset) = resets_at {
        window.reset_description = Some(format!("Resets {}", reset.date()));
    }

    let mut snapshot = UsageSnapshot::new(now);
    snapshot.primary = Some(window);
    snapshot.identity = Identity {
        plan: limit.map(|value| format!("{value} credits/month")),
        ..Identity::default()
    };
    snapshot.source_label = Some("cli".into());
    Ok(snapshot)
}

fn integer(value: &str) -> Result<u64, AugmentError> {
    value
        .replace(',', "")
        .parse()
        .map_err(|_| AugmentError::Parse(format!("invalid integer `{value}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_output() {
        let usage = parse(
            "319,054 credits remaining                     Max Plan\n450,000 credits / month\n9 days remaining in this billing cycle (ends 6/9/2026)\n",
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        assert!((usage.primary.unwrap().used_percent - 29.099_111).abs() < 0.001);
        assert_eq!(usage.identity.plan.as_deref(), Some("450000 credits/month"));
    }

    #[test]
    fn parses_legacy_output() {
        let usage = parse(
            "Max Plan 450,000 credits / month\n11,657 remaining · 953,170 / 964,827 credits used\n2 days remaining in this billing cycle (ends 1/8/2026)\n",
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        assert!((usage.primary.unwrap().used_percent - 98.7918).abs() < 0.001);
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn executes_real_fixture_process_end_to_end() {
        let result = fetch_with(crate::providers::cli::fixture_binary(), cli::environment())
            .await
            .unwrap();
        assert_eq!(result.strategy_id, "augment.cli");
        assert!((result.usage.primary.unwrap().used_percent - 29.099_111).abs() < 0.001);
    }
}
