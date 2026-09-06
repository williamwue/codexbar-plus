//! Display formatting shared by the tray, the popover and the CLI.

use time::Duration;

/// Compact countdown: `4h 12m`, `6d 23h`, `45m`, `now`.
/// Mirrors upstream's reset-countdown style (`UsageFormatter` / menu card rows).
pub fn duration_short(delta: Duration) -> String {
    if delta <= Duration::ZERO {
        return "now".to_string();
    }
    let total_minutes = delta.whole_minutes();
    let days = total_minutes / (24 * 60);
    let hours = (total_minutes % (24 * 60)) / 60;
    let minutes = total_minutes % 60;

    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "<1m".to_string()
    }
}

/// `89% left` style percentage, rounded like the upstream bars.
pub fn percent_left(used_percent: f64) -> String {
    let left = (100.0 - used_percent).clamp(0.0, 100.0);
    format!("{}% left", left.round() as i64)
}

/// Money with the provider's own currency code, `$12.50` for USD.
pub fn currency(amount: f64, code: Option<&str>) -> String {
    match code.unwrap_or("USD") {
        "USD" => format!("${amount:.2}"),
        "EUR" => format!("€{amount:.2}"),
        "GBP" => format!("£{amount:.2}"),
        other => format!("{amount:.2} {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_picks_two_largest_units() {
        assert_eq!(duration_short(Duration::minutes(4 * 60 + 12)), "4h 12m");
        assert_eq!(duration_short(Duration::hours(6 * 24 + 23)), "6d 23h");
        assert_eq!(duration_short(Duration::minutes(45)), "45m");
        assert_eq!(duration_short(Duration::hours(5)), "5h");
        assert_eq!(duration_short(Duration::days(7)), "7d");
        assert_eq!(duration_short(Duration::seconds(20)), "<1m");
        assert_eq!(duration_short(Duration::seconds(-5)), "now");
    }

    #[test]
    fn percent_left_is_complement_and_clamped() {
        assert_eq!(percent_left(11.4), "89% left");
        assert_eq!(percent_left(0.0), "100% left");
        assert_eq!(percent_left(180.0), "0% left");
    }

    #[test]
    fn currency_uses_known_symbols() {
        assert_eq!(currency(12.5, Some("USD")), "$12.50");
        assert_eq!(currency(12.5, None), "$12.50");
        assert_eq!(currency(3.0, Some("SEK")), "3.00 SEK");
    }
}
