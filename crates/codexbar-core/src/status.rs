//! Provider status polling.
//!
//! Port of `Sources/CodexBar/UsageStore+Status.swift:1-140,269-348` and the indicator
//! mapping in `UsageStoreSupport.swift:3-79`.
//!
//! Statuspage-style feeds are read in two requests, exactly like upstream:
//! `/api/v2/status.json` for the headline indicator and `/api/v2/components.json` for the
//! component rows. `summary.json` is deliberately not used for components — upstream only
//! ever decodes the top-level `status` from it.
//!
//! A failed poll is not an outage: callers keep the previous status
//! (`UsageStore+ProviderStatus.swift:32-36`).

use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::http::HttpClient;

/// Upstream uses a 10-second timeout for every status request.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Severity, in the app's own vocabulary (`ProviderStatusIndicator`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Indicator {
    None,
    Minor,
    Major,
    Critical,
    Maintenance,
    Unknown,
}

impl Indicator {
    /// Anything other than `none` is an issue worth surfacing.
    pub fn has_issue(self) -> bool {
        self != Indicator::None
    }

    /// Which tray overlay to draw (`IconRenderer.swift:1009-1045`):
    /// minor and maintenance get a dot, the louder states get an exclamation mark.
    pub fn overlay(self) -> Overlay {
        match self {
            Indicator::None => Overlay::None,
            Indicator::Minor | Indicator::Maintenance => Overlay::Dot,
            Indicator::Major | Indicator::Critical | Indicator::Unknown => Overlay::Exclamation,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Indicator::None => "Operational",
            Indicator::Minor => "Degraded performance",
            Indicator::Major => "Partial outage",
            Indicator::Critical => "Major outage",
            Indicator::Maintenance => "Under maintenance",
            Indicator::Unknown => "Status unknown",
        }
    }

    /// Definite outage for hook transitions (`HookTransitionDetector.swift:53-64`):
    /// `Some(true)` outage, `Some(false)` healthy, `None` indeterminate.
    pub fn outage_state(self) -> Option<bool> {
        match self {
            Indicator::None => Some(false),
            Indicator::Minor | Indicator::Major | Indicator::Critical => Some(true),
            Indicator::Maintenance | Indicator::Unknown => None,
        }
    }

    /// Raw statuspage vocabulary → ours (`UsageStoreSupport.swift:66-79`).
    pub fn from_raw(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "none" | "operational" => Indicator::None,
            "minor" | "degraded_performance" => Indicator::Minor,
            "major" | "partial_outage" => Indicator::Major,
            "critical" | "major_outage" | "full_outage" => Indicator::Critical,
            "maintenance" | "under_maintenance" => Indicator::Maintenance,
            _ => Indicator::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Dot,
    Exclamation,
}

/// One component row, e.g. "Codex API — Major outage".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Component {
    pub name: String,
    pub indicator: Indicator,
    /// Group components are headers for their children.
    pub group: bool,
}

/// A provider's status as the tray and menu render it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderStatus {
    pub indicator: Indicator,
    /// Provider-supplied text, e.g. "Partial degradation"; falls back to the label.
    pub description: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
}

impl ProviderStatus {
    pub fn summary(&self) -> String {
        match &self.description {
            Some(text) if !text.trim().is_empty() => text.clone(),
            _ => self.indicator.label().to_string(),
        }
    }

    /// Components that are not operational, which is all the menu shows.
    pub fn issues(&self) -> impl Iterator<Item = &Component> {
        self.components.iter().filter(|c| c.indicator.has_issue())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("provider has no status page")]
    NoStatusPage,
    #[error("status page URL is invalid: {0}")]
    InvalidUrl(String),
    #[error("status request failed: {0}")]
    Http(String),
    #[error("status response was not valid JSON: {0}")]
    Decode(String),
}

// MARK: - wire models (`UsageStore+Status.swift:269-307`)

#[derive(Debug, Deserialize)]
struct StatusResponse {
    #[serde(default)]
    page: Option<Page>,
    status: Status,
}

