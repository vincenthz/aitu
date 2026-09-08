//! Aggregation and table rendering.

use std::collections::{BTreeMap, BTreeSet};

use crate::pricing::Prices;
use crate::usage::{Event, Tokens};

/// Tokens plus cost for one group of events, tracking separately how much of
/// it could not be priced - so an unknown model shows up as a gap rather than
/// as a cheap one.
#[derive(Clone, Debug, Default)]
pub struct Totals {
    pub tokens: Tokens,
    pub usd: f64,
    pub calls: usize,
    pub unpriced_calls: usize,
    pub unpriced_tokens: u64,
    sessions: BTreeSet<String>,
}

impl Totals {
    pub fn add(&mut self, event: &Event, prices: &Prices) {
        self.tokens.add(&event.tokens);
        self.calls += 1;
        self.sessions.insert(event.session.clone());
        match prices.lookup(&event.model, event.speed) {
            Some(price) => self.usd += price.cost(&event.tokens),
            None => {
                self.unpriced_calls += 1;
                self.unpriced_tokens += event.tokens.total();
            }
        }
    }

    pub fn sessions(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_partial(&self) -> bool {
        self.unpriced_calls > 0
    }

    pub fn cost_cell(&self) -> String {
        if self.calls > 0 && self.unpriced_calls == self.calls {
            return String::from("n/a");
        }
        let marker = if self.is_partial() { "+" } else { "" };
        format!("${}{marker}", money(self.usd))
    }
}

/// Events grouped by a key, ready to render.
pub struct Grouped<K> {
    pub rows: Vec<(K, Totals)>,
    pub overall: Totals,
}

pub fn group_by<K, F>(events: &[Event], prices: &Prices, key: F) -> Grouped<K>
where
    K: Ord + Clone,
    F: Fn(&Event) -> K,
{
    let mut groups: BTreeMap<K, Totals> = BTreeMap::new();
    let mut overall = Totals::default();
    for event in events {
        groups.entry(key(event)).or_default().add(event, prices);
        overall.add(event, prices);
    }
    Grouped {
        rows: groups.into_iter().collect(),
        overall,
    }
}

/// Every model seen that has no price, with how much it accounts for. This is
/// the signal that the built-in price table needs a config entry.
pub fn unpriced_models(events: &[Event], prices: &Prices) -> Vec<(String, u64)> {
    let mut models: BTreeMap<String, u64> = BTreeMap::new();
    for event in events {
        if prices.lookup(&event.model, event.speed).is_none() {
            *models.entry(event.model.clone()).or_default() += event.tokens.total();
        }
    }
    let mut models: Vec<(String, u64)> = models.into_iter().collect();
    models.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    models
}

/// Right-align every column except the first, which is usually a name.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }
    }

    let mut out = String::new();
    let render = |out: &mut String, cells: &[String]| {
        for (index, cell) in cells.iter().enumerate() {
            if index > 0 {
                out.push_str("  ");
            }
            let pad = widths[index].saturating_sub(cell.chars().count());
            if index == 0 {
                out.push_str(cell);
                if index + 1 < cells.len() {
                    out.push_str(&" ".repeat(pad));
                }
            } else {
                out.push_str(&" ".repeat(pad));
                out.push_str(cell);
            }
        }
        out.push('\n');
    };

    let head: Vec<String> = headers.iter().map(|header| header.to_string()).collect();
    render(&mut out, &head);
    let rule: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    render(&mut out, &rule);
    for row in rows {
        render(&mut out, row);
    }
    out
}

/// The usage columns shared by every report.
pub const USAGE_HEADERS: [&str; 6] = ["input", "cache wr", "cache rd", "output", "total", "cost"];

pub fn usage_cells(totals: &Totals) -> Vec<String> {
    vec![
        count(totals.tokens.input),
        count(totals.tokens.cache_write()),
        count(totals.tokens.cache_read),
        count(totals.tokens.output),
        count(totals.tokens.total()),
        totals.cost_cell(),
    ]
}

