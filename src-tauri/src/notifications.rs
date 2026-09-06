//! Quota alert edge detection. OS delivery stays in `main`; this module only decides
//! when a provider crossed the warning boundary, which keeps refresh behavior testable.

use std::collections::HashSet;

use crate::state::ProviderView;

const ALERT_USED_PERCENT: f64 = 90.0;
const REARM_USED_PERCENT: f64 = 85.0;

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaAlert {
    pub provider_id: String,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Default)]
pub struct QuotaAlertTracker {
    alerted: HashSet<String>,
}

impl QuotaAlertTracker {
    /// Emits once at 90% used, then rearms only after usage falls below 85%.
    /// The hysteresis prevents rounded or eventually-consistent readings from spamming Toasts.
    pub fn evaluate(&mut self, providers: &[ProviderView]) -> Vec<QuotaAlert> {
        let active: HashSet<&str> = providers
            .iter()
            .map(|provider| provider.id.as_str())
            .collect();
        self.alerted.retain(|id| active.contains(id.as_str()));

        let mut alerts = Vec::new();
        for provider in providers {
            let Some(used_percent) = provider.worst_used_percent() else {
                continue;
            };

            if used_percent < REARM_USED_PERCENT {
                self.alerted.remove(&provider.id);
                continue;
            }
            if used_percent < ALERT_USED_PERCENT || !self.alerted.insert(provider.id.clone()) {
                continue;
            }

            let remaining = (100.0 - used_percent).clamp(0.0, 100.0);
            alerts.push(QuotaAlert {
                provider_id: provider.id.clone(),
                title: format!("{} quota is running low", provider.display_name),
                body: format!("{remaining:.0}% remaining in the most constrained window."),
            });
        }
        alerts
    }
}

#[cfg(test)]
mod tests {
    use codexbar_core::model::{FetchKind, FetchResult, RateWindow, UsageSnapshot};
    use time::OffsetDateTime;

    use super::*;

    fn provider(id: &str, used_percent: f64) -> ProviderView {
        let mut usage = UsageSnapshot::new(OffsetDateTime::UNIX_EPOCH);
        usage.primary = Some(RateWindow::new(used_percent));
        let result = FetchResult {
            usage,
            strategy_id: "test".to_string(),
            strategy_kind: FetchKind::ApiToken,
        };
        ProviderView::pending(id, id, "#000000").with_result(result, OffsetDateTime::UNIX_EPOCH)
    }

    #[test]
    fn alerts_once_until_usage_recovers_below_hysteresis() {
        let mut tracker = QuotaAlertTracker::default();

        assert!(tracker.evaluate(&[provider("codex", 89.9)]).is_empty());
        let first = tracker.evaluate(&[provider("codex", 90.0)]);
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].body,
            "10% remaining in the most constrained window."
        );
        assert!(tracker.evaluate(&[provider("codex", 96.0)]).is_empty());
        assert!(tracker.evaluate(&[provider("codex", 85.0)]).is_empty());
        assert!(tracker.evaluate(&[provider("codex", 84.9)]).is_empty());
        assert_eq!(tracker.evaluate(&[provider("codex", 91.0)]).len(), 1);
    }

    #[test]
    fn removed_provider_rearms_when_it_returns() {
        let mut tracker = QuotaAlertTracker::default();
        assert_eq!(tracker.evaluate(&[provider("codex", 99.0)]).len(), 1);
        assert!(tracker.evaluate(&[]).is_empty());
        assert_eq!(tracker.evaluate(&[provider("codex", 99.0)]).len(), 1);
    }
}
