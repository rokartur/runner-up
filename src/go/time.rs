//! Go's time.Time as runner-up stores it: a zero value that means "never",
//! and RFC 3339 with nanoseconds in JSON.

use chrono::{DateTime, NaiveDate, SecondsFormat, TimeDelta, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time(pub DateTime<Utc>);

fn zero_utc() -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(1, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc()
}

impl Default for Time {
    fn default() -> Self {
        Time(zero_utc())
    }
}

impl Time {
    pub fn now() -> Self {
        Time(Utc::now())
    }
    pub fn is_zero(&self) -> bool {
        self.0 == zero_utc()
    }
    /// self - earlier, as Go's Time.Sub; negative when earlier is later.
    pub fn sub(&self, earlier: Time) -> TimeDelta {
        self.0 - earlier.0
    }
    /// How long since self, never negative.
    pub fn since(&self, now: Time) -> Duration {
        now.sub(*self).to_std().unwrap_or_default()
    }
    pub fn add(&self, d: Duration) -> Time {
        Time(self.0 + TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX))
    }
    pub fn sub_dur(&self, d: Duration) -> Time {
        Time(self.0 - TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX))
    }
    pub fn before(&self, other: Time) -> bool {
        self.0 < other.0
    }
    pub fn after(&self, other: Time) -> bool {
        self.0 > other.0
    }
    pub fn from_unix(secs: i64) -> Time {
        Time(DateTime::from_timestamp(secs, 0).unwrap_or_else(zero_utc))
    }
    pub fn unix(&self) -> i64 {
        self.0.timestamp()
    }
    /// RFC 3339 with trailing zeros of the fraction dropped, as Go's RFC3339Nano.
    pub fn rfc3339(&self) -> String {
        let s = self.0.to_rfc3339_opts(SecondsFormat::Nanos, true);
        match s.split_once('.') {
            Some((head, frac)) => {
                let digits = frac.trim_end_matches('Z').trim_end_matches('0');
                if digits.is_empty() { format!("{head}Z") } else { format!("{head}.{digits}Z") }
            }
            None => s,
        }
    }
    /// As Go's Format(time.RFC3339): whole seconds.
    pub fn rfc3339_secs(&self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::Secs, true)
    }
    /// As Go's Local().Format(time.RFC3339).
    pub fn local_rfc3339(&self) -> String {
        self.0.with_timezone(&chrono::Local).to_rfc3339_opts(SecondsFormat::Secs, true)
    }
    pub fn unix_nanos(&self) -> i64 {
        self.0.timestamp_nanos_opt().unwrap_or(i64::MAX)
    }
    pub fn from_system(t: std::time::SystemTime) -> Time {
        Time(t.into())
    }
    pub fn parse(s: &str) -> Result<Time, chrono::ParseError> {
        Ok(Time(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc)))
    }
}

impl std::fmt::Display for Time {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.rfc3339())
    }
}

impl Serialize for Time {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.rfc3339())
    }
}

impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Time::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// d as a chrono span, to compare with Time::sub.
pub fn td(d: Duration) -> TimeDelta {
    TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX)
}

/// d rounded to the second and written as Go's Duration.String, as
/// d.Round(time.Second).String().
pub fn round_secs(d: TimeDelta) -> String {
    let n = d.num_nanoseconds().unwrap_or(if d < TimeDelta::zero() { i64::MIN } else { i64::MAX });
    crate::go::duration::format_nanos(round_nanos(n, 1_000_000_000))
}

/// Go's Duration.Round: halfway values round away from zero, and a result
/// past the range saturates, as Time::sub's spans from the zero time do.
fn round_nanos(d: i64, m: i64) -> i64 {
    let r = (d % m).abs();
    if d < 0 {
        if r + r < m {
            return d + r;
        }
        return d.checked_sub(m - r).unwrap_or(i64::MIN);
    }
    if r + r < m {
        return d - r;
    }
    d.checked_add(m - r).unwrap_or(i64::MAX)
}

/// For `#[serde(skip_serializing_if)]` on optional times.
pub fn is_zero(t: &Time) -> bool {
    t.is_zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    // As Go, (time.Now().Sub(time.Time{})).Round(time.Second) saturates to
    // its largest duration rather than overflowing.
    #[test]
    fn round_secs_saturates_like_go() {
        assert_eq!(round_secs(Time::now().sub(Time::default())), "2562047h47m16.854775807s");
        assert_eq!(round_secs(TimeDelta::milliseconds(-1500)), "-2s");
        assert_eq!(round_secs(TimeDelta::milliseconds(1499)), "1s");
    }

    #[test]
    fn round_trips_like_go() {
        let z = Time::default();
        assert!(z.is_zero());
        assert_eq!(serde_json::to_string(&z).unwrap(), "\"0001-01-01T00:00:00Z\"");
        let t: Time = serde_json::from_str("\"2024-05-01T10:00:00.5+02:00\"").unwrap();
        assert_eq!(t.rfc3339(), "2024-05-01T08:00:00.5Z");
    }
}
