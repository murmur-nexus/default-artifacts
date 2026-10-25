//! Conversion between git's `<unix-seconds> <+HHMM>` identity-line timestamp and the ISO 8601
//! string GitHub's Git Data API takes for `author.date` / `committer.date`.
//!
//! GitHub reconstructs the identity line from `name`, `email` and `date` when it recomputes
//! the commit's SHA1, using the offset carried in the ISO string. Byte-reproducing the local
//! commit therefore requires carrying the exact offset through, not normalizing to UTC — `+0000`
//! and `-0000` are different bytes in the object git already wrote.

/// A parsed git identity line: `name <email> seconds offset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
    pub seconds: i64,
    /// Offset in minutes, e.g. `+0200` is `120`, `-0530` is `-330`.
    pub offset_minutes: i32,
    /// Whether the source wrote the sign as `-` on a zero offset (`-0000`), which is distinct
    /// from `+0000` in git's object format even though both mean UTC.
    pub negative_zero: bool,
}

/// Parse `"Name <email> 1690000000 +0200"` (the tail of a git `author`/`committer` header,
/// after the field name is stripped). Trailing whitespace on `name` before `<` is trimmed.
pub fn parse_identity_line(line: &str) -> Result<Identity, String> {
    let open = line
        .find('<')
        .ok_or_else(|| format!("identity line has no '<': {line:?}"))?;
    let close = line[open..]
        .find('>')
        .map(|i| open + i)
        .ok_or_else(|| format!("identity line has no '>': {line:?}"))?;
    let name = line[..open].trim_end().to_string();
    let email = line[open + 1..close].to_string();
    let rest = line[close + 1..].trim();
    let mut parts = rest.split_whitespace();
    let seconds: i64 = parts
        .next()
        .ok_or_else(|| format!("identity line has no timestamp: {line:?}"))?
        .parse()
        .map_err(|_| format!("identity line timestamp is not an integer: {line:?}"))?;
    let offset_raw = parts
        .next()
        .ok_or_else(|| format!("identity line has no timezone offset: {line:?}"))?;
    let (sign, digits) = offset_raw
        .split_at_checked(1)
        .filter(|(sign, _)| *sign == "+" || *sign == "-")
        .ok_or_else(|| format!("identity line offset must start with +/-: {line:?}"))?;
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("identity line offset must be 4 digits: {line:?}"));
    }
    let hours: i32 = digits[..2].parse().unwrap();
    let minutes: i32 = digits[2..].parse().unwrap();
    let magnitude = hours * 60 + minutes;
    let negative_zero = sign == "-" && magnitude == 0;
    let offset_minutes = if sign == "-" { -magnitude } else { magnitude };
    Ok(Identity {
        name,
        email,
        seconds,
        offset_minutes,
        negative_zero,
    })
}

/// Render an [`Identity`]'s timestamp as ISO 8601 with the identity's own offset, for the Git
/// Data API's `date` field: `2023-07-22T10:26:40+02:00`, or `...-00:00` for a `negative_zero`
/// identity so the round trip through GitHub reconstructs the same offset sign.
pub fn to_iso8601(identity: &Identity) -> String {
    let local_seconds = identity.seconds + i64::from(identity.offset_minutes) * 60;
    let days = local_seconds.div_euclid(86_400);
    let secs_of_day = local_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let sign = if identity.offset_minutes < 0 || identity.negative_zero {
        '-'
    } else {
        '+'
    };
    let magnitude = identity.offset_minutes.unsigned_abs();
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{sign}{:02}:{:02}",
        magnitude / 60,
        magnitude % 60
    )
}

/// Render an [`Identity`] as the tail of a git `author`/`committer` header, the inverse of
/// [`parse_identity_line`]: `Name <email> 1690000000 +0200`.
pub fn to_identity_line(identity: &Identity) -> String {
    let sign = if identity.offset_minutes < 0 || identity.negative_zero {
        '-'
    } else {
        '+'
    };
    let magnitude = identity.offset_minutes.unsigned_abs();
    format!(
        "{} <{}> {} {sign}{:02}{:02}",
        identity.name,
        identity.email,
        identity.seconds,
        magnitude / 60,
        magnitude % 60
    )
}