/// Short human counts: 1.2k, 34.6M, 5.2B.
pub fn count(value: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "B"), (1_000_000, "M"), (1_000, "k")];
    for (scale, suffix) in UNITS {
        if value >= scale {
            let scaled = value as f64 / scale as f64;
            return if scaled < 10.0 {
                format!("{scaled:.2}{suffix}")
            } else if scaled < 100.0 {
                format!("{scaled:.1}{suffix}")
            } else {
                format!("{scaled:.0}{suffix}")
            };
        }
    }
    value.to_string()
}

/// Dollars, with enough precision that a small amount is not shown as 0.00.
pub fn money(usd: f64) -> String {
    if usd > 0.0 && usd < 0.01 {
        return format!("{usd:.4}");
    }
    format!("{usd:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Prices;
    use crate::usage::{Agent, Speed};

    fn event(model: &str, session: &str, input: u64) -> Event {
        Event {
            agent: Agent::Claude,
            home: String::from("h"),
            session: session.to_string(),
            date: String::from("2026-09-08"),
            project: String::from("/work"),
            model: model.to_string(),
            speed: Speed::Standard,
            tokens: Tokens {
                input,
                output: 10,
                ..Tokens::default()
            },
        }
    }

    #[test]
    fn groups_and_counts_distinct_sessions() {
        let prices = Prices::default();
        let events = vec![
            event("claude-opus-5", "a", 1_000_000),
            event("claude-opus-5", "a", 2_000_000),
            event("claude-opus-5", "b", 3_000_000),
        ];

        let grouped = group_by(&events, &prices, |e| e.session.clone());

        assert_eq!(grouped.rows.len(), 2);
        assert_eq!(grouped.rows[0].1.calls, 2);
        assert_eq!(grouped.overall.sessions(), 2);
        assert_eq!(grouped.overall.tokens.input, 6_000_000);
    }

    #[test]
    fn an_unpriced_model_is_reported_as_a_gap_not_as_free() {
        let prices = Prices::default();
        let events = vec![event("model-from-the-future", "a", 1_000_000)];

        let grouped = group_by(&events, &prices, |e| e.model.clone());

        assert_eq!(grouped.overall.usd, 0.0);
        assert_eq!(grouped.overall.unpriced_calls, 1);
        assert_eq!(grouped.overall.cost_cell(), "n/a");
        assert_eq!(
            unpriced_models(&events, &prices),
            vec![(String::from("model-from-the-future"), 1_000_010)]
        );
    }

    #[test]
    fn a_partly_priced_total_is_marked_as_a_lower_bound() {
        let prices = Prices::default();
        let events = vec![
            event("claude-opus-5", "a", 1_000_000),
            event("model-from-the-future", "a", 1_000_000),
        ];

        let grouped = group_by(&events, &prices, |e| e.session.clone());

        assert!(grouped.overall.is_partial());
        assert!(grouped.overall.cost_cell().ends_with('+'));
        assert!(grouped.overall.usd > 0.0);
    }

    #[test]
    fn scales_counts_for_reading() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_200), "1.20k");
        assert_eq!(count(34_600_000), "34.6M");
        assert_eq!(count(5_186_940_523), "5.19B");
    }

    #[test]
    fn shows_extra_precision_for_sub_cent_amounts() {
        assert_eq!(money(0.0), "0.00");
        assert_eq!(money(0.0042), "0.0042");
        assert_eq!(money(412.8), "412.80");
    }

    #[test]
    fn renders_a_table_with_a_rule_and_aligned_columns() {
        let rendered = table(
            &["name", "n"],
            &[
                vec![String::from("a"), String::from("1")],
                vec![String::from("long"), String::from("22")],
            ],
        );
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(lines[0], "name   n");
        assert_eq!(lines[1], "----  --");
        assert_eq!(lines[2], "a      1");
        assert_eq!(lines[3], "long  22");
    }
}
