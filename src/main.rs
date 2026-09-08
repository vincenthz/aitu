//! aitu - AI token usage across Claude Code and Codex sessions.

mod claude;
mod codex;
mod config;
mod pricing;
mod report;
mod timeline;
mod usage;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::process::ExitCode;

use config::{Config, Home};
use pricing::Prices;
use report::{USAGE_HEADERS, count, group_by, money, table, unpriced_models, usage_cells};
use timeline::{Clock, Range};
use usage::{Agent, Event, Scan};

#[derive(Parser, Debug)]
#[command(name = "aitu", version, about = "AI token usage: claude and codex")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Only report this home; repeat for several. Default: every configured home.
    #[arg(long = "home", short = 'H', global = true, value_name = "NAME")]
    homes: Vec<String>,

    /// Only report this agent.
    #[arg(long, short = 'a', global = true, value_name = "claude|codex")]
    agent: Option<String>,

    /// Only include usage on or after this day (YYYY-MM-DD).
    #[arg(long, global = true, value_name = "DATE")]
    since: Option<String>,

    /// Only include usage on or before this day (YYYY-MM-DD).
    #[arg(long, global = true, value_name = "DATE")]
    until: Option<String>,

    /// Bucket days by UTC instead of the machine's timezone.
    #[arg(long, global = true)]
    utc: bool,

    /// Maximum rows per table.
    #[arg(
        long,
        short = 'n',
        global = true,
        value_name = "N",
        default_value_t = 20
    )]
    limit: usize,
}

#[derive(Clone, Debug, Subcommand)]
enum Command {
    /// Totals per agent, and per home when more than one has usage.
    Summary,
    /// Daily totals, most recent first.
    Days,
    /// Totals per working directory.
    Projects,
    /// Totals per model, and which models have no price.
    Models,
    /// Totals per session, most expensive first.
    Sessions,
    /// Show the configured homes and what was found in each.
    Homes,
    /// Write a starter config file.
    Init,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("aitu: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Write the whole report in one go. A reader that closes early - `| head` -
/// is a normal way to use a CLI, not an error, so a broken pipe ends the
/// program quietly instead of panicking inside `println!`.
fn emit(text: &str) -> Result<()> {
    let mut stdout = std::io::stdout();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(Into::into),
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.clone().unwrap_or(Command::Summary);
    let config = config::load()?;

    if matches!(command, Command::Init) {
        config::write_example_config(&config.path)?;
        println!("wrote {}", config.path.display());
        return Ok(());
    }

    let clock = if cli.utc { Clock::Utc } else { Clock::Local };
    let prices = Prices::new(&config.prices);
    let agent = cli
        .agent
        .as_deref()
        .map(|value| {
            Agent::parse(value)
                .ok_or_else(|| anyhow!("unknown agent {value:?}; try claude or codex"))
        })
        .transpose()?;
    let homes = select_homes(&config, &cli.homes)?;
    let range = Range::new(cli.since.as_deref(), cli.until.as_deref())?;

    if matches!(command, Command::Homes) {
        return emit(&show_homes(&config, &homes, clock, &prices));
    }

    let scan = collect(&homes, agent, clock);
    let events: Vec<Event> = scan
        .events
        .iter()
        .filter(|event| range.contains(&event.date))
        .cloned()
        .collect();

    if events.is_empty() {
        let mut out = String::from("No usage found.\n");
        warnings(&mut out, &scan);
        return emit(&out);
    }

    let limit = cli.limit.max(1);
    let mut out = match command {
        Command::Summary => summary(&events, &prices, &homes, clock, &range),
        Command::Days => days(&events, &prices, limit, clock),
        Command::Projects => projects(&events, &prices, limit),
        Command::Models => models(&events, &prices, limit),
        Command::Sessions => sessions(&events, &prices, limit),
        Command::Homes | Command::Init => unreachable!("handled above"),
    };

    unpriced(&mut out, &events, &prices);
    warnings(&mut out, &scan);
    emit(&out)
}

fn select_homes<'a>(config: &'a Config, wanted: &[String]) -> Result<Vec<&'a Home>> {
    if wanted.is_empty() {
        return Ok(config.homes.iter().collect());
    }
    let mut selected = Vec::new();
    for name in wanted {
        let home = config
            .homes
            .iter()
            .find(|home| &home.name == name)
            .ok_or_else(|| {
                let known: Vec<&str> = config.homes.iter().map(|h| h.name.as_str()).collect();
                anyhow!(
                    "no home named {name:?} in {}; configured: {}",
                    config.path.display(),
                    known.join(", ")
                )
            })?;
        selected.push(home);
    }
    Ok(selected)
}

