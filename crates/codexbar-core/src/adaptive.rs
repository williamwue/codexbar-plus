//! Adaptive refresh decision table.
//!
//! Line-for-line port of `Sources/AdaptiveRefreshCore/AdaptiveRefreshPolicyCore.swift:53-103`.
//! Pure math, no platform calls: the caller supplies the signals (Windows adapters read
//! power state and coding activity and pass them in).

use time::{Duration, OffsetDateTime};

/// Normalized thermal/power signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalPressure {
    Nominal,
    Constrained,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    RecentInteraction,
    CodingActivity,
    Warm,
    Idle,
    LongIdle,
    Constrained,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::RecentInteraction => "recentInteraction",
            Reason::CodingActivity => "codingActivity",
            Reason::Warm => "warm",
            Reason::Idle => "idle",
            Reason::LongIdle => "longIdle",
            Reason::Constrained => "constrained",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    pub delay: Duration,
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy)]
pub struct Input {
    pub now: OffsetDateTime,
    pub last_menu_open_at: Option<OffsetDateTime>,
    pub last_coding_activity_at: Option<OffsetDateTime>,
    pub low_power_mode: bool,
    pub thermal_pressure: ThermalPressure,
}

const RECENT_INTERACTION_THRESHOLD: Duration = Duration::minutes(5);
const WARM_THRESHOLD: Duration = Duration::hours(1);
const IDLE_THRESHOLD: Duration = Duration::hours(4);
const CODING_ACTIVITY_THRESHOLD: Duration = Duration::minutes(5);

const RECENT_INTERACTION_DELAY: Duration = Duration::minutes(2);
const WARM_DELAY: Duration = Duration::minutes(5);
const IDLE_DELAY: Duration = Duration::minutes(15);
const LONG_IDLE_DELAY: Duration = Duration::minutes(30);
const CONSTRAINED_DELAY: Duration = Duration::minutes(30);
const CODING_ACTIVITY_DELAY_CAP: Duration = Duration::minutes(5);

/// Cadence for callers that need one number without live state.
pub const NOMINAL_INTERVAL: Duration = Duration::minutes(5);

/// Decides the next poll delay.
///
/// Precedence: constrained wins outright; otherwise menu-age picks the base delay and
/// recent coding activity caps it at 5 minutes.
pub fn next_delay(input: Input) -> Decision {
    if input.low_power_mode || input.thermal_pressure == ThermalPressure::Constrained {
        return Decision {
            delay: CONSTRAINED_DELAY,
            reason: Reason::Constrained,
        };
    }

    let base = menu_activity_decision(input);
    let Some(coding) = input.last_coding_activity_at else {
        return base;
    };
    if input.now - coding < CODING_ACTIVITY_THRESHOLD && base.delay > CODING_ACTIVITY_DELAY_CAP {
        return Decision {
            delay: CODING_ACTIVITY_DELAY_CAP,
            reason: Reason::CodingActivity,
        };
    }
    base
}

fn menu_activity_decision(input: Input) -> Decision {
    let Some(last_open) = input.last_menu_open_at else {
        return Decision {
            delay: LONG_IDLE_DELAY,
            reason: Reason::LongIdle,
        };
    };

    // A future timestamp yields a negative age, which reads as "just interacted".
    let age = input.now - last_open;

    if age <= RECENT_INTERACTION_THRESHOLD {
        Decision {
            delay: RECENT_INTERACTION_DELAY,
            reason: Reason::RecentInteraction,
        }
    } else if age <= WARM_THRESHOLD {
        Decision {
            delay: WARM_DELAY,
            reason: Reason::Warm,
        }
    } else if age < IDLE_THRESHOLD {
        Decision {
            delay: IDLE_DELAY,
            reason: Reason::Idle,
        }
    } else {
        Decision {
            delay: LONG_IDLE_DELAY,
            reason: Reason::LongIdle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-01 12:00 UTC);

    fn input(menu_age: Option<Duration>) -> Input {
        Input {
            now: NOW,
            last_menu_open_at: menu_age.map(|age| NOW - age),
            last_coding_activity_at: None,
            low_power_mode: false,
            thermal_pressure: ThermalPressure::Nominal,
        }
    }

    #[test]
    fn menu_age_selects_the_delay_ladder() {
        assert_eq!(
            next_delay(input(Some(Duration::minutes(1)))).delay,
            Duration::minutes(2)
        );
        assert_eq!(
            next_delay(input(Some(Duration::minutes(5)))).delay,
            Duration::minutes(2)
        );
        assert_eq!(
            next_delay(input(Some(Duration::minutes(6)))).delay,
            Duration::minutes(5)
        );
        assert_eq!(
            next_delay(input(Some(Duration::minutes(60)))).delay,
            Duration::minutes(5)
        );
        assert_eq!(
            next_delay(input(Some(Duration::minutes(61)))).delay,
            Duration::minutes(15)
        );
        assert_eq!(
            next_delay(input(Some(Duration::hours(4)))).delay,
            Duration::minutes(30),
            "exactly 4h is long idle, not idle"
        );
    }

    #[test]
    fn never_opened_menu_polls_slowly() {
        let decision = next_delay(input(None));
        assert_eq!(decision.delay, Duration::minutes(30));
        assert_eq!(decision.reason, Reason::LongIdle);
    }

    #[test]
    fn constrained_power_overrides_everything_including_coding_activity() {
        let mut i = input(Some(Duration::minutes(1)));
        i.low_power_mode = true;
        i.last_coding_activity_at = Some(NOW);
        let decision = next_delay(i);
        assert_eq!(decision.delay, Duration::minutes(30));
        assert_eq!(decision.reason, Reason::Constrained);

        let mut i = input(Some(Duration::minutes(1)));
        i.thermal_pressure = ThermalPressure::Constrained;
        assert_eq!(next_delay(i).reason, Reason::Constrained);
    }

    #[test]
    fn coding_activity_caps_slow_lanes_but_never_slows_fast_ones() {
        let mut i = input(Some(Duration::hours(6)));
        i.last_coding_activity_at = Some(NOW - Duration::minutes(1));
        let decision = next_delay(i);
        assert_eq!(decision.delay, Duration::minutes(5));
        assert_eq!(decision.reason, Reason::CodingActivity);

        // Already faster than the cap: leave it alone.
        let mut i = input(Some(Duration::minutes(1)));
        i.last_coding_activity_at = Some(NOW - Duration::minutes(1));
        let decision = next_delay(i);
        assert_eq!(decision.delay, Duration::minutes(2));
        assert_eq!(decision.reason, Reason::RecentInteraction);

        // Stale coding activity does not cap anything.
        let mut i = input(Some(Duration::hours(6)));
        i.last_coding_activity_at = Some(NOW - Duration::minutes(6));
        assert_eq!(next_delay(i).delay, Duration::minutes(30));
    }

    #[test]
    fn clock_skew_into_the_future_reads_as_recent() {
        let mut i = input(None);
        i.last_menu_open_at = Some(NOW + Duration::minutes(30));
        assert_eq!(next_delay(i).reason, Reason::RecentInteraction);
    }
}