#[derive(Debug, Deserialize)]
struct Page {
    #[serde(default, rename = "updated_at")]
    updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Status {
    indicator: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ComponentsResponse {
    #[serde(default)]
    components: Option<Vec<RawComponent>>,
}

#[derive(Debug, Deserialize)]
struct RawComponent {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    group: Option<bool>,
    #[serde(default)]
    position: Option<i64>,
}

/// Normalises a status page origin into the two endpoints upstream requests.
fn endpoints(status_page_url: &str) -> Result<(String, String), StatusError> {
    let trimmed = status_page_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(StatusError::NoStatusPage);
    }
    // Descriptors may already point at an API path; keep only the origin.
    let base = match url::Url::parse(trimmed) {
        Ok(url) => {
            let host = url
                .host_str()
                .ok_or_else(|| StatusError::InvalidUrl(status_page_url.to_string()))?;
            match url.port() {
                Some(port) => format!("{}://{host}:{port}", url.scheme()),
                None => format!("{}://{host}", url.scheme()),
            }
        }
        Err(_) => return Err(StatusError::InvalidUrl(status_page_url.to_string())),
    };
    Ok((
        format!("{base}/api/v2/status.json"),
        format!("{base}/api/v2/components.json"),
    ))
}

/// Fetches a provider's status. Components are best-effort: a failure there still yields
/// the headline indicator.
pub async fn fetch(
    client: &HttpClient,
    status_page_url: &str,
) -> Result<ProviderStatus, StatusError> {
    let (status_url, components_url) = endpoints(status_page_url)?;

    let response = client
        .send(Method::GET, || {
            client
                .raw()
                .get(&status_url)
                .timeout(TIMEOUT)
                .header(reqwest::header::ACCEPT, "application/json")
        })
        .await
        .map_err(|e| StatusError::Http(e.to_string()))?;

    if let Some(err) = response.error_for_status() {
        return Err(StatusError::Http(err.to_string()));
    }
    let parsed: StatusResponse = response
        .json()
        .map_err(|e| StatusError::Decode(e.to_string()))?;

    let mut status = ProviderStatus {
        indicator: Indicator::from_raw(&parsed.status.indicator),
        description: parsed.status.description.filter(|d| !d.trim().is_empty()),
        updated_at: parsed.page.and_then(|p| p.updated_at).and_then(|raw| {
            OffsetDateTime::parse(&raw, &time::format_description::well_known::Rfc3339).ok()
        }),
        components: Vec::new(),
    };

    match fetch_components(client, &components_url).await {
        Ok(components) => status.components = components,
        Err(err) => {
            tracing::debug!(url = %components_url, error = %err, "status components unavailable")
        }
    }

    Ok(status)
}

async fn fetch_components(client: &HttpClient, url: &str) -> Result<Vec<Component>, StatusError> {
    let response = client
        .send(Method::GET, || {
            client
                .raw()
                .get(url)
                .timeout(TIMEOUT)
                .header(reqwest::header::ACCEPT, "application/json")
        })
        .await
        .map_err(|e| StatusError::Http(e.to_string()))?;
    if let Some(err) = response.error_for_status() {
        return Err(StatusError::Http(err.to_string()));
    }
    let parsed: ComponentsResponse = response
        .json()
        .map_err(|e| StatusError::Decode(e.to_string()))?;
    Ok(map_components(parsed.components.unwrap_or_default()))
}

/// Trims empty names and orders by the page's own `position`
/// (`UsageStore+Status.swift:294-348`).
fn map_components(raw: Vec<RawComponent>) -> Vec<Component> {
    let mut components: Vec<(i64, Component)> = raw
        .into_iter()
        .filter_map(|component| {
            let name = component.name?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some((
                component.position.unwrap_or(i64::MAX),
                Component {
                    name,
                    indicator: Indicator::from_raw(component.status.as_deref().unwrap_or("")),
                    group: component.group.unwrap_or(false),
                },
            ))
        })
        .collect();
    components.sort_by_key(|(position, _)| *position);
    components
        .into_iter()
        .map(|(_, component)| component)
        .collect()
}

/// Edge-triggered outage detection (`HookTransitionDetector.swift:161-220`).
///
/// The first definite observation only establishes a baseline; later flips report. An
/// indeterminate reading (maintenance/unknown) never disturbs the baseline, so a flaky
/// status page cannot spam notifications.
#[derive(Debug, Clone, Default)]
pub struct OutageTracker {
    baseline: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    None,
    BecameUnavailable,
    Recovered,
}

impl OutageTracker {
    pub fn observe(&mut self, indicator: Indicator) -> Transition {
        let Some(state) = indicator.outage_state() else {
            return Transition::None;
        };
        match self.baseline {
            None => {
                self.baseline = Some(state);
                Transition::None
            }
            Some(previous) if previous == state => Transition::None,
            Some(_) => {
                self.baseline = Some(state);
                if state {
                    Transition::BecameUnavailable
                } else {
                    Transition::Recovered
                }
            }
        }
    }

