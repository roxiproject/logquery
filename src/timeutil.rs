//! Time helpers: duration literals, RFC 3339 timestamps and fixed-offset zones.
//!
//! Logs carry timestamps as text, and questions about logs are usually about
//! recency ("the last 15 minutes"). Both ends of that need arithmetic on a
//! common scale, so everything here converts to and from **epoch seconds** as
//! an `f64`: whole seconds are exact well past the year 2100 and a fractional
//! part carries sub-second precision without a second type.
//!
//! There is no calendar dependency. Dates are converted with the days-from-civil
//! algorithm, which is exact for the proleptic Gregorian calendar, and zones are
//! fixed offsets — logquery never needs a zone database because it never has to
//! know when a zone's rules changed.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, now.
pub fn now_epoch() -> f64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        // Before 1970 the clock is going backwards; report the epoch itself
        // rather than an error nobody can act on.
        Err(_) => 0.0,
    }
}

/// Parse a duration literal such as `30s`, `15m`, `2h`, `7d` or `1.5h`.
///
/// The number may be fractional but not negative — a negative window is written
/// with the subtraction, as `now() - 1h`. Returns the duration in seconds.
pub fn parse_duration(text: &str) -> Option<f64> {
    let (digits, unit) = split_unit(text)?;
    if digits.is_empty() {
        return None;
    }
    let n: f64 = digits.parse().ok()?;
    if !n.is_finite() || n < 0.0 {
        return None;
    }
    let scale = match unit {
        "ms" => 0.001,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        _ => return None,
    };
    Some(n * scale)
}

/// Split a duration into its numeric head and its unit suffix.
fn split_unit(text: &str) -> Option<(&str, &str)> {
    let idx = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    Some((&text[..idx], &text[idx..]))
}

/// True when `unit` is a duration suffix the lexer should attach to a number.
pub fn is_duration_unit(unit: &str) -> bool {
    matches!(unit, "ms" | "s" | "m" | "h" | "d")
}

/// Parse an RFC 3339 / ISO 8601 timestamp into epoch seconds.
///
/// Accepted: `2026-07-27T09:14:15Z`, `2026-07-27T09:14:15.250Z`,
/// `2026-07-27T09:14:15+02:00`, and the same with a space instead of `T`. A
/// timestamp with no zone is read as UTC, which is what log emitters mean when
/// they omit it.
pub fn parse_rfc3339(s: &str) -> Option<f64> {
    let s = s.trim();
    let b: Vec<char> = s.chars().collect();
    if b.len() < 10 {
        return None;
    }
    let year: i64 = digits(&b[0..4])?;
    if b[4] != '-' {
        return None;
    }
    let month: i64 = digits(&b[5..7])?;
    if b[7] != '-' {
        return None;
    }
    let day: i64 = digits(&b[8..10])?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut secs = days_from_civil(year, month as u32, day as u32) as f64 * 86400.0;
    if b.len() == 10 {
        return Some(secs);
    }

    // Time part.
    if !matches!(b[10], 'T' | 't' | ' ') || b.len() < 16 {
        return None;
    }
    let hour: i64 = digits(&b[11..13])?;
    if b[13] != ':' {
        return None;
    }
    let min: i64 = digits(&b[14..16])?;
    if hour > 23 || min > 59 {
        return None;
    }
    secs += (hour * 3600 + min * 60) as f64;

    let mut i = 16;
    if i < b.len() && b[i] == ':' {
        if b.len() < i + 3 {
            return None;
        }
        let sec: i64 = digits(&b[i + 1..i + 3])?;
        // A leap second is reported as :60; clamp it rather than reject it.
        secs += sec.min(60) as f64;
        i += 3;
        if i < b.len() && b[i] == '.' {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j == start {
                return None;
            }
            let frac: String = b[start..j].iter().collect();
            secs += format!("0.{frac}").parse::<f64>().ok()?;
            i = j;
        }
    }

    // Zone.
    if i >= b.len() {
        return Some(secs);
    }
    match b[i] {
        'Z' | 'z' if i + 1 == b.len() => Some(secs),
        '+' | '-' => {
            let rest: String = b[i..].iter().collect();
            let offset = parse_offset(&rest)?;
            Some(secs - offset as f64 * 60.0)
        }
        _ => None,
    }
}

