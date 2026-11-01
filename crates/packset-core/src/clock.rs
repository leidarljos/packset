//! UTC timestamps in the one format the store round-trips.
//!
//! Atoms compare timestamps as strings, so the format has to sort the way the
//! instants do: fixed width, milliseconds, `Z`. A shorter or longer fraction
//! sorts wrong against what is already on disk.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the epoch, formatted as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
#[must_use]
pub fn format_millis(millis: i64) -> String {
    let (days, ms_of_day) = {
        let d = millis.div_euclid(86_400_000);
        let r = millis.rem_euclid(86_400_000);
        (d, r)
    };
    let (year, month, day) = civil_from_days(days);
    let ms = ms_of_day % 1000;
    let secs = ms_of_day / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

/// Now, in the store's format.
#[must_use]
pub fn utcnow() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    format_millis(millis)
}

/// Parse a stored timestamp back to milliseconds since the epoch.
///
/// Accepts the `Z` form this module writes and the `+00:00` offset form,
/// since both are already on disk.
#[must_use]
pub fn parse_millis(text: &str) -> Option<i64> {
    let raw = text.trim();
    let raw = raw.strip_suffix('Z').unwrap_or(raw);
    let raw = raw.strip_suffix("+00:00").unwrap_or(raw);
    let (date, time) = raw.split_once('T').or_else(|| raw.split_once(' '))?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    let (clock, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next().unwrap_or("0").parse().ok()?;
    // A fraction is any width on disk; take milliseconds and ignore the rest.
    let mut millis = 0i64;
    for (i, ch) in frac.chars().take(3).enumerate() {
        let digit = ch.to_digit(10)? as i64;
        millis += digit * 10i64.pow(2 - i as u32);
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + millis)
}

/// Rewrite any stamp [`parse_millis`] accepts into the store form.
///
/// Atoms compare timestamps as strings, so a caller-supplied `+00:00`,
/// space instead of `T`, or missing-millis form has to become
/// `YYYY-MM-DDTHH:MM:SS.mmmZ` before it is compared to `valid_from` /
/// `valid_to`.
#[must_use]
pub fn canonicalize(text: &str) -> Option<String> {
    parse_millis(text).map(format_millis)
}

/// A stored timestamp shifted by whole seconds, in the same format.
#[must_use]
pub fn shift(text: &str, seconds: i64) -> Option<String> {
    parse_millis(text).map(|ms| format_millis(ms + seconds * 1000))
}

/// Days between two stored timestamps, floored at zero.
#[must_use]
pub fn elapsed_days(from: &str, to: &str) -> f64 {
    match (parse_millis(from), parse_millis(to)) {
        (Some(a), Some(b)) => ((b - a) as f64 / 86_400_000.0).max(0.0),
        _ => 0.0,
    }
}

/// Days since the epoch for a civil date. Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((if m <= 2 { y + 1 } else { y }), m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_format_sorts_the_way_the_instants_do() {
        let early = format_millis(1_700_000_000_000);
        let late = format_millis(1_700_000_001_000);
        assert!(early < late, "{early} {late}");
        assert_eq!(early.len(), 24, "{early}");
        assert!(early.ends_with('Z'));
    }

    #[test]
    fn a_stamp_round_trips() {
        for ms in [0, 1, 1_700_000_000_123, 253_370_764_800_000] {
            let text = format_millis(ms);
            assert_eq!(parse_millis(&text), Some(ms), "{text}");
        }
    }

    #[test]
    fn a_known_instant_reads_back_as_written() {
        assert_eq!(format_millis(1_767_225_600_000), "2026-01-01T00:00:00.000Z");
        assert_eq!(
            parse_millis("2026-01-01T00:00:00.000Z"),
            Some(1_767_225_600_000)
        );
    }

    #[test]
    fn the_offset_form_already_on_disk_still_parses() {
        // Records on disk carry both spellings of the same instant, so a
        // reader that took only one would silently drop half of them.
        assert_eq!(
            parse_millis("2026-01-01T00:00:00+00:00"),
            parse_millis("2026-01-01T00:00:00.000Z")
        );
    }

    #[test]
    fn a_caller_stamp_rewrites_to_the_store_form() {
        assert_eq!(
            canonicalize("2024-06-01T00:00:00+00:00").as_deref(),
            Some("2024-06-01T00:00:00.000Z")
        );
        assert_eq!(
            canonicalize("2024-06-01T00:00:00.000Z").as_deref(),
            Some("2024-06-01T00:00:00.000Z")
        );
        assert_eq!(
            canonicalize("2024-06-01 00:00:00").as_deref(),
            Some("2024-06-01T00:00:00.000Z")
        );
        assert_eq!(
            canonicalize("2024-06-01 00:00:00.000Z").as_deref(),
            Some("2024-06-01T00:00:00.000Z")
        );
        assert_eq!(canonicalize("not-a-date"), None);
    }

    #[test]
    fn a_fraction_of_any_width_takes_milliseconds() {
        assert_eq!(
            parse_millis("2026-01-01T00:00:00.123456Z"),
            parse_millis("2026-01-01T00:00:00.123Z")
        );
        assert_eq!(
            parse_millis("2026-01-01T00:00:00.1Z"),
            parse_millis("2026-01-01T00:00:00.100Z")
        );
    }

    #[test]
    fn shifting_a_day_lands_on_the_next_one() {
        assert_eq!(
            shift("2026-01-01T00:00:00.000Z", 86_400).as_deref(),
            Some("2026-01-02T00:00:00.000Z")
        );
        assert_eq!(
            shift("2026-02-28T00:00:00.000Z", 86_400).as_deref(),
            Some("2026-03-01T00:00:00.000Z")
        );
    }

    #[test]
    fn elapsed_is_floored_at_zero() {
        let a = "2026-01-01T00:00:00.000Z";
        let b = "2026-01-03T00:00:00.000Z";
        assert!((elapsed_days(a, b) - 2.0).abs() < 1e-9);
        assert_eq!(elapsed_days(b, a), 0.0);
        assert_eq!(elapsed_days("not a stamp", b), 0.0);
    }

    #[test]
    fn a_leap_day_survives_the_round_trip() {
        let text = "2028-02-29T12:34:56.789Z";
        assert_eq!(parse_millis(text).map(format_millis).as_deref(), Some(text));
    }
}
