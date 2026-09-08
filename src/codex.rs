//! Codex session reader.
//!
//! Rollouts live as JSONL under `<codex home>/sessions/<yyyy>/<mm>/<dd>/`.
//! Only that subtree is read - `history.jsonl` at the codex home root holds
//! prompt text and is deliberately not touched.
//!
//! Two record types carry token counts:
//!
//! * `token_usage_record` - one per API response, with a `response_id` and a
//!   `usage` object that is already the per-response delta. Preferred: the id
//!   is a natural dedup key and no arithmetic is needed.
//! * `event_msg` / `token_count` - the UI event, whose `total_token_usage` is
//!   cumulative over the session. Used only for older rollouts that have no
//!   `token_usage_record`, by differencing consecutive totals.
//!
//! A rollout that has both is read from the first kind only, or every call
//! would be counted twice.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use walkdir::WalkDir;

use crate::timeline::Clock;
use crate::usage::{Agent, Event, Scan, Speed, Tokens};

pub fn scan(home: &str, codex_home: &Path, clock: Clock, seen: &mut HashSet<String>) -> Scan {
    let mut scan = Scan::default();
    let sessions = codex_home.join("sessions");
    if !sessions.is_dir() {
        scan.warnings.push(format!(
            "[{home}] no Codex sessions: {} is not a directory",
            sessions.display()
        ));
        return scan;
    }

    for entry in WalkDir::new(&sessions).sort_by_file_name() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                scan.warnings.push(format!("[{home}] walk failed: {err}"));
                continue;
            }
        };
        let path = entry.path();
        if !entry.file_type().is_file() || path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }

        scan.files += 1;
        match read_file(home, path, clock, seen) {
            Ok(file_scan) => scan.absorb(file_scan),
            Err(err) => scan.warnings.push(format!(
                "[{home}] failed to read {}: {err:#}",
                path.display()
            )),
        }
    }

    scan
}

/// A usage reading pulled off one line, before it becomes an `Event`.
struct Reading {
    /// Unique within the rollout; `None` when only the cumulative event exists.
    response_id: Option<String>,
    turn_id: Option<String>,
    timestamp: Option<String>,
    tokens: Tokens,
}

