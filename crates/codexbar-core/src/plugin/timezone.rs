//! `ctx.date.nextDailyReset(timeZone, hour)`.
//!
//! Upstream computes this natively because JS engines embedded without ICU cannot resolve
//! IANA zones. Same here: QuickJS has no tz database, so the host answers using a bundled
//! one (`time-tz`). Used by at least the Crof plugin (`America/Chicago` daily reset).

use time::{Duration, OffsetDateTime};
use time_tz::{timezones, OffsetDateTimeExt, PrimitiveDateTimeExt};

/// Next occurrence of `hour:00:00` local time in `zone`, strictly after `now`.
///
/// Returns `None` for an unknown zone or an out-of-range hour, which surfaces as `NaN`
/// in JS and makes the plugin fail loudly rather than silently drift.
pub fn next_daily_reset(zone: &str, hour: u8, now: OffsetDateTime) -> Option<OffsetDateTime> {
    if hour > 23 {
        return None;
    }
    let tz = timezones::get_by_name(zone)?;
    let local = now.to_timezone(tz);

    for offset_days in 0..=2 {
        let date = local.date() + Duration::days(offset_days);
        let Ok(naive) = date.with_hms(hour, 0, 0) else {
            continue;
        };
        // DST spring-forward can delete the target hour; `unwrap_first` then yields the
        // adjacent valid instant, which is the behaviour a countdown wants.
        let candidate = naive.assume_timezone(tz).unwrap_first();
        if candidate > now {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn returns_the_next_local_reset_hour() {
        // 2026-09-01 12:00 UTC is 07:00 in Chicago (CDT, UTC-5).
        let now = datetime!(2026-09-01 12:00 UTC);
        let reset = next_daily_reset("America/Chicago", 0, now).expect("known zone");
        // Next local midnight is 2026-09-02 00:00 CDT == 05:00 UTC.
        assert_eq!(
            reset.to_offset(time::UtcOffset::UTC),
            datetime!(2026-09-02 05:00 UTC)
        );

        let later_today = next_daily_reset("America/Chicago", 18, now).expect("known zone");
        assert_eq!(
            later_today.to_offset(time::UtcOffset::UTC),
            datetime!(2026-09-01 23:00 UTC)
        );
    }

    #[test]
    fn utc_zone_is_exact() {
        let now = datetime!(2026-09-01 12:00 UTC);
        let reset = next_daily_reset("UTC", 9, now).unwrap();
        assert_eq!(
            reset.to_offset(time::UtcOffset::UTC),
            datetime!(2026-09-02 09:00 UTC)
        );
    }

    #[test]
    fn result_is_always_in_the_future() {
        let now = datetime!(2026-09-01 12:00 UTC);
        for hour in 0..24u8 {
            let reset = next_daily_reset("Europe/Berlin", hour, now).unwrap();
            assert!(reset > now, "hour {hour} produced a past reset");
            assert!(
                reset - now <= Duration::hours(25),
                "hour {hour} overshot a day"
            );
        }
    }

    #[test]
    fn rejects_unknown_zones_and_hours() {
        let now = datetime!(2026-09-01 12:00 UTC);
        assert!(next_daily_reset("Mars/Olympus", 0, now).is_none());
        assert!(next_daily_reset("UTC", 24, now).is_none());
    }
}