/// Read every selected home. Dedup state is shared across the whole scan so a
/// call that appears in more than one place is counted once.
fn collect(homes: &[&Home], agent: Option<Agent>, clock: Clock) -> Scan {
    let mut seen = HashSet::new();
    let mut scan = Scan::default();
    for home in homes {
        if agent.is_none_or(|a| a == Agent::Claude)
            && let Some(dir) = &home.claude
        {
            scan.absorb(claude::scan(&home.name, dir, clock, &mut seen));
        }
        if agent.is_none_or(|a| a == Agent::Codex)
            && let Some(dir) = &home.codex
        {
            scan.absorb(codex::scan(&home.name, dir, clock, &mut seen));
        }
    }
    scan
}

fn summary(
    events: &[Event],
    prices: &Prices,
    homes: &[&Home],
    clock: Clock,
    range: &Range,
) -> String {
    let by_agent = group_by(events, prices, |event| event.agent);
    let overall = &by_agent.overall;
    let mut out = String::new();

    let scope = if range.is_open() {
        String::new()
    } else {
        let since = range.since.map(bytes_to_day).unwrap_or_default();
        let until = range.until.map(bytes_to_day).unwrap_or_default();
        format!("  {since}..{until}")
    };
    let _ = writeln!(out, "aitu summary  ({} days){scope}\n", clock.label());

    let mut rows = Vec::new();
    for (agent, totals) in &by_agent.rows {
        rows.push(named_row(agent.label(), totals));
    }
    if by_agent.rows.len() > 1 {
        rows.push(named_row("all", overall));
    }
    out.push_str(&table(&headers("agent"), &rows));

    // Only worth a second table when more than one home is in play.
    let by_home = group_by(events, prices, |event| event.home.clone());
    if by_home.rows.len() > 1 || homes.len() > 1 {
        out.push('\n');
        let rows: Vec<Vec<String>> = by_home
            .rows
            .iter()
            .map(|(home, totals)| named_row(home, totals))
            .collect();
        out.push_str(&table(&headers("home"), &rows));
    }

    let _ = writeln!(
        out,
        "\n{} calls over {}, {} tokens, {} freshly processed",
        overall.calls,
        plural(overall.sessions(), "session"),
        count(overall.tokens.total()),
        count(overall.tokens.billed_input() + overall.tokens.output),
    );
    let _ = if overall.unpriced_calls == overall.calls {
        writeln!(out, "estimated cost unavailable: no model here is priced")
    } else if overall.is_partial() {
        writeln!(
            out,
            "estimated cost at least ${} (some models are unpriced)",
            money(overall.usd)
        )
    } else {
        writeln!(out, "estimated cost ${}", money(overall.usd))
    };
    out
}

fn days(events: &[Event], prices: &Prices, limit: usize, clock: Clock) -> String {
    let grouped = group_by(events, prices, |event| event.date.clone());
    // Most recent first.
    let rows: Vec<Vec<String>> = grouped
        .rows
        .iter()
        .rev()
        .take(limit)
        .map(|(date, totals)| named_row(date, totals))
        .collect();

    let mut out = format!("daily usage  ({} days)\n\n", clock.label());
    out.push_str(&table(&headers("date"), &rows));
    truncation(&mut out, grouped.rows.len(), limit, "days");
    out
}

fn projects(events: &[Event], prices: &Prices, limit: usize) -> String {
    let mut ranked = group_by(events, prices, |event| event.project.clone()).rows;
    ranked.sort_by_key(|(_, totals)| std::cmp::Reverse(totals.tokens.total()));
    let rows: Vec<Vec<String>> = ranked
        .iter()
        .take(limit)
        .map(|(project, totals)| named_row(project, totals))
        .collect();

    let mut out = String::from("usage by project\n\n");
    out.push_str(&table(&headers("project"), &rows));
    truncation(&mut out, ranked.len(), limit, "projects");
    out
}

fn models(events: &[Event], prices: &Prices, limit: usize) -> String {
    let mut ranked = group_by(events, prices, |event| (event.agent, event.model.clone())).rows;
    ranked.sort_by_key(|(_, totals)| std::cmp::Reverse(totals.tokens.total()));

    let mut rows = Vec::new();
    for ((agent, model), totals) in ranked.iter().take(limit) {
        let mut row = vec![
            agent.label().to_string(),
            model.clone(),
            totals.calls.to_string(),
        ];
        row.extend(usage_cells(totals));
        rows.push(row);
    }
    let mut columns = vec!["agent", "model", "calls"];
    columns.extend(USAGE_HEADERS);

    let mut out = String::from("usage by model\n\n");
    out.push_str(&table(&columns, &rows));
    truncation(&mut out, ranked.len(), limit, "models");
    out
}

