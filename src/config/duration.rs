//! Durations, with a YAML shape that accepts what people actually write.
//!
//! `idle_timeout: 30s`, `idle_timeout: 10m`, `idle_timeout: 1h30m`, and a bare
//! `idle_timeout: 30` (seconds) all work, because both spellings appear in the
//! wild. Signed on purpose: zero *and* negative mean "never" for `idle_timeout`.

use std::fmt;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

/// Nanoseconds in one second; the whole module counts in nanoseconds so
/// fractional seconds are exact.
pub const NANOS_PER_SEC: i64 = 1_000_000_000;

/// A time span that also accepts a bare number of seconds, so both
/// `idle_timeout: 30s` and `idle_timeout: 30` work.
///
/// Signed on purpose: a negative or zero idle timeout means "never stop".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Duration {
    nanos: i64,
}

impl Duration {
    #[allow(dead_code)] // handy constant, used by tests and callers
    pub const ZERO: Duration = Duration::from_nanos(0);

    pub const fn from_nanos(nanos: i64) -> Self {
        Duration { nanos }
    }

    pub const fn from_secs(secs: i64) -> Self {
        Duration {
            nanos: secs * NANOS_PER_SEC,
        }
    }

    pub const fn from_millis(millis: i64) -> Self {
        Duration {
            nanos: millis * 1_000_000,
        }
    }

    pub const fn nanos(self) -> i64 {
        self.nanos
    }

    pub const fn millis(self) -> i64 {
        self.nanos / 1_000_000
    }

    pub const fn is_positive(self) -> bool {
        self.nanos > 0
    }

    /// Clamped to zero, which is what the syscalls want.
    pub fn as_std(self) -> StdDuration {
        StdDuration::from_nanos(self.nanos.max(0) as u64)
    }
}

impl From<StdDuration> for Duration {
    fn from(value: StdDuration) -> Self {
        Duration::from_nanos(value.as_nanos().min(i64::MAX as u128) as i64)
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut nanos = self.nanos();
        if nanos == 0 {
            return f.write_str("0s");
        }
        if nanos < 0 {
            f.write_str("-")?;
            nanos = -nanos;
        }

        if nanos < 1_000 {
            return write!(f, "{nanos}ns");
        }
        if nanos < 1_000_000 {
            return write!(f, "{}µs", decimals(nanos as f64 / 1e3));
        }
        if nanos < NANOS_PER_SEC {
            return write!(f, "{}ms", decimals(nanos as f64 / 1e6));
        }

        let mut secs = nanos / NANOS_PER_SEC;
        let fraction = nanos % NANOS_PER_SEC;
        let hours = secs / 3600;
        secs %= 3600;
        let minutes = secs / 60;
        let seconds = secs % 60;

        if hours > 0 {
            write!(f, "{hours}h{minutes}m")?;
        } else if minutes > 0 {
            write!(f, "{minutes}m")?;
        }
        if fraction == 0 {
            write!(f, "{seconds}s")
        } else {
            write!(f, "{}s", decimals(seconds as f64 + fraction as f64 / 1e9))
        }
    }
}

/// Three decimals with trailing zeros trimmed: `1.5`, `250`, `0.001`.
fn decimals(value: f64) -> String {
    let text = format!("{value:.3}");
    match text.split_once('.') {
        Some((whole, fraction)) => {
            let fraction = fraction.trim_end_matches('0');
            if fraction.is_empty() {
                whole.to_string()
            } else {
                format!("{whole}.{fraction}")
            }
        }
        None => text,
    }
}

/// Parses either a bare integer (seconds) or a duration with units, which is
/// what YAML values like `30s`, `1h30m`, `250ms` and `1.5s` are.
pub fn parse_duration(text: &str) -> Result<Duration> {
    if let Ok(seconds) = text.parse::<i64>() {
        return Ok(Duration::from_secs(seconds));
    }
    parse_units(text).with_context(|| format!("invalid duration {text:?}"))
}

