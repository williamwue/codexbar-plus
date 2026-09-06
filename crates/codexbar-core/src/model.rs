//! Canonical usage model.
//!
//! Field-for-field port of upstream `Sources/CodexBarCore/UsageFetcher.swift:3-176`
//! (`RateWindow`, `UsageSnapshot`) and `CreditsModels.swift:45-48` (`CreditsSnapshot`).
//! Keeping the shapes identical is deliberate: menu-bar rendering, the CLI renderer and
//! the JS plugin contract (`Resources/Plugins/codexbar-plugin.d.ts`) all assume them.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A single quota window (session / weekly / monthly / bespoke).
///
/// `used_percent` is USED, not remaining: 0.0 = untouched, 100.0 = exhausted.
/// The UI derives "89% left" by `100 - used_percent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateWindow {
    pub used_percent: f64,
    /// Window length in minutes (e.g. 300 for a 5-hour window, 10080 for weekly).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u32>,
    /// Absolute instant the window rolls over.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
    /// Provider-supplied human string, used verbatim when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_description: Option<String>,
    /// Percent that regenerates at the next tick, for providers with rolling regen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_regen_percent: Option<f64>,
    /// True when the window is a placeholder invented to keep layout stable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_synthetic_placeholder: bool,
}

impl RateWindow {
    pub fn new(used_percent: f64) -> Self {
        Self {
            used_percent,
            window_minutes: None,
            resets_at: None,
            reset_description: None,
            next_regen_percent: None,
            is_synthetic_placeholder: false,
        }
    }

    pub fn with_window_minutes(mut self, minutes: u32) -> Self {
        self.window_minutes = Some(minutes);
        self
    }

    pub fn with_reset(mut self, at: OffsetDateTime) -> Self {
        self.resets_at = Some(at);
        self
    }

    /// Remaining percent, clamped to `0..=100`.
    pub fn remaining_percent(&self) -> f64 {
        (100.0 - self.used_percent).clamp(0.0, 100.0)
    }

    /// Time until reset relative to `now`; `None` when unknown or already elapsed.
    pub fn time_until_reset(&self, now: OffsetDateTime) -> Option<time::Duration> {
        let at = self.resets_at?;
        let delta = at - now;
        if delta.is_positive() {
            Some(delta)
        } else {
            None
        }
    }
}

/// A quota window that carries its own label (upstream `CodexBarNamedRateWindow`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedRateWindow {
    pub id: String,
    pub title: String,
    #[serde(flatten)]
    pub window: RateWindow,
}

/// Credit balance ledger (upstream `CreditsSnapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreditsSnapshot {
    /// Remaining balance in provider-native units.
    pub remaining: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<CreditEvent>,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreditEvent {
    pub label: String,
    pub amount: f64,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<OffsetDateTime>,
}

/// Who the snapshot belongs to (upstream `CodexBarIdentitySnapshot`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
}

impl Identity {
    pub fn is_empty(&self) -> bool {
        self.account.is_none()
            && self.plan.is_none()
            && self.account_id.is_none()
            && self.organization.is_none()
    }
}

/// How much to trust the snapshot (upstream `confidence` / plugin `dataConfidence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Confidence {
    /// Provider reported the numbers directly.
    Exact,
    /// Derived locally (e.g. from session logs) and may drift.
    Estimated,
    /// Only a percentage is trustworthy; absolute amounts are not.
    PercentOnly,
    /// Authenticated but the provider exposed no quota data.
    IdentityOnly,
    /// Provider did not say.
    Unknown,
}

/// A detail row rendered under the meters (upstream `CodexBarDetailRow`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetailRow {
    pub label: String,
    pub value: String,
    /// Upstream `secondaryValue`: shown dimmed next to the value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// A small chart attached to a detail section (upstream `CodexBarDetailChart`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetailChart {
    pub kind: ChartKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub points: Vec<ChartPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChartKind {
    Bars,
    Line,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartPoint {
    pub label: String,
    pub value: f64,
}

/// Grouped detail rows (upstream `CodexBarDetailSection`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetailSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub rows: Vec<DetailRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chart: Option<DetailChart>,
}

impl DetailSection {
    /// An untitled section, the shape native providers produce.
    pub fn untitled(rows: Vec<DetailRow>) -> Self {
        Self {
            title: None,
            rows,
            chart: None,
        }
    }
}

/// Provider-reported spend against a cap (upstream `ProviderCostSnapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostSnapshot {
    pub used: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    pub currency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub resets_at: Option<OffsetDateTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
}

/// Provider-reported daily spend history (upstream `CodexBarCostUsageSnapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostUsageSnapshot {
    pub currency: String,
    pub history_days: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_label: Option<String>,
    /// Inclusive `YYYY-MM-DD` end of the reported window.
    pub window_end: String,
    pub entries: Vec<CostUsageEntry>,
}

