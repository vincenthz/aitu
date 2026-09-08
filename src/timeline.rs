//! Turning log timestamps into calendar days.
//!
//! Session logs record UTC instants. Slicing the first ten characters off the
//! string is tempting and wrong: it buckets by UTC day, so for anyone far from
//! Greenwich a large share of an evening's work lands on the wrong date. Days
//! are therefore resolved in one explicit timezone, the same one for every
//! agent.

use chrono::{DateTime, Local, Utc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Clock {
    /// The machine's timezone. What a person means by "today".
    Local,
    /// UTC, for comparing against tools that bucket by the raw timestamp.
    Utc,
}

impl Clock {
    pub fn label(self) -> &'static str {
        match self {
            Clock::Local => "local",
            Clock::Utc => "UTC",
        }
    }

    /// `YYYY-MM-DD` for an RFC 3339 timestamp. Unparseable input falls back to
    /// the leading date of the string so a malformed record still lands
    /// somewhere plausible rather than being dropped.
    pub fn date_of(self, timestamp: &str) -> String {
        match DateTime::parse_from_rfc3339(timestamp) {
            Ok(instant) => match self {
                Clock::Local => instant.with_timezone(&Local).format("%Y-%m-%d").to_string(),
                Clock::Utc => instant.with_timezone(&Utc).format("%Y-%m-%d").to_string(),
            },
            Err(_) => timestamp.get(0..10).unwrap_or("unknown").to_string(),
        }
    }
}

/// An inclusive `--since` / `--until` day range.
#[derive(Clone, Copy, Debug, Default)]
pub struct Range {
    pub since: Option<[u8; 10]>,
    pub until: Option<[u8; 10]>,
}

impl Range {
    pub fn new(since: Option<&str>, until: Option<&str>) -> anyhow::Result<Self> {
        Ok(Self {
            since: since.map(parse_day).transpose()?,
            until: until.map(parse_day).transpose()?,
        })
    }

    pub fn is_open(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }

    pub fn contains(&self, date: &str) -> bool {
        // `YYYY-MM-DD` sorts lexicographically, so byte comparison is enough.
        let bytes = date.as_bytes();
        if let Some(since) = &self.since
            && bytes < since.as_slice()
        {
            return false;
        }
        if let Some(until) = &self.until
            && bytes > until.as_slice()
        {
            return false;
        }
        true
    }
}

fn parse_day(value: &str) -> anyhow::Result<[u8; 10]> {
    let value = value.trim();
    let bytes = value.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
    if !shaped {
        return Err(anyhow::anyhow!("expected a YYYY-MM-DD date, got {value:?}"));
    }
    let mut day = [0u8; 10];
    day.copy_from_slice(bytes);
    Ok(day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_and_local_disagree_for_a_late_evening_call() {
        // 23:30 in UTC+8 is still the previous day in UTC.
        let timestamp = "2026-09-08T15:30:00.000Z";

        assert_eq!(Clock::Utc.date_of(timestamp), "2026-09-08");
        // Only assert the shape here; the machine's zone decides the value.
        assert_eq!(Clock::Local.date_of(timestamp).len(), 10);
    }

    #[test]
    fn resolves_an_offset_timestamp_to_the_same_instant() {
        assert_eq!(
            Clock::Utc.date_of("2026-09-08T01:00:00+08:00"),
            "2026-09-07"
        );
    }

    #[test]
    fn falls_back_to_the_leading_date_of_an_unparseable_timestamp() {
        assert_eq!(Clock::Utc.date_of("2026-09-08 nonsense"), "2026-09-08");
        assert_eq!(Clock::Utc.date_of("junk"), "unknown");
    }

    #[test]
    fn range_bounds_are_inclusive() {
        let range = Range::new(Some("2026-09-02"), Some("2026-09-04")).unwrap();

        assert!(!range.contains("2026-09-01"));
        assert!(range.contains("2026-09-02"));
        assert!(range.contains("2026-09-04"));
        assert!(!range.contains("2026-09-05"));
    }

    #[test]
    fn an_open_range_contains_everything_including_unknown_days() {
        let range = Range::default();

        assert!(range.is_open());
        assert!(range.contains("unknown"));
    }

    #[test]
    fn rejects_a_malformed_date() {
        assert!(Range::new(Some("2026-9-8"), None).is_err());
        assert!(Range::new(Some("yesterday"), None).is_err());
        assert!(Range::new(Some("2026-09-08"), None).is_ok());
    }
}
