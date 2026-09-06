//! `CodexBarUsageSnapshot` (from JS) → canonical [`UsageSnapshot`].
//!
//! Port of `Plugins/ProviderPluginSnapshotMapper.swift:39-80,185-222,278-340,522-535`.
//! The JS object is handed over as JSON (`JSON.stringify`), so `Date` values arrive as
//! ISO-8601 strings exactly like upstream's date coercion accepts.

use serde::Deserialize;
use time::OffsetDateTime;

use super::PluginError;
use crate::model::{
    ChartKind, ChartPoint, Confidence, CostSnapshot, CostUsageEntry, CostUsageSnapshot,
    DetailChart, DetailRow, DetailSection, Identity, NamedRateWindow, RateWindow, UsageSnapshot,
};

/// Upstream caps detail strings; keep the same bound so a runaway plugin cannot
/// blow up the popover (`ProviderDetailSection.maximumStringLength`).
const MAX_STRING_LEN: usize = 512;
const MAX_HISTORY_DAYS: u32 = 366;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWindow {
    used_percent: f64,
    #[serde(default)]
    window_minutes: Option<f64>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    reset_description: Option<String>,
    #[serde(default)]
    next_regen_percent: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawNamedWindow {
    id: String,
    title: String,
    /// Either `{ id, title, window: {...} }` or the window fields inlined.
    #[serde(default)]
    window: Option<RawWindow>,
    #[serde(flatten, default)]
    inline: Option<RawWindow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCost {
    used: f64,
    #[serde(default)]
    limit: Option<f64>,
    currency: String,
    #[serde(default)]
    period: Option<String>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    balance: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCostUsageEntry {
    date: String,
    #[serde(default)]
    input_tokens: f64,
    #[serde(default)]
    output_tokens: f64,
    #[serde(default)]
    reasoning_tokens: Option<f64>,
    #[serde(default)]
    requests: f64,
    #[serde(default)]
    cost: f64,
    #[serde(default)]
    estimated_cost: Option<f64>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCostUsage {
    currency: String,
    history_days: f64,
    #[serde(default)]
    history_label: Option<String>,
    window_end: String,
    #[serde(default)]
    entries: Vec<RawCostUsageEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIdentity {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    organization: Option<String>,
    #[serde(default)]
    login_method: Option<String>,
    #[serde(default, rename = "accountID")]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDetailRow {
    label: String,
    value: String,
    #[serde(default)]
    secondary_value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawChartPoint {
    label: String,
    value: f64,
}

#[derive(Debug, Deserialize)]
struct RawChart {
    kind: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    points: Vec<RawChartPoint>,
}

#[derive(Debug, Deserialize)]
struct RawDetailSection {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    rows: Vec<RawDetailRow>,
    #[serde(default)]
    chart: Option<RawChart>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSnapshot {
    #[serde(default)]
    primary: Option<RawWindow>,
    #[serde(default)]
    secondary: Option<RawWindow>,
    #[serde(default)]
    tertiary: Option<RawWindow>,
    #[serde(default)]
    extra_windows: Option<Vec<RawNamedWindow>>,
    #[serde(default)]
    cost: Option<RawCost>,
    #[serde(default)]
    cost_usage: Option<RawCostUsage>,
    #[serde(default)]
    identity: Option<RawIdentity>,
    #[serde(default)]
    subscription_renews_at: Option<String>,
    #[serde(default)]
    subscription_expires_at: Option<String>,
    #[serde(default)]
    data_confidence: Option<String>,
    #[serde(default)]
    details: Option<Vec<RawDetailSection>>,
}

/// Parses and validates the plugin's returned snapshot.
pub fn map(json: &str, now: OffsetDateTime) -> Result<UsageSnapshot, PluginError> {
    let raw: RawSnapshot = serde_json::from_str(json).map_err(|e| {
        PluginError::InvalidSnapshot(format!("fetchUsage must resolve to a snapshot object: {e}"))
    })?;

    let mut snapshot = UsageSnapshot::new(now);
    snapshot.primary = raw
        .primary
        .as_ref()
        .map(|w| window(w, "primary"))
        .transpose()?;
    snapshot.secondary = raw
        .secondary
        .as_ref()
        .map(|w| window(w, "secondary"))
        .transpose()?;
    snapshot.tertiary = raw
        .tertiary
        .as_ref()
        .map(|w| window(w, "tertiary"))
        .transpose()?;

    for (index, named) in raw.extra_windows.iter().flatten().enumerate() {
        let path = format!("extraWindows[{index}]");
        let source = named
            .window
            .as_ref()
            .or(named.inline.as_ref())
            .ok_or_else(|| PluginError::InvalidSnapshot(format!("{path} has no window")))?;
        snapshot.extra_rate_windows.push(NamedRateWindow {
            id: trimmed(&named.id, &path)?,
            title: trimmed(&named.title, &path)?,
            window: window(source, &path)?,
        });
    }

    if let Some(cost) = &raw.cost {
        snapshot.cost = Some(CostSnapshot {
            used: finite(cost.used, "cost.used")?,
            limit: cost.limit.map(|v| finite(v, "cost.limit")).transpose()?,
            currency: currency_code(&cost.currency, "cost.currency")?,
            period: optional_string(cost.period.as_deref()),
            resets_at: parse_date(cost.resets_at.as_deref(), "cost.resetsAt")?,
            balance: cost
                .balance
                .map(|v| finite(v, "cost.balance"))
                .transpose()?,
        });
    }

    if let Some(usage) = &raw.cost_usage {
        let days = finite(usage.history_days, "costUsage.historyDays")?;
        if !(1.0..=MAX_HISTORY_DAYS as f64).contains(&days) {
            return Err(PluginError::InvalidSnapshot(format!(
                "costUsage.historyDays must be 1-{MAX_HISTORY_DAYS}"
            )));
        }
        let mut entries = Vec::with_capacity(usage.entries.len());
        for (index, entry) in usage.entries.iter().enumerate() {
            let path = format!("costUsage.entries[{index}]");
            entries.push(CostUsageEntry {
                date: trimmed(&entry.date, &path)?,
                input_tokens: count(entry.input_tokens, &path)?,
                output_tokens: count(entry.output_tokens, &path)?,
                reasoning_tokens: entry
                    .reasoning_tokens
                    .map(|v| count(v, &path))
                    .transpose()?,
                requests: count(entry.requests, &path)?,
                cost: finite(entry.cost, &path)?,
                estimated_cost: entry.estimated_cost.map(|v| finite(v, &path)).transpose()?,
                model: optional_string(entry.model.as_deref()),
            });
        }
        snapshot.cost_usage = Some(CostUsageSnapshot {
            currency: currency_code(&usage.currency, "costUsage.currency")?,
            history_days: days as u32,
            history_label: optional_string(usage.history_label.as_deref()),
            window_end: trimmed(&usage.window_end, "costUsage.windowEnd")?,
            entries,
        });
    }

    for (index, section) in raw.details.iter().flatten().enumerate() {
        let path = format!("details[{index}]");
        let rows: Vec<DetailRow> = section
            .rows
            .iter()
            .map(|row| {
                Ok(DetailRow {
                    label: trimmed(&row.label, &path)?,
                    value: trimmed(&row.value, &path)?,
                    hint: optional_string(row.secondary_value.as_deref()),
                })
            })
            .collect::<Result<_, PluginError>>()?;
        let chart = section
            .chart
            .as_ref()
            .map(|chart| {
                Ok::<_, PluginError>(DetailChart {
                    kind: match chart.kind.as_str() {
                        "bars" => ChartKind::Bars,
                        "line" => ChartKind::Line,
                        other => {
                            return Err(PluginError::InvalidSnapshot(format!(
                                "{path}.chart.kind '{other}' is not supported"
                            )))
                        }
                    },
                    title: optional_string(chart.title.as_deref()),
                    unit: optional_string(chart.unit.as_deref()),
                    points: chart
                        .points
                        .iter()
                        .map(|p| {
                            Ok(ChartPoint {
                                label: trimmed(&p.label, &path)?,
                                value: finite(p.value, &path)?,
                            })
                        })
                        .collect::<Result<_, PluginError>>()?,
                })
            })
            .transpose()?;
        if rows.is_empty() && chart.is_none() {
            continue;
        }
        snapshot.details.push(DetailSection {
            title: optional_string(section.title.as_deref()),
            rows,
            chart,
        });
    }

    if let Some(identity) = &raw.identity {
        snapshot.identity = Identity {
            account: optional_string(identity.email.as_deref()),
            plan: optional_string(identity.login_method.as_deref()),
            account_id: optional_string(identity.account_id.as_deref()),
            organization: optional_string(identity.organization.as_deref()),
        };
    }

    snapshot.subscription_renews_at = parse_date(
        raw.subscription_renews_at.as_deref(),
        "subscriptionRenewsAt",
    )?;
    snapshot.subscription_expires_at = parse_date(
        raw.subscription_expires_at.as_deref(),
        "subscriptionExpiresAt",
    )?;

    snapshot.confidence = match raw.data_confidence.as_deref() {
        None | Some("exact") => Confidence::Exact,
        Some("estimated") => Confidence::Estimated,
        Some("percentOnly") => Confidence::PercentOnly,
        Some("unknown") => Confidence::Unknown,
        Some(other) => {
            return Err(PluginError::InvalidSnapshot(format!(
                "dataConfidence '{other}' is not supported"
            )))
        }
    };

    // Upstream refuses empty snapshots so a broken plugin cannot look "connected".
    let has_window = snapshot.primary.is_some()
        || snapshot.secondary.is_some()
        || snapshot.tertiary.is_some()
        || !snapshot.extra_rate_windows.is_empty();
    if !has_window
        && snapshot.cost.is_none()
        && snapshot.cost_usage.is_none()
        && snapshot.details.is_empty()
        && snapshot.identity.is_empty()
    {
        return Err(PluginError::InvalidSnapshot(
            "snapshot must contain at least one rate window, cost, detail section, or identity field".into(),
        ));
    }
    if !has_window && snapshot.confidence == Confidence::Exact {
        snapshot.confidence = Confidence::IdentityOnly;
    }

    Ok(snapshot)
}

fn window(raw: &RawWindow, path: &str) -> Result<RateWindow, PluginError> {
    let used = finite(raw.used_percent, path)?;
    let mut window = RateWindow::new(used.clamp(0.0, 100.0));
    if let Some(minutes) = raw.window_minutes {
        let minutes = finite(minutes, path)?;
        if minutes > 0.0 {
            window.window_minutes = Some(minutes as u32);
        }
    }
    window.resets_at = parse_date(raw.resets_at.as_deref(), path)?;
    window.reset_description = optional_string(raw.reset_description.as_deref());
    window.next_regen_percent = raw
        .next_regen_percent
        .map(|v| finite(v, path).map(|v| v.clamp(0.0, 100.0)))
        .transpose()?;
    Ok(window)
}

fn finite(value: f64, path: &str) -> Result<f64, PluginError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(PluginError::InvalidSnapshot(format!(
            "{path} must be a finite number"
        )))
    }
}

fn count(value: f64, path: &str) -> Result<u64, PluginError> {
    let value = finite(value, path)?;
    if value < 0.0 {
        return Err(PluginError::InvalidSnapshot(format!(
            "{path} must not be negative"
        )));
    }
    Ok(value.round() as u64)
}

fn trimmed(value: &str, path: &str) -> Result<String, PluginError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(PluginError::InvalidSnapshot(format!(
            "{path} must not be empty"
        )));
    }
    if trimmed.chars().count() > MAX_STRING_LEN {
        return Err(PluginError::InvalidSnapshot(format!(
            "{path} exceeds {MAX_STRING_LEN} characters"
        )));
    }
    Ok(trimmed.to_string())
}

fn optional_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.chars().take(MAX_STRING_LEN).collect())
}

fn currency_code(value: &str, path: &str) -> Result<String, PluginError> {
    let code = value.trim().to_uppercase();
    if code.len() != 3 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(PluginError::InvalidSnapshot(format!(
            "{path} must be a 3-letter currency code"
        )));
    }
    Ok(code)
}

