//! Core data model: one `Event` per billable model call.

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            _ => None,
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Response speed tier. Claude Code records this per message; fast mode is
/// billed at a premium, so it needs its own price lookup.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Speed {
    #[default]
    Standard,
    Fast,
}

/// Token counts for a single model call, in the five categories that are
/// billed at distinct rates. Cache creation is split by TTL because a 1-hour
/// write costs 2x the base input rate while a 5-minute write costs 1.25x.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tokens {
    /// Fresh (uncached, non-cache-write) input tokens.
    pub input: u64,
    /// Tokens read from the prompt cache.
    pub cache_read: u64,
    /// Tokens written to the cache with a 5-minute TTL.
    pub cache_write_5m: u64,
    /// Tokens written to the cache with a 1-hour TTL.
    pub cache_write_1h: u64,
    /// Output tokens, including reasoning/thinking tokens.
    pub output: u64,
}

impl Tokens {
    pub fn cache_write(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }

    /// Every token that passed through the model. Dominated by cache reads in
    /// practice, so it is a poor spend proxy - report cost for that.
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write() + self.output
    }

    /// Tokens that were freshly processed, i.e. everything except cache reads.
    /// A much better proxy for real work than `total`.
    pub fn billed_input(&self) -> u64 {
        self.input + self.cache_write()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    pub fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.output += other.output;
    }
}

/// One billable model call, already attributed to a day, project and home.
#[derive(Clone, Debug)]
pub struct Event {
    pub agent: Agent,
    /// Name of the configured home this came from.
    pub home: String,
    pub session: String,
    /// Calendar day in the reporting timezone, `YYYY-MM-DD`.
    pub date: String,
    /// Working directory the call was made from.
    pub project: String,
    pub model: String,
    pub speed: Speed,
    pub tokens: Tokens,
}

/// What a scan produced, plus anything the user should know about it.
#[derive(Debug, Default)]
pub struct Scan {
    pub events: Vec<Event>,
    /// Session files read, per agent.
    pub files: usize,
    pub warnings: Vec<String>,
}

impl Scan {
    pub fn absorb(&mut self, other: Scan) {
        self.events.extend(other.events);
        self.files += other.files;
        self.warnings.extend(other.warnings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_counts_every_category_once() {
        let tokens = Tokens {
            input: 1,
            cache_read: 2,
            cache_write_5m: 4,
            cache_write_1h: 8,
            output: 16,
        };

        assert_eq!(tokens.cache_write(), 12);
        assert_eq!(tokens.total(), 31);
        assert_eq!(tokens.billed_input(), 13);
    }

    #[test]
    fn parses_agent_aliases() {
        assert_eq!(Agent::parse("Claude"), Some(Agent::Claude));
        assert_eq!(Agent::parse("codex"), Some(Agent::Codex));
        assert_eq!(Agent::parse("opencode"), None);
    }
}
