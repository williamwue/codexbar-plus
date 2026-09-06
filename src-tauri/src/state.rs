//! Shared app state: latest snapshots per provider plus the refresh bookkeeping the
//! adaptive policy needs (upstream keeps the same two signals: last menu open and last
//! coding activity).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use codexbar_core::model::{FetchResult, UsageSnapshot};
use codexbar_core::status::ProviderStatus;
use serde::Serialize;
use time::OffsetDateTime;

/// One provider row as the UI consumes it.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderView {
    pub id: String,
    pub display_name: String,
    pub accent: String,
    pub snapshot: Option<UsageSnapshot>,
    pub source: Option<String>,
    pub error: Option<String>,
    /// Provider status page result; `None` until the first successful poll.
    pub status: Option<ProviderStatus>,
    /// Local cost estimate for today, in USD, when this provider has session logs.
    pub cost_today: Option<f64>,
    /// Rolling 30-day local cost estimate.
    pub cost_month: Option<f64>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub fetched_at: Option<OffsetDateTime>,
}

impl ProviderView {
    pub fn pending(id: &str, display_name: &str, accent: &str) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            accent: accent.to_string(),
            snapshot: None,
            source: None,
            error: None,
            status: None,
            cost_today: None,
            cost_month: None,
            fetched_at: None,
        }
    }

    /// Attaches a status-page result. A failed poll keeps the previous status, matching
    /// upstream (`UsageStore+ProviderStatus.swift:32-36`).
    pub fn with_status(mut self, status: Option<ProviderStatus>) -> Self {
        if let Some(status) = status {
            self.status = Some(status);
        }
        self
    }

    /// Attaches local cost estimates.
    pub fn with_cost(mut self, today: Option<f64>, month: Option<f64>) -> Self {
        self.cost_today = today;
        self.cost_month = month;
        self
    }

    /// True when the provider's status page reports anything other than operational.
    pub fn has_incident(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|s| s.indicator.has_issue())
    }

    pub fn with_result(mut self, result: FetchResult, at: OffsetDateTime) -> Self {
        self.source = Some(result.strategy_kind.label().to_string());
        self.snapshot = Some(result.usage);
        self.error = None;
        self.fetched_at = Some(at);
        self
    }

    pub fn with_error(mut self, message: String, at: OffsetDateTime) -> Self {
        self.error = Some(message);
        self.fetched_at = Some(at);
        self
    }

    /// Remaining fraction of the session lane, for the tray meter.
    pub fn primary_remaining(&self) -> Option<f64> {
        Some(
            self.snapshot
                .as_ref()?
                .primary
                .as_ref()?
                .remaining_percent()
                / 100.0,
        )
    }

    /// Remaining fraction of the weekly lane.
    pub fn secondary_remaining(&self) -> Option<f64> {
        Some(
            self.snapshot
                .as_ref()?
                .secondary
                .as_ref()?
                .remaining_percent()
                / 100.0,
        )
    }

    pub fn worst_used_percent(&self) -> Option<f64> {
        self.snapshot.as_ref()?.worst_used_percent()
    }
}

#[derive(Debug, Default)]
pub struct AppState {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    providers: Vec<ProviderView>,
    /// Which provider the tray icon currently represents.
    selected: Option<String>,
    /// True once the user picked a provider by hand; auto-selection then stops fighting them.
    manual_selection: bool,
    /// Set every time the popover opens; drives the adaptive cadence.
    last_menu_open_at: Option<OffsetDateTime>,
    refreshing: bool,
    /// Last time each window answered a liveness ping.
    ///
    /// A WebView2 runtime update kills the webview while leaving the window shell alive,
    /// which leaves a blank panel behind. Tracking replies lets the app notice and reload.
    last_pong: HashMap<String, Instant>,
}

impl AppState {
    pub fn new(providers: Vec<ProviderView>) -> Self {
        let selected = providers.first().map(|p| p.id.clone());
        Self {
            inner: Mutex::new(Inner {
                providers,
                selected,
                manual_selection: false,
                last_menu_open_at: None,
                refreshing: false,
                last_pong: HashMap::new(),
            }),
        }
    }

    pub fn providers(&self) -> Vec<ProviderView> {
        self.inner.lock().expect("state lock").providers.clone()
    }

    pub fn replace(&self, providers: Vec<ProviderView>) {
        let mut inner = self.inner.lock().expect("state lock");
        inner.providers = providers;
        if inner
            .selected
            .as_ref()
            .map(|id| !inner.providers.iter().any(|p| &p.id == id))
            .unwrap_or(true)
        {
            inner.selected = inner.providers.first().map(|p| p.id.clone());
        }
    }

    pub fn selected(&self) -> Option<ProviderView> {
        let inner = self.inner.lock().expect("state lock");
        let id = inner.selected.clone()?;
        inner.providers.iter().find(|p| p.id == id).cloned()
    }

    /// Explicit user choice; pins the tray to this provider.
    pub fn select(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().expect("state lock");
        if inner.providers.iter().any(|p| p.id == id) {
            inner.selected = Some(id.to_string());
            inner.manual_selection = true;
            true
        } else {
            false
        }
    }