/// Parse `+HH:MM`, `-HH:MM`, `+HHMM` or `+HH` into minutes east of UTC.
fn parse_offset(s: &str) -> Option<i32> {
    let b: Vec<char> = s.chars().collect();
    let sign = match b.first()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let hh: i64 = digits(b.get(1..3)?)?;
    let mm: i64 = match b.len() {
        3 => 0,
        5 if b[3] == ':' => digits(&b[4..6])?,
        _ if b.len() == 5 => digits(&b[3..5])?,
        6 if b[3] == ':' => digits(&b[4..6])?,
        _ => return None,
    };
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh * 60 + mm) as i32)
}

fn digits(chars: &[char]) -> Option<i64> {
    if chars.is_empty() || !chars.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    chars.iter().collect::<String>().parse().ok()
}

/// Parse a `--tz` value into minutes east of UTC.
///
/// Accepted: `utc`, `z`, `local`, and fixed offsets such as `+02:00`, `-0700`
/// or `+05:30`. Named zones are deliberately not supported: resolving them
/// correctly needs a zone database, and guessing is worse than refusing.
pub fn parse_tz(spec: &str) -> Option<i32> {
    match spec.trim().to_ascii_lowercase().as_str() {
        "utc" | "z" | "gmt" => Some(0),
        "local" => Some(local_offset_minutes()),
        other => parse_offset(other),
    }
}

/// The machine's current UTC offset, in minutes.
///
/// Derived by comparing the system clock's civil time (via `SystemTime`) with
/// the same instant rendered by the C library through the `TZ`-aware path is
/// not available without a dependency, so the offset comes from the `TZ`
/// environment variable when it holds a fixed offset, and is otherwise 0.
fn local_offset_minutes() -> i32 {
    std::env::var("TZ")
        .ok()
        .and_then(|tz| parse_offset(tz.trim()))
        .unwrap_or(0)
}

/// Render epoch seconds as RFC 3339 at a fixed offset.
///
/// Sub-second precision is kept to milliseconds when it is non-zero, so a
/// whole-second timestamp round-trips to exactly the text it came from.
pub fn format_epoch(secs: f64, offset_minutes: i32) -> String {
    if !secs.is_finite() {
        return String::new();
    }
    let shifted = secs + offset_minutes as f64 * 60.0;
    let whole = shifted.floor();
    let mut frac_ms = ((shifted - whole) * 1000.0).round() as i64;
    let mut total = whole as i64;
    if frac_ms >= 1000 {
        frac_ms -= 1000;
        total += 1;
    }
    let days = total.div_euclid(86400);
    let rem = total.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let mut out = format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}");
    if frac_ms != 0 {
        out.push_str(&format!(".{frac_ms:03}"));
    }
    if offset_minutes == 0 {
        out.push('Z');
    } else {
        let sign = if offset_minutes < 0 { '-' } else { '+' };
        let a = offset_minutes.abs();
        out.push_str(&format!("{sign}{:02}:{:02}", a / 60, a % 60));
    }
    out
}