    /// A failed poll must not move the baseline.
    pub fn observe_failure(&self) -> Transition {
        Transition::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_status_vocabulary_maps_to_severities() {
        for (raw, expected) in [
            ("none", Indicator::None),
            ("operational", Indicator::None),
            ("minor", Indicator::Minor),
            ("degraded_performance", Indicator::Minor),
            ("major", Indicator::Major),
            ("partial_outage", Indicator::Major),
            ("critical", Indicator::Critical),
            ("major_outage", Indicator::Critical),
            ("full_outage", Indicator::Critical),
            ("under_maintenance", Indicator::Maintenance),
            ("something_new", Indicator::Unknown),
            ("", Indicator::Unknown),
        ] {
            assert_eq!(Indicator::from_raw(raw), expected, "{raw}");
        }
    }

    #[test]
    fn overlays_follow_upstreams_dot_versus_exclamation_split() {
        assert_eq!(Indicator::None.overlay(), Overlay::None);
        assert_eq!(Indicator::Minor.overlay(), Overlay::Dot);
        assert_eq!(Indicator::Maintenance.overlay(), Overlay::Dot);
        assert_eq!(Indicator::Major.overlay(), Overlay::Exclamation);
        assert_eq!(Indicator::Critical.overlay(), Overlay::Exclamation);
        assert_eq!(Indicator::Unknown.overlay(), Overlay::Exclamation);
    }

    #[test]
    fn endpoints_are_derived_from_the_origin_only() {
        let (status, components) = endpoints("https://status.openai.com/").unwrap();
        assert_eq!(status, "https://status.openai.com/api/v2/status.json");
        assert_eq!(
            components,
            "https://status.openai.com/api/v2/components.json"
        );

        // A descriptor pointing at a deeper path still yields the API endpoints.
        let (status, _) = endpoints("https://status.anthropic.com/api/v2/summary.json").unwrap();
        assert_eq!(status, "https://status.anthropic.com/api/v2/status.json");

        assert!(matches!(endpoints("   "), Err(StatusError::NoStatusPage)));
        assert!(matches!(
            endpoints("not a url"),
            Err(StatusError::InvalidUrl(_))
        ));
    }

    #[test]
    fn components_are_named_sorted_and_severity_mapped() {
        let raw: Vec<RawComponent> = serde_json::from_str(
            r#"[
                { "name": "CLI", "status": "operational", "position": 3 },
                { "name": "  ", "status": "major_outage", "position": 1 },
                { "name": "Codex API", "status": "major_outage", "position": 2 },
                { "name": "Chat Completions", "status": "degraded_performance", "position": 1 },
                { "name": "APIs", "status": "operational", "group": true, "position": 0 }
            ]"#,
        )
        .unwrap();
        let components = map_components(raw);

        assert_eq!(components.len(), 4, "the unnamed component is dropped");
        assert_eq!(components[0].name, "APIs");
        assert!(components[0].group);
        assert_eq!(components[1].name, "Chat Completions");
        assert_eq!(components[1].indicator, Indicator::Minor);
        assert_eq!(components[2].name, "Codex API");
        assert_eq!(components[2].indicator, Indicator::Critical);

        let status = ProviderStatus {
            indicator: Indicator::Major,
            description: None,
            updated_at: None,
            components,
        };
        assert_eq!(
            status.issues().count(),
            2,
            "only non-operational rows are issues"
        );
        assert_eq!(
            status.summary(),
            "Partial outage",
            "falls back to the label"
        );
    }

    #[test]
    fn description_wins_over_the_label() {
        let status = ProviderStatus {
            indicator: Indicator::Major,
            description: Some("Partial degradation".into()),
            updated_at: None,
            components: Vec::new(),
        };
        assert_eq!(status.summary(), "Partial degradation");
    }

    #[test]
    fn outage_transitions_are_edge_triggered_and_ignore_indeterminate_readings() {
        let mut tracker = OutageTracker::default();
        // First definite observation is only a baseline.
        assert_eq!(tracker.observe(Indicator::None), Transition::None);
        assert_eq!(tracker.observe(Indicator::None), Transition::None);
        assert_eq!(
            tracker.observe(Indicator::Major),
            Transition::BecameUnavailable
        );
        assert_eq!(
            tracker.observe(Indicator::Critical),
            Transition::None,
            "still down"
        );
        // Maintenance/unknown must not flip anything.
        assert_eq!(tracker.observe(Indicator::Maintenance), Transition::None);
        assert_eq!(tracker.observe(Indicator::Unknown), Transition::None);
        assert_eq!(tracker.observe(Indicator::None), Transition::Recovered);
        assert_eq!(tracker.observe_failure(), Transition::None);
    }

    #[test]
    fn first_observation_of_an_outage_still_only_sets_the_baseline() {
        let mut tracker = OutageTracker::default();
        assert_eq!(
            tracker.observe(Indicator::Critical),
            Transition::None,
            "no notification for state we joined in"
        );
        assert_eq!(tracker.observe(Indicator::None), Transition::Recovered);
    }
}