    /// Points the tray at the provider closest to exhaustion, like upstream's
    /// highest-usage auto-selection (`PreferencesMenuBarPane.swift:56-80`).
    /// No-op once the user has chosen a provider explicitly.
    pub fn select_highest_usage(&self) {
        let mut inner = self.inner.lock().expect("state lock");
        if inner.manual_selection {
            return;
        }
        let worst = inner
            .providers
            .iter()
            .filter_map(|p| p.worst_used_percent().map(|v| (p.id.clone(), v)))
            .fold(None::<(String, f64)>, |acc, (id, v)| match acc {
                Some((_, best)) if best >= v => acc,
                _ => Some((id, v)),
            });
        if let Some((id, _)) = worst {
            inner.selected = Some(id);
        }
    }

    /// Records that a window's UI answered a ping.
    pub fn note_pong(&self, label: &str) {
        self.inner
            .lock()
            .expect("state lock")
            .last_pong
            .insert(label.to_string(), Instant::now());
    }

    /// Whether `label` answered since `since`.
    pub fn answered_since(&self, label: &str, since: Instant) -> bool {
        self.inner
            .lock()
            .expect("state lock")
            .last_pong
            .get(label)
            .is_some_and(|at| *at >= since)
    }

    pub fn note_menu_open(&self, at: OffsetDateTime) {
        self.inner.lock().expect("state lock").last_menu_open_at = Some(at);
    }

    pub fn last_menu_open_at(&self) -> Option<OffsetDateTime> {
        self.inner.lock().expect("state lock").last_menu_open_at
    }

    /// Claims the refresh slot; returns false when a refresh is already running so
    /// clicking "refresh" repeatedly cannot stack requests.
    pub fn begin_refresh(&self) -> bool {
        let mut inner = self.inner.lock().expect("state lock");
        if inner.refreshing {
            return false;
        }
        inner.refreshing = true;
        true
    }

    pub fn end_refresh(&self) {
        self.inner.lock().expect("state lock").refreshing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codexbar_core::model::{Confidence, RateWindow};
    use time::macros::datetime;

    fn view(id: &str, primary_used: Option<f64>) -> ProviderView {
        let mut v = ProviderView::pending(id, id, "#000000");
        if let Some(used) = primary_used {
            let mut snap = UsageSnapshot::new(datetime!(2026-09-01 12:00 UTC));
            snap.primary = Some(RateWindow::new(used));
            snap.confidence = Confidence::Exact;
            v.snapshot = Some(snap);
        }
        v
    }

    #[test]
    fn selection_defaults_to_the_first_provider() {
        let state = AppState::new(vec![view("codex", None), view("claude", None)]);
        assert_eq!(state.selected().unwrap().id, "codex");
    }

    #[test]
    fn selecting_an_unknown_provider_is_rejected() {
        let state = AppState::new(vec![view("codex", None)]);
        assert!(!state.select("nope"));
        assert!(state.select("codex"));
    }

    #[test]
    fn highest_usage_selection_picks_the_most_exhausted_provider() {
        let state = AppState::new(vec![view("codex", Some(24.0)), view("claude", Some(91.0))]);
        state.select_highest_usage();
        assert_eq!(state.selected().unwrap().id, "claude");
    }

    #[test]
    fn replacing_providers_keeps_a_valid_selection() {
        let state = AppState::new(vec![view("codex", None), view("claude", None)]);
        assert!(state.select("claude"));
        state.replace(vec![view("codex", None)]);
        assert_eq!(state.selected().unwrap().id, "codex");
    }

    #[test]
    fn liveness_replies_are_tracked_per_window() {
        let state = AppState::new(vec![view("codex", None)]);
        let asked = Instant::now();
        assert!(
            !state.answered_since("main", asked),
            "a window that never replied is not alive"
        );

        state.note_pong("main");
        assert!(state.answered_since("main", asked));
        assert!(
            !state.answered_since("settings", asked),
            "labels are independent"
        );

        // A reply from before the question does not count.
        let asked_later = Instant::now();
        assert!(!state.answered_since("main", asked_later));
    }

    #[test]
    fn refresh_slot_is_exclusive() {
        let state = AppState::new(vec![view("codex", None)]);
        assert!(state.begin_refresh());
        assert!(
            !state.begin_refresh(),
            "second claim must fail while one is running"
        );
        state.end_refresh();
        assert!(state.begin_refresh());
    }

    #[test]
    fn incidents_come_from_the_status_page_not_from_fetch_errors() {
        use codexbar_core::status::{Indicator, ProviderStatus};

        let healthy = ProviderStatus {
            indicator: Indicator::None,
            description: Some("All Systems Operational".into()),
            updated_at: None,
            components: Vec::new(),
        };
        let degraded = ProviderStatus {
            indicator: Indicator::Major,
            description: Some("Partial degradation".into()),
            updated_at: None,
            components: Vec::new(),
        };

        let view = view("codex", Some(24.0));
        assert!(!view.has_incident(), "no status yet is not an incident");
        assert!(!view
            .clone()
            .with_status(Some(healthy.clone()))
            .has_incident());
        assert!(view.clone().with_status(Some(degraded)).has_incident());

        // A failed poll must not clear a known-bad status.
        let sticky = view.with_status(Some(healthy)).with_status(None);
        assert!(sticky.status.is_some(), "previous status is preserved");
    }

    #[test]
    fn cost_estimates_attach_to_the_view() {
        let view = view("codex", None).with_cost(Some(0.51), Some(22.08));
        assert_eq!(view.cost_today, Some(0.51));
        assert_eq!(view.cost_month, Some(22.08));
    }

    #[test]
    fn remaining_fractions_are_derived_from_used_percent() {
        let v = view("codex", Some(24.0));
        assert_eq!(v.primary_remaining(), Some(0.76));
        assert_eq!(v.secondary_remaining(), None);
    }
}