/// Days since 1970-01-01 for a civil date. Hinnant's algorithm, exact for the
/// proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_scale_by_unit() {
        assert_eq!(parse_duration("30s"), Some(30.0));
        assert_eq!(parse_duration("15m"), Some(900.0));
        assert_eq!(parse_duration("2h"), Some(7200.0));
        assert_eq!(parse_duration("7d"), Some(604_800.0));
        assert_eq!(parse_duration("250ms"), Some(0.25));
        assert_eq!(parse_duration("1.5h"), Some(5400.0));
    }

    #[test]
    fn durations_reject_junk() {
        assert_eq!(parse_duration("10"), None);
        assert_eq!(parse_duration("10y"), None);
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("-5m"), None);
    }

    #[test]
    fn duration_units_are_recognised() {
        assert!(is_duration_unit("ms"));
        assert!(is_duration_unit("d"));
        assert!(!is_duration_unit("w"));
    }

    #[test]
    fn epoch_zero_round_trips() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(format_epoch(0.0, 0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn parses_a_realistic_timestamp() {
        let t = parse_rfc3339("2026-07-27T09:14:15Z").unwrap();
        assert_eq!(format_epoch(t, 0), "2026-07-27T09:14:15Z");
    }

    #[test]
    fn parses_fractional_seconds() {
        let t = parse_rfc3339("2026-07-27T09:14:15.250Z").unwrap();
        assert!((t.fract() - 0.25).abs() < 1e-9);
        assert_eq!(format_epoch(t, 0), "2026-07-27T09:14:15.250Z");
    }

    #[test]
    fn parses_zone_offsets() {
        let z = parse_rfc3339("2026-07-27T09:14:15Z").unwrap();
        assert_eq!(parse_rfc3339("2026-07-27T11:14:15+02:00"), Some(z));
        assert_eq!(parse_rfc3339("2026-07-27T04:14:15-05:00"), Some(z));
        assert_eq!(parse_rfc3339("2026-07-27T11:14:15+0200"), Some(z));
    }

    #[test]
    fn accepts_a_space_separator_and_a_bare_date() {
        assert_eq!(
            parse_rfc3339("2026-07-27 09:14:15Z"),
            parse_rfc3339("2026-07-27T09:14:15Z")
        );
        assert_eq!(
            parse_rfc3339("2026-07-27"),
            parse_rfc3339("2026-07-27T00:00:00Z")
        );
    }

    #[test]
    fn missing_zone_is_read_as_utc() {
        assert_eq!(
            parse_rfc3339("2026-07-27T09:14:15"),
            parse_rfc3339("2026-07-27T09:14:15Z")
        );
    }

    #[test]
    fn rejects_text_that_is_not_a_timestamp() {
        for s in [
            "",
            "hello",
            "2026",
            "2026-13-01T00:00:00Z",
            "2026-07-27T25:00:00Z",
            "2026-07-27T09:14:15X",
            "2026/07/27T09:14:15Z",
            "500",
        ] {
            assert_eq!(parse_rfc3339(s), None, "{s} should not parse");
        }
    }

    #[test]
    fn formatting_applies_the_offset() {
        let t = parse_rfc3339("2026-07-27T09:14:15Z").unwrap();
        assert_eq!(format_epoch(t, 120), "2026-07-27T11:14:15+02:00");
        assert_eq!(format_epoch(t, -330), "2026-07-27T03:44:15-05:30");
    }

    #[test]
    fn tz_specs_parse() {
        assert_eq!(parse_tz("UTC"), Some(0));
        assert_eq!(parse_tz("z"), Some(0));
        assert_eq!(parse_tz("+02:00"), Some(120));
        assert_eq!(parse_tz("-0700"), Some(-420));
        assert_eq!(parse_tz("+05:30"), Some(330));
        assert_eq!(parse_tz("Mars/Olympus"), None);
        assert_eq!(parse_tz("+99:00"), None);
    }

    #[test]
    fn civil_dates_round_trip_across_leap_years() {
        for (y, m, d) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (2024, 2, 29),
            (2026, 12, 31),
            (2100, 3, 1),
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d), "{y}-{m}-{d}");
        }
    }

    #[test]
    fn now_is_after_the_project_was_written() {
        // 2026-01-01T00:00:00Z. A clock earlier than this means the test
        // machine is misconfigured, not that the function is wrong.
        assert!(now_epoch() > 1_767_225_600.0);
    }
}