impl CostUsageSnapshot {
    pub fn total_cost(&self) -> f64 {
        self.entries.iter().map(|e| e.cost).sum()
    }

    pub fn total_tokens(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| e.input_tokens + e.output_tokens + e.reasoning_tokens.unwrap_or(0))
            .sum()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostUsageEntry {
    pub date: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    pub requests: u64,
    pub cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Everything one provider fetch produced (upstream `UsageSnapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    /// Headline window drawn as the first bar of the tray icon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<RateWindow>,
    /// Second bar, conventionally the weekly window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<RateWindow>,
    /// Third bar, conventionally monthly/credits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tertiary: Option<RateWindow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_rate_windows: Vec<NamedRateWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<CreditsSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<DetailSection>,
    /// Spend against a provider-declared cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostSnapshot>,
    /// Daily spend history when the provider reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usage: Option<CostUsageSnapshot>,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub subscription_renews_at: Option<OffsetDateTime>,
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub subscription_expires_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Identity::is_empty")]
    pub identity: Identity,
    pub confidence: Confidence,
    /// Provenance shown as "Updated just now · oauth".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_label: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl UsageSnapshot {
    pub fn new(updated_at: OffsetDateTime) -> Self {
        Self {
            primary: None,
            secondary: None,
            tertiary: None,
            extra_rate_windows: Vec::new(),
            credits: None,
            details: Vec::new(),
            cost: None,
            cost_usage: None,
            subscription_renews_at: None,
            subscription_expires_at: None,
            identity: Identity::default(),
            confidence: Confidence::Exact,
            source_label: None,
            updated_at,
        }
    }

    /// Appends rows as one untitled section, the shape native providers produce.
    pub fn push_rows(&mut self, rows: Vec<DetailRow>) {
        if rows.is_empty() {
            return;
        }
        match self.details.iter_mut().find(|s| s.title.is_none()) {
            Some(section) => section.rows.extend(rows),
            None => self.details.push(DetailSection::untitled(rows)),
        }
    }

    /// Flattened rows, for renderers that do not group.
    pub fn detail_rows(&self) -> impl Iterator<Item = &DetailRow> {
        self.details.iter().flat_map(|s| s.rows.iter())
    }

    /// Window that drives the tray icon's highest-usage auto-selection.
    pub fn worst_used_percent(&self) -> Option<f64> {
        [
            self.primary.as_ref(),
            self.secondary.as_ref(),
            self.tertiary.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(|w| w.used_percent)
        .chain(
            self.extra_rate_windows
                .iter()
                .map(|w| w.window.used_percent),
        )
        .fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |a| a.max(v)))
        })
    }
}

/// Which strategy produced a result (upstream `ProviderFetchKind`,
/// `Providers/ProviderFetchPlan.swift:264-272`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FetchKind {
    Cli,
    Web,
    Oauth,
    ApiToken,
    LocalProbe,
    WebDashboard,
}

impl FetchKind {
    pub fn label(self) -> &'static str {
        match self {
            FetchKind::Cli => "cli",
            FetchKind::Web => "web",
            FetchKind::Oauth => "oauth",
            FetchKind::ApiToken => "api",
            FetchKind::LocalProbe => "local",
            FetchKind::WebDashboard => "dashboard",
        }
    }
}

/// Result of one strategy run (upstream `ProviderFetchResult`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FetchResult {
    pub usage: UsageSnapshot,
    pub strategy_id: String,
    pub strategy_kind: FetchKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn remaining_is_complement_of_used_and_clamped() {
        assert_eq!(RateWindow::new(11.0).remaining_percent(), 89.0);
        assert_eq!(RateWindow::new(140.0).remaining_percent(), 0.0);
        assert_eq!(RateWindow::new(-5.0).remaining_percent(), 100.0);
    }

    #[test]
    fn elapsed_reset_reports_no_remaining_time() {
        let now = datetime!(2026-09-01 12:00 UTC);
        let past = RateWindow::new(1.0).with_reset(datetime!(2026-09-01 11:00 UTC));
        let future = RateWindow::new(1.0).with_reset(datetime!(2026-09-01 16:12 UTC));
        assert!(past.time_until_reset(now).is_none());
        assert_eq!(
            future.time_until_reset(now),
            Some(time::Duration::minutes(4 * 60 + 12))
        );
    }

    #[test]
    fn worst_used_percent_spans_every_lane() {
        let mut snap = UsageSnapshot::new(datetime!(2026-09-01 12:00 UTC));
        assert_eq!(snap.worst_used_percent(), None);
        snap.primary = Some(RateWindow::new(11.0));
        snap.secondary = Some(RateWindow::new(2.0));
        snap.extra_rate_windows.push(NamedRateWindow {
            id: "opus".into(),
            title: "Opus".into(),
            window: RateWindow::new(64.5),
        });
        assert_eq!(snap.worst_used_percent(), Some(64.5));
    }
}