/// Grouped by session id alone. A session that `cd`s into subdirectories
/// still has one row - splitting it per directory would make the session
/// count disagree with every other report - labelled with whichever
/// directory accounts for most of its tokens.
fn sessions(events: &[Event], prices: &Prices, limit: usize) -> String {
    let mut ranked = group_by(events, prices, |event| event.session.clone()).rows;
    ranked.sort_by(|a, b| {
        b.1.usd
            .partial_cmp(&a.1.usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.tokens.total().cmp(&a.1.tokens.total()))
    });

    let mut tokens_by_project: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
    for event in events {
        *tokens_by_project
            .entry(&event.session)
            .or_default()
            .entry(&event.project)
            .or_default() += event.tokens.total();
    }

    let mut rows = Vec::new();
    for (session, totals) in ranked.iter().take(limit) {
        let dominant = tokens_by_project
            .get(session.as_str())
            .and_then(|projects| {
                projects
                    .iter()
                    .max_by_key(|(_, tokens)| **tokens)
                    .map(|(project, _)| *project)
            })
            .unwrap_or("unknown");
        let extra = tokens_by_project
            .get(session.as_str())
            .map(|projects| projects.len())
            .unwrap_or(1);
        let project = if extra > 1 {
            format!("{dominant} (+{})", extra - 1)
        } else {
            dominant.to_string()
        };
        // The uuid prefix is enough to find a session on disk.
        let short = session.get(0..8).unwrap_or(session).to_string();
        let mut row = vec![short, project, totals.calls.to_string()];
        row.extend(usage_cells(totals));
        rows.push(row);
    }
    let mut columns = vec!["session", "project", "calls"];
    columns.extend(USAGE_HEADERS);

    let mut out = String::from("usage by session\n\n");
    out.push_str(&table(&columns, &rows));
    truncation(&mut out, ranked.len(), limit, "sessions");
    out
}

fn show_homes(config: &Config, homes: &[&Home], clock: Clock, prices: &Prices) -> String {
    let mut out = String::new();
    if config.loaded {
        let _ = writeln!(out, "config: {}", config.path.display());
    } else {
        let _ = writeln!(
            out,
            "config: {} (not present - using defaults; run `aitu init`)",
            config.path.display()
        );
    }
    if prices.override_count() > 0 {
        let _ = writeln!(out, "price overrides: {}", prices.override_count());
    }
    out.push('\n');

    let mut rows = Vec::new();
    for home in homes {
        for (agent, dir) in [
            (Agent::Claude, home.claude.as_ref()),
            (Agent::Codex, home.codex.as_ref()),
        ] {
            let Some(dir) = dir else { continue };
            let mut seen = HashSet::new();
            let scan = match agent {
                Agent::Claude => claude::scan(&home.name, dir, clock, &mut seen),
                Agent::Codex => codex::scan(&home.name, dir, clock, &mut seen),
            };
            let tokens: u64 = scan.events.iter().map(|event| event.tokens.total()).sum();
            rows.push(vec![
                home.name.clone(),
                agent.label().to_string(),
                dir.display().to_string(),
                if scan.warnings.is_empty() {
                    scan.files.to_string()
                } else {
                    String::from("-")
                },
                scan.events.len().to_string(),
                count(tokens),
            ]);
        }
    }
    out.push_str(&table(
        &["home", "agent", "path", "files", "calls", "tokens"],
        &rows,
    ));
    out
}

/// Column headers: a caller-named first column, then the usage columns.
fn headers(first: &str) -> Vec<&str> {
    let mut columns = vec![first, "sessions", "calls"];
    columns.extend(USAGE_HEADERS);
    columns
}

fn named_row(name: &str, totals: &report::Totals) -> Vec<String> {
    let mut row = vec![
        name.to_string(),
        totals.sessions().to_string(),
        totals.calls.to_string(),
    ];
    row.extend(usage_cells(totals));
    row
}

fn unpriced(out: &mut String, events: &[Event], prices: &Prices) {
    let models = unpriced_models(events, prices);
    if models.is_empty() {
        return;
    }
    out.push_str("\nNo price for these models, so they are missing from every cost above:\n");
    for (model, tokens) in &models {
        let _ = writeln!(out, "  {model}  ({} tokens)", count(*tokens));
    }
    out.push_str("Add them under [prices.\"<model>\"] in the config (`aitu init`).\n");
}

fn warnings(out: &mut String, scan: &Scan) {
    if scan.warnings.is_empty() {
        return;
    }
    out.push('\n');
    for warning in &scan.warnings {
        let _ = writeln!(out, "note: {warning}");
    }
}

fn truncation(out: &mut String, total: usize, limit: usize, noun: &str) {
    if total > limit {
        let _ = writeln!(out, "({limit} of {total} {noun}; --limit to see more)");
    }
}

fn bytes_to_day(day: [u8; 10]) -> String {
    String::from_utf8_lossy(&day).into_owned()
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}
