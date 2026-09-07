//! Hand-rolled UUID and ISO-8601 codecs.
//!
//! These run 3M+ times during ingest, so they avoid the general-purpose
//! parsers: both formats are fixed-width here and validate cheaply.

/// Parse a canonical `8-4-4-4-12` UUID into a `u128`. Also accepts the
/// unhyphenated 32-char form.
pub fn parse_uuid(s: &str) -> Option<u128> {
    let b = s.as_bytes();
    let mut out: u128 = 0;
    let mut nibbles = 0u32;
    match b.len() {
        36 => {
            if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
                return None;
            }
        }
        32 => {}
        _ => return None,
    }
    for &c in b {
        if c == b'-' {
            continue;
        }
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        out = (out << 4) | v as u128;
        nibbles += 1;
    }
    if nibbles == 32 {
        Some(out)
    } else {
        None
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Render a `u128` back to canonical hyphenated form.
pub fn format_uuid(v: u128) -> String {
    let mut out = [0u8; 36];
    let mut i = 0;
    for pos in 0..32 {
        if pos == 8 || pos == 12 || pos == 16 || pos == 20 {
            out[i] = b'-';
            i += 1;
        }
        let shift = (31 - pos) * 4;
        out[i] = HEX[((v >> shift) & 0xf) as usize];
        i += 1;
    }
    // SAFETY: every byte written is ASCII hex or '-'.
    unsafe { String::from_utf8_unchecked(out.to_vec()) }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    // Howard Hinnant's civil-date algorithm.
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn digits(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
    }
    let mut v: i64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as i64;
    }
    Some(v)
}

/// Parse `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS[.mmm][Z]` to Unix seconds (UTC).
///
/// Trailing numeric offsets (`+10:00`) are honoured; anything else after the
/// seconds field is ignored.
pub fn parse_datetime(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = digits(&b[0..4])?;
    let mo = digits(&b[5..7])?;
    let d = digits(&b[8..10])?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, mo, d) * 86_400;
    if b.len() >= 19 && (b[10] == b'T' || b[10] == b' ') {
        let h = digits(&b[11..13])?;
        let mi = digits(&b[14..16])?;
        let se = digits(&b[17..19])?;
        secs += h * 3600 + mi * 60 + se;
        // Optional numeric UTC offset.
        let rest = &b[19..];
        if let Some(p) = rest.iter().position(|&c| c == b'+' || c == b'-') {
            let off = &rest[p..];
            if off.len() >= 6 && off[3] == b':' {
                if let (Some(oh), Some(om)) = (digits(&off[1..3]), digits(&off[4..6])) {
                    let delta = oh * 3600 + om * 60;
                    secs += if off[0] == b'+' { -delta } else { delta };
                }
            }
        }
    }
    Some(secs)
}

/// Render Unix seconds as `YYYY-MM-DD`.
pub fn format_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Render Unix seconds as `YYYY-MM-DD HH:MM`.
pub fn format_datetime(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Compact "3d ago" style age, for result listings.
pub fn format_age(secs: i64, now: i64) -> String {
    let d = (now - secs).max(0);
    if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else if d < 86_400 * 60 {
        format!("{}d ago", d / 86_400)
    } else if d < 86_400 * 730 {
        format!("{}mo ago", d / (86_400 * 30))
    } else {
        format!("{}y ago", d / (86_400 * 365))
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_roundtrip() {
        let s = "dd81ef00-5605-11ef-a26a-692e722be878";
        let v = parse_uuid(s).expect("parse");
        assert_eq!(format_uuid(v), s);
        // Unhyphenated form parses to the same value.
        assert_eq!(parse_uuid(&s.replace('-', "")), Some(v));
        assert_eq!(parse_uuid("not-a-uuid"), None);
        assert_eq!(parse_uuid(""), None);
    }

    #[test]
    fn datetime_parsing() {
        assert_eq!(parse_datetime("1970-01-01"), Some(0));
        assert_eq!(parse_datetime("1970-01-02T00:00:00Z"), Some(86_400));
        // Known reference point: 2026-07-01T04:46:06Z
        let t = parse_datetime("2026-07-01T04:46:06.376Z").expect("parse");
        assert_eq!(format_datetime(t), "2026-07-01 04:46");
        assert_eq!(format_date(t), "2026-07-01");
        assert_eq!(parse_datetime("garbage"), None);
    }

    #[test]
    fn offsets_are_normalised_to_utc() {
        let a = parse_datetime("2026-07-01T10:00:00+10:00").expect("parse");
        let b = parse_datetime("2026-07-01T00:00:00Z").expect("parse");
        assert_eq!(a, b);
    }

    #[test]
    fn civil_roundtrip_over_a_long_span() {
        for day in (-20000..20000).step_by(37) {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day);
        }
    }
}