/// Parse an ISO 8601 date (as GitHub returns it: GraphQL's `GitTimestamp` keeps the offset
/// the commit was written with) back into an [`Identity`]'s numeric fields. Accepts `Z` or
/// `±HH:MM`.
pub fn from_iso8601(text: &str) -> Result<(i64, i32, bool), String> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return Err(format!("too short to be ISO 8601: {text:?}"));
    }
    let year: i64 = text[0..4].parse().map_err(|_| "bad year")?;
    let month: i64 = text[5..7].parse().map_err(|_| "bad month")?;
    let day: i64 = text[8..10].parse().map_err(|_| "bad day")?;
    let hour: i64 = text[11..13].parse().map_err(|_| "bad hour")?;
    let minute: i64 = text[14..16].parse().map_err(|_| "bad minute")?;
    let second: i64 = text[17..19].parse().map_err(|_| "bad second")?;
    let tail = &text[19..];
    let (offset_minutes, negative_zero) = if tail == "Z" || tail.is_empty() {
        (0, false)
    } else {
        let sign = tail.chars().next().ok_or("missing offset sign")?;
        let digits: String = tail[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() < 4 {
            return Err(format!("bad offset: {tail:?}"));
        }
        let hh: i32 = digits[0..2].parse().map_err(|_| "bad offset hours")?;
        let mm: i32 = digits[2..4].parse().map_err(|_| "bad offset minutes")?;
        let magnitude = hh * 60 + mm;
        let neg_zero = sign == '-' && magnitude == 0;
        (if sign == '-' { -magnitude } else { magnitude }, neg_zero)
    };
    let days = days_from_civil(year, month, day);
    let local_seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    let utc_seconds = local_seconds - i64::from(offset_minutes) * 60;
    Ok((utc_seconds, offset_minutes, negative_zero))
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a proleptic-Gregorian
/// `(year, month, day)`. `days` may be negative.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// The inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_typical_identity_line() {
        let id = parse_identity_line("Ada Lovelace <ada@example.com> 1690000000 +0200").unwrap();
        assert_eq!(id.name, "Ada Lovelace");
        assert_eq!(id.email, "ada@example.com");
        assert_eq!(id.seconds, 1_690_000_000);
        assert_eq!(id.offset_minutes, 120);
        assert!(!id.negative_zero);
    }

    #[test]
    fn parses_a_negative_offset_and_negative_zero() {
        let id = parse_identity_line("A <a@b.com> 1000 -0530").unwrap();
        assert_eq!(id.offset_minutes, -330);
        let id = parse_identity_line("A <a@b.com> 1000 -0000").unwrap();
        assert_eq!(id.offset_minutes, 0);
        assert!(id.negative_zero);
    }

    #[test]
    fn iso8601_round_trips_through_the_offset() {
        let id = parse_identity_line("Ada <ada@example.com> 1690000000 +0200").unwrap();
        let iso = to_iso8601(&id);
        assert_eq!(iso, "2023-07-22T06:26:40+02:00");
        let (seconds, offset, neg_zero) = from_iso8601(&iso).unwrap();
        assert_eq!(seconds, id.seconds);
        assert_eq!(offset, id.offset_minutes);
        assert!(!neg_zero);
    }

    #[test]
    fn iso8601_preserves_negative_zero() {
        let id = parse_identity_line("Ada <ada@example.com> 1690000000 -0000").unwrap();
        let iso = to_iso8601(&id);
        assert!(iso.ends_with("-00:00"), "{iso}");
    }

    #[test]
    fn identity_line_round_trips_through_iso8601() {
        for line in [
            "Ada <ada@example.com> 1690000000 +0200",
            "A <a@b.com> 1792158258 -0500",
            "A <a@b.com> 1000 -0000",
        ] {
            let id = parse_identity_line(line).unwrap();
            let (seconds, offset_minutes, negative_zero) = from_iso8601(&to_iso8601(&id)).unwrap();
            let rebuilt = Identity {
                name: id.name.clone(),
                email: id.email.clone(),
                seconds,
                offset_minutes,
                negative_zero,
            };
            assert_eq!(to_identity_line(&rebuilt), line);
        }
    }

    #[test]
    fn epoch_is_1970_01_01() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }
}