/// Accepts ISO-8601 (what `JSON.stringify` produces for a `Date`) with or without
/// fractional seconds.
fn parse_date(value: Option<&str>, path: &str) -> Result<Option<OffsetDateTime>, PluginError> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map(Some)
        .map_err(|_| {
            PluginError::InvalidSnapshot(format!("{path} must be an ISO-8601 date, found '{raw}'"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-01 12:00 UTC);

    #[test]
    fn maps_the_shape_venice_returns() {
        let snapshot = map(
            r#"{ "primary": { "usedPercent": 0, "resetDescription": "$4.00 USD remaining" }, "identity": {} }"#,
            NOW,
        )
        .unwrap();
        let primary = snapshot.primary.expect("primary window");
        assert_eq!(primary.used_percent, 0.0);
        assert_eq!(
            primary.reset_description.as_deref(),
            Some("$4.00 USD remaining")
        );
        assert_eq!(
            snapshot.confidence,
            Confidence::Exact,
            "a window with no dataConfidence stays exact"
        );
    }

    #[test]
    fn clamps_percentages_and_converts_dates() {
        let snapshot = map(
            r#"{ "primary": { "usedPercent": 140, "windowMinutes": 300,
                              "resetsAt": "2026-09-01T16:12:00.000Z", "nextRegenPercent": -5 } }"#,
            NOW,
        )
        .unwrap();
        let primary = snapshot.primary.unwrap();
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at, Some(datetime!(2026-09-01 16:12 UTC)));
        assert_eq!(primary.next_regen_percent, Some(0.0));
    }

    #[test]
    fn accepts_named_windows_inline_or_nested() {
        let snapshot = map(
            r#"{ "extraWindows": [
                    { "id": "a", "title": "Inline", "usedPercent": 10 },
                    { "id": "b", "title": "Nested", "window": { "usedPercent": 20 } }
                 ] }"#,
            NOW,
        )
        .unwrap();
        assert_eq!(snapshot.extra_rate_windows.len(), 2);
        assert_eq!(snapshot.extra_rate_windows[0].window.used_percent, 10.0);
        assert_eq!(snapshot.extra_rate_windows[1].window.used_percent, 20.0);
    }

    #[test]
    fn empty_snapshots_are_rejected() {
        let err = map(r#"{}"#, NOW).unwrap_err();
        assert!(matches!(err, PluginError::InvalidSnapshot(m) if m.contains("at least one")));
        let err = map(r#"{ "identity": {} }"#, NOW).unwrap_err();
        assert!(matches!(err, PluginError::InvalidSnapshot(_)));
    }

    #[test]
    fn identity_only_snapshots_are_allowed_and_flagged() {
        let snapshot = map(r#"{ "identity": { "email": "dev@example.com" } }"#, NOW).unwrap();
        assert_eq!(
            snapshot.identity.account.as_deref(),
            Some("dev@example.com")
        );
        assert_eq!(snapshot.confidence, Confidence::IdentityOnly);
    }

    #[test]
    fn cost_and_history_are_validated() {
        let snapshot = map(
            r#"{ "cost": { "used": 12.5, "limit": 100, "currency": "usd", "period": "Monthly" },
                 "costUsage": { "currency": "USD", "historyDays": 30, "windowEnd": "2026-09-01",
                                "entries": [ { "date": "2026-09-01", "inputTokens": 10, "outputTokens": 5,
                                               "requests": 2, "cost": 0.25 } ] } }"#,
            NOW,
        )
        .unwrap();
        let cost = snapshot.cost.unwrap();
        assert_eq!(cost.currency, "USD", "currency is upper-cased");
        let history = snapshot.cost_usage.unwrap();
        assert_eq!(history.total_tokens(), 15);
        assert_eq!(history.total_cost(), 0.25);

        let err = map(
            r#"{ "costUsage": { "currency": "US", "historyDays": 30, "windowEnd": "2026-09-01", "entries": [] } }"#,
            NOW,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::InvalidSnapshot(m) if m.contains("currency code")));

        let err = map(
            r#"{ "costUsage": { "currency": "USD", "historyDays": 900, "windowEnd": "2026-09-01", "entries": [] } }"#,
            NOW,
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::InvalidSnapshot(m) if m.contains("historyDays")));
    }

    #[test]
    fn detail_sections_and_charts_survive() {
        let snapshot = map(
            r#"{ "details": [ { "title": "Spend",
                                "rows": [ { "label": "Today", "value": "$1.00", "secondaryValue": "est." } ],
                                "chart": { "kind": "bars", "unit": "USD",
                                           "points": [ { "label": "Mon", "value": 1 } ] } } ] }"#,
            NOW,
        )
        .unwrap();
        let section = &snapshot.details[0];
        assert_eq!(section.title.as_deref(), Some("Spend"));
        assert_eq!(section.rows[0].hint.as_deref(), Some("est."));
        assert_eq!(section.chart.as_ref().unwrap().kind, ChartKind::Bars);
    }

    #[test]
    fn unsupported_confidence_and_chart_kinds_are_errors() {
        assert!(map(
            r#"{ "primary": { "usedPercent": 1 }, "dataConfidence": "vibes" }"#,
            NOW
        )
        .is_err());
        assert!(map(
            r#"{ "details": [ { "rows": [], "chart": { "kind": "pie", "points": [] } } ] }"#,
            NOW
        )
        .is_err());
    }

    #[test]
    fn data_confidence_maps_every_documented_value() {
        for (raw, expected) in [
            ("exact", Confidence::Exact),
            ("estimated", Confidence::Estimated),
            ("percentOnly", Confidence::PercentOnly),
            ("unknown", Confidence::Unknown),
        ] {
            let json =
                format!(r#"{{ "primary": {{ "usedPercent": 1 }}, "dataConfidence": "{raw}" }}"#);
            assert_eq!(map(&json, NOW).unwrap().confidence, expected, "{raw}");
        }
    }
}
