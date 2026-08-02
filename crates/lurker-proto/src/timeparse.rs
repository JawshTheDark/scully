// RFC 3339 → unix seconds, dependency-free.
//
// Exists because the render path was parsing ~500 of these per redraw —
// per incoming message — through glib::DateTime, which drags locale and
// timezone machinery into what is, on this wire, always a fixed-format
// ASCII string. A hand parser is ~100ns and pure, so it can live here and
// serve the store (which has no glib) as well as the UI.

/// Parse `YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH:MM|±HHMM]` to unix seconds.
/// Fractional seconds are truncated. Returns `None` on anything malformed —
/// callers treat that as "no timestamp", never as an error.
pub fn rfc3339_to_unix(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: core::ops::Range<usize>| -> Option<i64> {
        let mut v: i64 = 0;
        for &c in &b[r] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    };
    if b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't' && b[10] != b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    // Skip fractional seconds.
    let mut i = 19;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
    }

    // Offset. Absent means UTC — lenient on purpose: some emitters drop the Z.
    let offset_secs: i64 = if i >= b.len() {
        0
    } else {
        match b[i] {
            b'Z' | b'z' if i + 1 == b.len() => 0,
            sign @ (b'+' | b'-') => {
                let rest = &s[i + 1..];
                let (oh, om) = match rest.len() {
                    5 if rest.as_bytes()[2] == b':' => {
                        (num(i + 1..i + 3)?, num(i + 4..i + 6)?)
                    }
                    4 => (num(i + 1..i + 3)?, num(i + 3..i + 5)?),
                    2 => (num(i + 1..i + 3)?, 0),
                    _ => return None,
                };
                let total = oh * 3600 + om * 60;
                if sign == b'-' { -total } else { total }
            }
            _ => return None,
        }
    };

    // Howard Hinnant's days-from-civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    Some(days * 86400 + hour * 3600 + minute * 60 + second.min(59) - offset_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_wire_shapes() {
        // Cross-checked against `date -u -d ... +%s`.
        assert_eq!(rfc3339_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_to_unix("2026-07-28T00:00:00Z"), Some(1785196800));
        assert_eq!(rfc3339_to_unix("2026-07-28T00:00:00.123Z"), Some(1785196800));
        assert_eq!(rfc3339_to_unix("2026-07-28T02:00:00+02:00"), Some(1785196800));
        assert_eq!(rfc3339_to_unix("2026-07-27T22:00:00-02:00"), Some(1785196800));
        assert_eq!(rfc3339_to_unix("2026-07-28T02:00:00+0200"), Some(1785196800));
        // No offset: treated as UTC.
        assert_eq!(rfc3339_to_unix("2026-07-28T00:00:00"), Some(1785196800));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in ["", "not a date", "2026-07-28", "2026-07-28T00:00", "2026-13-01T00:00:00Z",
                    "2026-07-32T00:00:00Z", "2026-07-28T24:00:00Z", "2026-07-28T00:00:00+2"] {
            assert_eq!(rfc3339_to_unix(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn leap_second_clamps_rather_than_rejects() {
        assert_eq!(
            rfc3339_to_unix("2016-12-31T23:59:60Z"),
            rfc3339_to_unix("2016-12-31T23:59:59Z")
        );
    }

    #[test]
    fn pre_epoch_dates_work() {
        assert_eq!(rfc3339_to_unix("1969-12-31T23:59:59Z"), Some(-1));
    }
}