fn parse_units(text: &str) -> Result<Duration> {
    let mut rest = text;
    let mut negative = false;
    if let Some(stripped) = rest.strip_prefix('-') {
        negative = true;
        rest = stripped;
    } else if let Some(stripped) = rest.strip_prefix('+') {
        rest = stripped;
    }
    if rest.is_empty() {
        bail!("empty duration");
    }

    let mut total: i64 = 0;
    while !rest.is_empty() {
        let number_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let number_text = &rest[..number_end];
        if number_text.is_empty() {
            bail!("missing number in {text:?}");
        }
        let value: f64 = number_text
            .parse()
            .with_context(|| format!("invalid number {number_text:?}"))?;
        rest = &rest[number_end..];

        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.' || c == '-')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        if unit.is_empty() {
            bail!("missing unit in {text:?}");
        }
        rest = &rest[unit_end..];

        let scale = match unit {
            "ns" => 1.0,
            "us" | "µs" | "μs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            _ => bail!("unknown unit {unit:?} in {text:?}"),
        };
        let piece = (value * scale) as i64;
        total = total
            .checked_add(piece)
            .with_context(|| format!("duration {text:?} overflows"))?;
    }

    Ok(Duration::from_nanos(if negative { -total } else { total }))
}

struct DurationVisitor;

impl<'de> Visitor<'de> for DurationVisitor {
    type Value = Duration;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a duration like 30s, 1h30m or a number of seconds")
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Duration, E> {
        let nanos = value
            .checked_mul(NANOS_PER_SEC)
            .ok_or_else(|| E::custom("duration overflows"))?;
        Ok(Duration::from_nanos(nanos))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Duration, E> {
        let nanos = (value as i128)
            .checked_mul(NANOS_PER_SEC as i128)
            .filter(|nanos| *nanos <= i64::MAX as i128)
            .ok_or_else(|| E::custom("duration overflows"))?;
        Ok(Duration::from_nanos(nanos as i64))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Duration, E> {
        parse_duration(value).map_err(E::custom)
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, _seq: A) -> Result<Duration, A::Error> {
        Err(de::Error::custom("duration must be a scalar"))
    }

    fn visit_map<A: de::MapAccess<'de>>(self, _map: A) -> Result<Duration, A::Error> {
        Err(de::Error::custom("duration must be a scalar"))
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(DurationVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_people_write() {
        let cases = [
            ("30s", 30 * NANOS_PER_SEC),
            ("1h30m", 90 * 60 * NANOS_PER_SEC),
            ("0", 0),
            ("120", 120 * NANOS_PER_SEC),
            ("-5", -5 * NANOS_PER_SEC),
            ("1.5s", 1_500_000_000),
            ("250ms", 250_000_000),
            ("100us", 100_000),
        ];
        for (input, nanos) in cases {
            let got = parse_duration(input).unwrap_or_else(|err| panic!("{input}: {err:#}"));
            assert_eq!(got.nanos(), nanos, "input {input}");
        }
        for bad in ["3 weeks", "", "5x"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn renders_readably() {
        assert_eq!(Duration::from_secs(30).to_string(), "30s");
        assert_eq!(Duration::from_secs(300).to_string(), "5m0s");
        assert_eq!(Duration::from_secs(5400).to_string(), "1h30m0s");
        assert_eq!(Duration::from_millis(250).to_string(), "250ms");
        assert_eq!(Duration::ZERO.to_string(), "0s");
    }

    #[test]
    fn yaml_accepts_strings_numbers_and_rejects_nonsense() {
        #[derive(serde::Deserialize, Debug)]
        struct Holder {
            d: Duration,
        }

        let parsed: Holder = serde_saphyr::from_str("d: 1h30m").unwrap();
        assert_eq!(parsed.d.nanos(), 90 * 60 * NANOS_PER_SEC);
        let parsed: Holder = serde_saphyr::from_str("d: 30").unwrap();
        assert_eq!(parsed.d.nanos(), 30 * NANOS_PER_SEC);
        let parsed: Holder = serde_saphyr::from_str("d: 0").unwrap();
        assert_eq!(parsed.d.nanos(), 0);

        let err = serde_saphyr::from_str::<Holder>("d: [1, 2]").unwrap_err();
        assert!(err.to_string().contains("scalar"), "{err}");
        assert!(serde_saphyr::from_str::<Holder>("d: 3 weeks").is_err());
    }
}