fn read_file(home: &str, path: &Path, clock: Clock, seen: &mut HashSet<String>) -> Result<Scan> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut scan = Scan::default();
    let mut malformed = 0usize;

    let mut session = file_stem(path);
    let mut project = String::from("unknown");
    let mut provider = String::new();
    let mut model = String::from("unknown");
    // `turn_context` announces the model for a turn before the usage records
    // of that turn arrive, so usage can be attributed to the right model even
    // when the model changes mid-session.
    let mut model_by_turn: HashMap<String, String> = HashMap::new();

    let mut per_response: Vec<Reading> = Vec::new();
    let mut cumulative: Vec<Reading> = Vec::new();

    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("read line {}", index + 1))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            malformed += 1;
            continue;
        };
        let timestamp = str_at(&value, "timestamp").map(str::to_string);
        let payload = value.get("payload");

        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if let Some(payload) = payload {
                    if let Some(id) =
                        str_at(payload, "session_id").or_else(|| str_at(payload, "id"))
                    {
                        session = id.to_string();
                    }
                    if let Some(cwd) = str_at(payload, "cwd") {
                        project = cwd.to_string();
                    }
                    if let Some(name) = str_at(payload, "model_provider") {
                        provider = name.to_string();
                    }
                }
            }
            Some("turn_context") => {
                if let Some(payload) = payload {
                    if let Some(cwd) = str_at(payload, "cwd") {
                        project = cwd.to_string();
                    }
                    if let Some(name) = str_at(payload, "model") {
                        model = name.to_string();
                        if let Some(turn) = str_at(payload, "turn_id") {
                            model_by_turn.insert(turn.to_string(), name.to_string());
                        }
                    }
                }
            }
            Some("token_usage_record") => {
                if let Some(payload) = payload
                    && let Some(usage) = payload.get("usage")
                {
                    {
                        per_response.push(Reading {
                            response_id: str_at(payload, "response_id").map(str::to_string),
                            turn_id: str_at(payload, "turn_id").map(str::to_string),
                            timestamp,
                            tokens: read_tokens(usage),
                        });
                    }
                }
            }
            Some("event_msg") => {
                let Some(payload) = payload else { continue };
                if payload.get("type").and_then(Value::as_str) != Some("token_count") {
                    continue;
                }
                if let Some(usage) = payload.pointer("/info/total_token_usage") {
                    cumulative.push(Reading {
                        response_id: None,
                        turn_id: None,
                        timestamp,
                        tokens: read_tokens(usage),
                    });
                }
            }
            _ => {}
        }
    }

    // Prefer per-response records; fall back to differencing the cumulative
    // totals only when the rollout predates them.
    let readings = if per_response.is_empty() {
        differences(cumulative)
    } else {
        per_response
    };

    let session_date = date_from_path(path);
    for (index, reading) in readings.into_iter().enumerate() {
        if reading.tokens.is_empty() {
            continue;
        }
        let key = match &reading.response_id {
            Some(id) => format!("codex:{id}"),
            None => format!("codex:{session}:{index}"),
        };
        if !seen.insert(key.clone()) {
            continue;
        }
        let date = reading
            .timestamp
            .as_deref()
            .map(|ts| clock.date_of(ts))
            .or_else(|| session_date.clone())
            .unwrap_or_else(|| String::from("unknown"));
        let model = reading
            .turn_id
            .as_deref()
            .and_then(|turn| model_by_turn.get(turn))
            .cloned()
            .unwrap_or_else(|| model.clone());

        scan.events.push(Event {
            agent: Agent::Codex,
            home: home.to_string(),
            session: session.clone(),
            date,
            project: project.clone(),
            model: label(&provider, &model),
            speed: Speed::Standard,
            tokens: reading.tokens,
        });
    }

    if malformed > 0 {
        scan.warnings.push(format!(
            "[{home}] skipped {malformed} unparseable line(s) in {}",
            path.display()
        ));
    }
    Ok(scan)
}

/// Codex reports `input_tokens` as the whole prompt, with `cached_input_tokens`
/// and `cache_write_input_tokens` as subsets of it, and
/// `reasoning_output_tokens` as a subset of `output_tokens`. Splitting the
/// prompt into disjoint buckets and leaving output alone keeps the sum equal
/// to the record's own `total_tokens`.
fn read_tokens(usage: &Value) -> Tokens {
    let prompt = u64_at(usage, "input_tokens");
    let cache_read = u64_at(usage, "cached_input_tokens").min(prompt);
    let cache_write = u64_at(usage, "cache_write_input_tokens").min(prompt - cache_read);

    Tokens {
        input: prompt - cache_read - cache_write,
        cache_read,
        cache_write_5m: cache_write,
        cache_write_1h: 0,
        output: u64_at(usage, "output_tokens"),
    }
}

/// Turn a series of cumulative readings into per-call deltas.
fn differences(readings: Vec<Reading>) -> Vec<Reading> {
    let mut previous = Tokens::default();
    let mut deltas = Vec::with_capacity(readings.len());
    for reading in readings {
        let current = reading.tokens;
        // A cumulative counter that goes backwards means the session was
        // rebased (a compaction, or a resumed thread starting over). Treat the
        // new reading as a fresh baseline rather than emitting a negative.
        let tokens = if current.total() < previous.total() {
            current
        } else {
            Tokens {
                input: current.input.saturating_sub(previous.input),
                cache_read: current.cache_read.saturating_sub(previous.cache_read),
                cache_write_5m: current
                    .cache_write_5m
                    .saturating_sub(previous.cache_write_5m),
                cache_write_1h: current
                    .cache_write_1h
                    .saturating_sub(previous.cache_write_1h),
                output: current.output.saturating_sub(previous.output),
            }
        };
        previous = current;
        deltas.push(Reading { tokens, ..reading });
    }
    deltas
}

