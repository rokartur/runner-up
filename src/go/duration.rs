//! Go's time.ParseDuration and Duration.String, for setup values and output
//! that must read the same as the Go program wrote them.

use std::time::Duration;

const UNITS: [(&str, u128); 8] = [
    ("ns", 1),
    ("us", 1_000),
    ("µs", 1_000),
    ("μs", 1_000),
    ("ms", 1_000_000),
    ("s", 1_000_000_000),
    ("m", 60_000_000_000),
    ("h", 3_600_000_000_000),
];

/// Parses a Go duration such as "1h30m" or "-1.5s" into signed nanoseconds.
pub fn parse_nanos(orig: &str) -> Result<i64, String> {
    let bad = || format!("time: invalid duration \"{orig}\"");
    let mut s = orig;
    let mut neg = false;
    if let Some(rest) = s.strip_prefix('-') {
        neg = true;
        s = rest;
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest;
    }
    if s == "0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(bad());
    }
    let mut total: f64 = 0.0;
    while !s.is_empty() {
        let num_end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
        let num = &s[..num_end];
        if num.is_empty() || num == "." {
            return Err(bad());
        }
        let value: f64 = num.parse().map_err(|_| bad())?;
        s = &s[num_end..];
        let unit_end = s.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(s.len());
        let unit = &s[..unit_end];
        if unit.is_empty() {
            return Err(format!("time: missing unit in duration \"{orig}\""));
        }
        let Some((_, scale)) = UNITS.iter().find(|(u, _)| *u == unit) else {
            return Err(format!("time: unknown unit \"{unit}\" in duration \"{orig}\""));
        };
        total += value * *scale as f64;
        s = &s[unit_end..];
    }
    if total > i64::MAX as f64 {
        return Err(bad());
    }
    let n = total.round() as i64;
    Ok(if neg { -n } else { n })
}

/// Parses a non-negative Go duration.
pub fn parse(s: &str) -> Result<Duration, String> {
    let n = parse_nanos(s)?;
    if n < 0 {
        return Err(format!("time: invalid duration \"{s}\""));
    }
    Ok(Duration::from_nanos(n as u64))
}

/// Formats signed nanoseconds as Go's Duration.String does: "1h2m3.5s", "1.5ms", "0s".
pub fn format_nanos(d: i64) -> String {
    if d == 0 {
        return "0s".into();
    }
    let neg = d < 0;
    let u = d.unsigned_abs();
    let sign = if neg { "-" } else { "" };
    if u < 1_000_000_000 {
        let (unit, scale) = if u < 1_000 {
            ("ns", 1)
        } else if u < 1_000_000 {
            ("µs", 1_000)
        } else {
            ("ms", 1_000_000)
        };
        return format!("{sign}{}{unit}", frac(u, scale));
    }
    let h = u / 3_600_000_000_000;
    let m = u / 60_000_000_000 % 60;
    let rest = u % 60_000_000_000;
    let secs = frac(rest, 1_000_000_000);
    let mut out = String::from(sign);
    if h > 0 {
        out += &format!("{h}h{m}m");
    } else if m > 0 {
        out += &format!("{m}m");
    }
    out + &secs + "s"
}

pub fn format(d: Duration) -> String {
    format_nanos(d.as_nanos().min(i64::MAX as u128) as i64)
}

/// Writes n/scale with the fraction's trailing zeros dropped.
fn frac(n: u64, scale: u64) -> String {
    let whole = n / scale;
    let mut f = n % scale;
    if f == 0 {
        return whole.to_string();
    }
    let mut digits = scale.ilog10() as usize;
    while f.is_multiple_of(10) {
        f /= 10;
        digits -= 1;
    }
    format!("{whole}.{f:0digits$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_like_go() {
        assert_eq!(parse_nanos("60m").unwrap(), 3_600_000_000_000);
        assert_eq!(parse_nanos("1h30m").unwrap(), 5_400_000_000_000);
        assert_eq!(parse_nanos("1.5s").unwrap(), 1_500_000_000);
        assert_eq!(parse_nanos("-2ms").unwrap(), -2_000_000);
        assert!(parse_nanos("10").is_err());
        assert!(parse_nanos("").is_err());
        assert!(parse_nanos("5x").is_err());
        assert_eq!(format_nanos(3_600_000_000_000), "1h0m0s");
        assert_eq!(format_nanos(90_500_000_000), "1m30.5s");
        assert_eq!(format_nanos(1_500_000), "1.5ms");
        assert_eq!(format_nanos(0), "0s");
        assert_eq!(format_nanos(-1_000_000_000), "-1s");
        assert_eq!(format(Duration::from_secs(600)), "10m0s");
    }
}
