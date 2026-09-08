//! RFC 3339 UTC timestamps, formatted by hand.
//!
//! A date-time crate would be a dependency bought for one `format!`, and every consumer
//! here is a `wasm32-wasip2` guest where each added dependency is bytes in the shipped
//! component. Only the one rendering these artifacts write into their records is offered:
//! UTC, millisecond precision, `Z` suffix. There is no parser and no local timezone.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current wall-clock instant as RFC 3339 UTC with millisecond precision.
///
/// `SystemTime::now()` resolves through `wasi:clocks/wall-clock` in a `wasm32-wasip2`
/// guest, so this works identically on the host and in the component. A clock reading
/// before the Unix epoch — which a host may return if its clock is unset — renders as the
/// epoch rather than failing the call that wanted a timestamp.
pub fn now_rfc3339_millis() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339_millis(since_epoch.as_millis() as i64)
}

/// RFC 3339 UTC rendering of a Unix millisecond timestamp.
pub fn format_rfc3339_millis(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    let ms_of_day = unix_ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let seconds_of_day = ms_of_day / 1000;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60,
        ms_of_day % 1000
    )
}

/// Days since the Unix epoch to a proleptic Gregorian `(year, month, day)`, via Howard
/// Hinnant's `civil_from_days`.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_render_as_rfc_3339_utc() {
        assert_eq!(format_rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339_millis(1_000), "1970-01-01T00:00:01.000Z");
        assert_eq!(format_rfc3339_millis(1_774_000_000_123), "2026-03-20T09:46:40.123Z");
        assert_eq!(format_rfc3339_millis(1_767_225_600_123), "2026-01-01T00:00:00.123Z");
    }

    #[test]
    fn a_leap_day_is_rendered_as_itself() {
        assert_eq!(format_rfc3339_millis(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn the_current_instant_has_the_shape_a_consumer_parses() {
        let now = now_rfc3339_millis();
        assert_eq!(now.len(), 24, "{now}");
        assert!(now.ends_with('Z'), "{now}");
        assert!(now.starts_with("20"), "{now}");
    }
}