fn label(provider: &str, model: &str) -> String {
    if provider.is_empty() || model == "unknown" {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    }
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn u64_at(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Rollouts are filed under `sessions/YYYY/MM/DD/`, a last resort for records
/// with no timestamp of their own.
fn date_from_path(path: &Path) -> Option<String> {
    let parts: Vec<&str> = path
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect();
    parts.windows(3).find_map(|window| {
        let [year, month, day] = window else {
            return None;
        };
        let digits = |text: &str, len: usize| {
            text.len() == len && text.bytes().all(|byte| byte.is_ascii_digit())
        };
        (digits(year, 4) && digits(month, 2) && digits(day, 2))
            .then(|| format!("{year}-{month}-{day}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_the_prompt_into_disjoint_buckets_preserving_the_total() {
        let tokens = read_tokens(&json!({
            "input_tokens": 14786,
            "cached_input_tokens": 11904,
            "cache_write_input_tokens": 0,
            "output_tokens": 157,
            "reasoning_output_tokens": 0,
            "total_tokens": 14943
        }));

        assert_eq!(tokens.input, 2882);
        assert_eq!(tokens.cache_read, 11904);
        assert_eq!(tokens.output, 157);
        assert_eq!(tokens.total(), 14943);
    }

    #[test]
    fn does_not_add_reasoning_tokens_to_output() {
        // Real record: input + output == total_tokens, so reasoning is a
        // breakdown of output, not an extra category.
        let tokens = read_tokens(&json!({
            "input_tokens": 37455,
            "cached_input_tokens": 0,
            "output_tokens": 232,
            "reasoning_output_tokens": 10,
            "total_tokens": 37687
        }));

        assert_eq!(tokens.output, 232);
        assert_eq!(tokens.total(), 37687);
    }

    #[test]
    fn accounts_for_cache_writes_without_inflating_the_prompt() {
        let tokens = read_tokens(&json!({
            "input_tokens": 1000,
            "cached_input_tokens": 600,
            "cache_write_input_tokens": 300,
            "output_tokens": 50
        }));

        assert_eq!(tokens.input, 100);
        assert_eq!(tokens.cache_read, 600);
        assert_eq!(tokens.cache_write_5m, 300);
        assert_eq!(tokens.total(), 1050);
    }

    fn cumulative(total: u64) -> Reading {
        Reading {
            response_id: None,
            turn_id: None,
            timestamp: None,
            tokens: Tokens {
                input: total,
                ..Tokens::default()
            },
        }
    }

    #[test]
    fn differences_a_cumulative_series_into_per_call_deltas() {
        let deltas = differences(vec![cumulative(100), cumulative(250), cumulative(400)]);
        let totals: Vec<u64> = deltas.iter().map(|d| d.tokens.total()).collect();

        assert_eq!(totals, vec![100, 150, 150]);
    }

    #[test]
    fn restarts_the_baseline_when_a_cumulative_counter_goes_backwards() {
        let deltas = differences(vec![cumulative(100), cumulative(400), cumulative(50)]);
        let totals: Vec<u64> = deltas.iter().map(|d| d.tokens.total()).collect();

        assert_eq!(totals, vec![100, 300, 50]);
    }

    #[test]
    fn extracts_the_session_date_from_the_rollout_path() {
        let path = Path::new("/h/.codex/sessions/2026/09/08/rollout-x.jsonl");

        assert_eq!(date_from_path(path).as_deref(), Some("2026-09-08"));
    }

    fn write_rollout(root: &Path, name: &str, lines: &[Value]) {
        let dir = root.join("sessions").join("2026").join("09").join("08");
        std::fs::create_dir_all(&dir).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(dir.join(name), body.join("\n") + "\n").unwrap();
    }

    fn meta() -> Value {
        json!({
            "timestamp": "2026-09-08T12:06:58.241Z",
            "type": "session_meta",
            "payload": { "session_id": "s1", "cwd": "/work", "model_provider": "openai" }
        })
    }

    fn turn(turn_id: &str, model: &str) -> Value {
        json!({
            "timestamp": "2026-09-08T12:06:59.583Z",
            "type": "turn_context",
            "payload": { "turn_id": turn_id, "cwd": "/work", "model": model }
        })
    }

    fn record(response: &str, turn_id: &str, prompt: u64, output: u64) -> Value {
        json!({
            "timestamp": "2026-09-08T12:07:04.908Z",
            "type": "token_usage_record",
            "payload": {
                "response_id": response,
                "turn_id": turn_id,
                "usage": {
                    "input_tokens": prompt,
                    "cached_input_tokens": 0,
                    "output_tokens": output
                }
            }
        })
    }

    fn token_count(total: u64) -> Value {
        json!({
            "timestamp": "2026-09-08T12:07:05.739Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": { "total_token_usage": { "input_tokens": total, "output_tokens": 0 } }
            }
        })
    }

    #[test]
    fn ignores_the_cumulative_event_when_per_response_records_exist() {
        let root = std::env::temp_dir().join(format!("aitu-codex-both-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_rollout(
            &root,
            "rollout-a.jsonl",
            &[
                meta(),
                turn("t1", "gpt-6-astra"),
                record("resp_1", "t1", 100, 10),
                token_count(110),
                record("resp_2", "t1", 200, 20),
                token_count(330),
            ],
        );

        let mut seen = HashSet::new();
        let scan = scan("h", &root, Clock::Utc, &mut seen);
        let total: u64 = scan.events.iter().map(|e| e.tokens.total()).sum();

        assert_eq!(scan.events.len(), 2);
        assert_eq!(total, 330);
        assert_eq!(scan.events[0].model, "openai/gpt-6-astra");
        assert_eq!(scan.events[0].project, "/work");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn falls_back_to_the_cumulative_event_for_older_rollouts() {
        let root = std::env::temp_dir().join(format!("aitu-codex-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_rollout(
            &root,
            "rollout-a.jsonl",
            &[
                meta(),
                turn("t1", "gpt-5.4"),
                token_count(100),
                token_count(250),
            ],
        );

        let mut seen = HashSet::new();
        let scan = scan("h", &root, Clock::Utc, &mut seen);
        let total: u64 = scan.events.iter().map(|e| e.tokens.total()).sum();

        assert_eq!(scan.events.len(), 2);
        assert_eq!(total, 250);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn attributes_usage_to_the_model_of_its_own_turn() {
        let root = std::env::temp_dir().join(format!("aitu-codex-model-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_rollout(
            &root,
            "rollout-a.jsonl",
            &[
                meta(),
                turn("t1", "gpt-6-astra"),
                turn("t2", "gpt-5.4"),
                record("resp_1", "t1", 100, 10),
                record("resp_2", "t2", 100, 10),
            ],
        );

        let mut seen = HashSet::new();
        let scan = scan("h", &root, Clock::Utc, &mut seen);

        assert_eq!(scan.events[0].model, "openai/gpt-6-astra");
        assert_eq!(scan.events[1].model, "openai/gpt-5.4");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn never_reads_outside_the_sessions_subtree() {
        let root = std::env::temp_dir().join(format!("aitu-codex-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_rollout(
            &root,
            "rollout-a.jsonl",
            &[meta(), record("r1", "t1", 100, 10)],
        );
        // Prompt text at the codex home root must not be touched.
        std::fs::write(
            root.join("history.jsonl"),
            json!({"text": "secret"}).to_string(),
        )
        .unwrap();

        let mut seen = HashSet::new();
        let scan = scan("h", &root, Clock::Utc, &mut seen);

        assert_eq!(scan.files, 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
