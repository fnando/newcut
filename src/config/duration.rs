//! Human durations (`"30d"`, `"12h"`, `"1h30m"`).

use std::fmt::Write as _;
use std::time::Duration;

/// Parse a duration such as `"30d"`, `"12h"`, `"10s"` or `"1h30m"`.
///
/// Units: `s`, `m`, `h`, `d`, `w`. At least one `<number><unit>` group is
/// required; groups may be concatenated.
///
/// # Errors
///
/// When `input` is empty, has a unit without a number before it, ends in a
/// number without a unit, uses an unknown unit, or adds up to more seconds
/// than fit in a `u64`.
pub fn parse(input: &str) -> Result<Duration, String> {
    let invalid =
        || format!("{input:?} is not a valid duration (e.g. \"30d\", \"12h\", \"1h30m\")");
    if input.is_empty() {
        return Err(invalid());
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    for ch in input.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        let unit = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return Err(invalid()),
        };
        let n: u64 = digits.parse().map_err(|_| invalid())?;
        digits.clear();
        total = n
            .checked_mul(unit)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(invalid)?;
    }
    if !digits.is_empty() {
        return Err(invalid());
    }
    Ok(Duration::from_secs(total))
}

/// A duration as the config writes it: `"6h"`, `"1d12h"`, `"0s"`.
#[must_use]
pub fn format(duration: Duration) -> String {
    let mut secs = duration.as_secs();
    if secs == 0 {
        return "0s".into();
    }
    let mut out = String::new();
    for (unit, size) in [
        ("w", 604_800),
        ("d", 86_400),
        ("h", 3600),
        ("m", 60),
        ("s", 1),
    ] {
        if secs >= size {
            let _ = write!(out, "{}{unit}", secs / size);
            secs %= size;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_units() {
        assert_eq!(parse("10s"), Ok(Duration::from_secs(10)));
        assert_eq!(parse("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse("12h"), Ok(Duration::from_secs(12 * 3600)));
        assert_eq!(parse("30d"), Ok(Duration::from_secs(30 * 86400)));
        assert_eq!(parse("2w"), Ok(Duration::from_secs(14 * 86400)));
    }

    #[test]
    fn parses_compound_durations() {
        assert_eq!(parse("1h30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse("1d12h"), Ok(Duration::from_secs(129_600)));
    }

    #[test]
    fn rejects_invalid_durations() {
        for bad in ["", "30", "d", "30x", "1.5h", "-1h", "30 d", "h30"] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format(Duration::from_secs(0)), "0s");
        assert_eq!(format(Duration::from_secs(5400)), "1h30m");
        assert_eq!(format(Duration::from_secs(7 * 86400)), "1w");
        assert_eq!(format(Duration::from_secs(129_600)), "1d12h");
    }
}
