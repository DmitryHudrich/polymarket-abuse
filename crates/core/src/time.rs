//! Calendar arithmetic without a date crate — Howard Hinnant's civil-date
//! algorithms plus the two conversions both binaries need: Gamma's ISO 8601
//! timestamps in, and ClickHouse's `DateTime64` literals out.

/// Civil `(year, month, day)` from days since the epoch — `civil_from_days`.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Days since the epoch for a civil date — the inverse of [`civil_from_days`].
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Wall-clock `(year, month, day, hour, minute, second)` at a UTC offset given
/// in minutes east of UTC.
pub fn wall_clock(ts: u64, offset_minutes: i64) -> (i64, u32, u32, u32, u32, u32) {
    let local = ts as i64 + offset_minutes * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let secs = local.rem_euclid(86_400);
    (
        y,
        m,
        d,
        (secs / 3_600) as u32,
        (secs % 3_600 / 60) as u32,
        (secs % 60) as u32,
    )
}

/// Unix seconds from an ISO 8601 timestamp as Gamma sends it
/// (`2026-11-03T12:00:00Z`, sometimes date-only or with a `±HH:MM` zone).
/// Fractional seconds are ignored; anything unparseable yields `None`.
pub fn parse_iso8601(s: &str) -> Option<u64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }

    let mut secs = days_from_civil(y, m, d) * 86_400;
    if b.len() >= 16 && (b[10] == b'T' || b[10] == b' ') {
        secs += s.get(11..13)?.parse::<i64>().ok()? * 3_600;
        secs += s.get(14..16)?.parse::<i64>().ok()? * 60;
        if b.len() >= 19 && b[16] == b':' {
            secs += s.get(17..19)?.parse::<i64>().unwrap_or(0);
        }
        // A trailing zone offset, if any, is what the wall clock was shifted by.
        if let Some(i) = s.rfind(['+', '-']).filter(|i| *i >= 11) {
            let z = &s[i + 1..];
            let zh: i64 = z.get(0..2).and_then(|v| v.parse().ok()).unwrap_or(0);
            let zm: i64 = z.get(3..5).and_then(|v| v.parse().ok()).unwrap_or(0);
            let sign = if b[i] == b'-' { 1 } else { -1 };
            secs += sign * (zh * 3_600 + zm * 60);
        }
    }
    u64::try_from(secs).ok()
}

/// `2026-07-25 16:23:41.123` in UTC — the unambiguous literal ClickHouse parses
/// into a `DateTime64(3)` under its default `date_time_input_format`. Sending
/// bare numbers instead would be read as raw ticks, not seconds.
pub fn clickhouse_datetime(ms: u64) -> String {
    let (y, mo, d, h, mi, s) = wall_clock(ms / 1_000, 0);
    format!(
        "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{:03}",
        ms % 1_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_timestamps() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-07-25T16:23:41Z = 1784996621
        assert_eq!(wall_clock(1_784_996_621, 0), (2026, 7, 25, 16, 23, 41));
        // Leap day, and the offset rolling the date backwards over midnight.
        assert_eq!(wall_clock(1_709_164_800, 0), (2024, 2, 29, 0, 0, 0));
        assert_eq!(wall_clock(1_709_164_800, -180), (2024, 2, 28, 21, 0, 0));
        // Fractional offset (UTC+5:30).
        assert_eq!(wall_clock(1_784_996_621, 330), (2026, 7, 25, 21, 53, 41));
    }

    #[test]
    fn iso_timestamps_round_trip_through_civil_days() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2026-07-25T16:23:41Z"), Some(1_784_996_621));
        // Date-only, fractional seconds, and a non-UTC zone that shifts the instant.
        assert_eq!(parse_iso8601("2024-02-29"), Some(1_709_164_800));
        assert_eq!(
            parse_iso8601("2026-07-25T16:23:41.123456Z"),
            Some(1_784_996_621)
        );
        assert_eq!(
            parse_iso8601("2026-07-25T19:23:41+03:00"),
            Some(1_784_996_621)
        );
        assert_eq!(
            parse_iso8601("2026-07-25T13:23:41-03:00"),
            Some(1_784_996_621)
        );
        // Every day of a leap year survives the round trip.
        for day in 0..366 {
            let ts = 1_704_067_200 + day * 86_400; // from 2024-01-01
            let (y, m, d, ..) = wall_clock(ts, 0);
            assert_eq!(days_from_civil(y, m, d) * 86_400, ts as i64);
        }
    }

    #[test]
    fn clickhouse_literals_keep_millisecond_precision() {
        assert_eq!(clickhouse_datetime(0), "1970-01-01 00:00:00.000");
        assert_eq!(
            clickhouse_datetime(1_784_996_621_007),
            "2026-07-25 16:23:41.007"
        );
        // Round trip through the ISO parser (which drops the fraction).
        let s = clickhouse_datetime(1_784_996_621_000);
        assert_eq!(parse_iso8601(&s), Some(1_784_996_621));
    }
}
