//! Claude Code session reader.
//!
//! Sessions live as JSONL under `<config dir>/projects/<slug>/<uuid>.jsonl`,
//! one JSON object per line. Assistant lines carry `message.usage`.
//!
//! Two things matter for correctness:
//!
//! * A single assistant message is written across several lines (one per
//!   content block) that all repeat the same cumulative `usage`. Counting
//!   lines counts the same call several times.
//! * Forking or resuming a session replays earlier calls into a new file
//!   under a *new* `sessionId`. The dedup key must therefore be the message
//!   identity alone - `message.id` plus `requestId` - and must be applied
//!   across every file, not per file.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use walkdir::WalkDir;

use crate::timeline::Clock;
use crate::usage::{Agent, Event, Scan, Speed, Tokens};

pub fn scan(home: &str, config_dir: &Path, clock: Clock, seen: &mut HashSet<String>) -> Scan {
    let mut scan = Scan::default();
    let projects = config_dir.join("projects");
    if !projects.is_dir() {
        scan.warnings.push(format!(
            "[{home}] no Claude Code sessions: {} is not a directory",
            projects.display()
        ));
        return scan;
    }

    for entry in WalkDir::new(&projects).sort_by_file_name() {
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

fn read_file(home: &str, path: &Path, clock: Clock, seen: &mut HashSet<String>) -> Result<Scan> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut scan = Scan::default();
    let mut malformed = 0usize;
    // Falls back to the file name, which is the session uuid.
    let mut session = file_stem(path);
    let mut project = project_from_slug(path);

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

        if let Some(id) = str_at(&value, "sessionId") {
            session = id.to_string();
        }
        if let Some(cwd) = str_at(&value, "cwd") {
            project = cwd.to_string();
        }

        let Some(usage) = value.pointer("/message/usage") else {
            continue;
        };
        let tokens = read_tokens(usage);
        if tokens.is_empty() {
            continue;
        }

        // `message.id` identifies the API response; `requestId` distinguishes
        // retries of the same logical message. Neither includes the session,
        // so a replayed call collapses onto its original.
        let key = match (
            value.pointer("/message/id").and_then(Value::as_str),
            str_at(&value, "requestId"),
        ) {
            (Some(id), Some(request)) => format!("{id}:{request}"),
            (Some(id), None) => id.to_string(),
            // No message id at all: fall back to the line's own uuid, which is
            // unique per line, so such a record is never deduped away.
            (None, _) => match str_at(&value, "uuid") {
                Some(uuid) => format!("uuid:{uuid}"),
                None => format!("{}:{}", path.display(), index),
            },
        };
        if !seen.insert(key.clone()) {
            continue;
        }

        let date = match str_at(&value, "timestamp") {
            Some(timestamp) => clock.date_of(timestamp),
            None => String::from("unknown"),
        };

        scan.events.push(Event {
            agent: Agent::Claude,
            home: home.to_string(),
            session: session.clone(),
            date,
            project: project.clone(),
            model: value
                .pointer("/message/model")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            speed: read_speed(usage),
            tokens,
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

/// Anthropic reports the three input categories separately: `input_tokens`
/// excludes both cache reads and cache writes. The 5m/1h split of the write
/// lives in the nested `cache_creation` object; `cache_creation_input_tokens`
/// is its total and is the authority on the sum.
fn read_tokens(usage: &Value) -> Tokens {
    let write_total = u64_at(usage, "cache_creation_input_tokens");
    let write_1h = usage
        .pointer("/cache_creation/ephemeral_1h_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let write_5m = usage
        .pointer("/cache_creation/ephemeral_5m_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // Trust the total, and treat anything the breakdown does not account for
    // as a 5-minute write (the default TTL). This keeps the sum exact even
    // when the nested object is absent or lags a new TTL.
    let write_1h = write_1h.min(write_total);
    let write_5m = write_5m.max(write_total - write_1h);

    Tokens {
        input: u64_at(usage, "input_tokens"),
        cache_read: u64_at(usage, "cache_read_input_tokens"),
        cache_write_5m: write_5m,
        cache_write_1h: write_1h,
        // Thinking tokens are already part of `output_tokens`
        // (`output_tokens_details.thinking_tokens` is a breakdown, not an
        // addition), so adding them would double count.
        output: u64_at(usage, "output_tokens"),
    }
}

fn read_speed(usage: &Value) -> Speed {
    match usage.get("speed").and_then(Value::as_str) {
        Some("fast") => Speed::Fast,
        _ => Speed::Standard,
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

/// Last-resort project name from the directory slug. Claude Code encodes the
/// cwd by replacing `/` with `-`, which is not reversible when a path
/// component itself contains `-`, so this is only used until a line supplies
/// the real `cwd`.
fn project_from_slug(path: &Path) -> String {
    path.parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .map(|name| format!("~slug:{name}"))
        .unwrap_or_else(|| String::from("unknown"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_cache_writes_by_ttl_from_the_nested_object() {
        let tokens = read_tokens(&json!({
            "input_tokens": 2,
            "cache_read_input_tokens": 22594,
            "cache_creation_input_tokens": 15802,
            "output_tokens": 204,
            "cache_creation": {
                "ephemeral_1h_input_tokens": 15802,
                "ephemeral_5m_input_tokens": 0
            }
        }));

        assert_eq!(tokens.input, 2);
        assert_eq!(tokens.cache_read, 22594);
        assert_eq!(tokens.cache_write_1h, 15802);
        assert_eq!(tokens.cache_write_5m, 0);
        assert_eq!(tokens.total(), 38602);
    }

    #[test]
    fn treats_an_absent_breakdown_as_a_five_minute_write() {
        let tokens = read_tokens(&json!({
            "input_tokens": 10,
            "cache_creation_input_tokens": 500,
            "output_tokens": 20
        }));

        assert_eq!(tokens.cache_write_5m, 500);
        assert_eq!(tokens.cache_write_1h, 0);
        assert_eq!(tokens.total(), 530);
    }

    #[test]
    fn keeps_the_write_total_exact_when_the_breakdown_is_short() {
        let tokens = read_tokens(&json!({
            "cache_creation_input_tokens": 1000,
            "cache_creation": {
                "ephemeral_1h_input_tokens": 300,
                "ephemeral_5m_input_tokens": 200
            }
        }));

        assert_eq!(tokens.cache_write_1h, 300);
        assert_eq!(tokens.cache_write_5m, 700);
        assert_eq!(tokens.cache_write(), 1000);
    }

    #[test]
    fn does_not_add_thinking_tokens_to_output() {
        let tokens = read_tokens(&json!({
            "output_tokens": 204,
            "output_tokens_details": { "thinking_tokens": 180 }
        }));

        assert_eq!(tokens.output, 204);
    }

    #[test]
    fn reads_the_fast_mode_marker() {
        assert_eq!(read_speed(&json!({ "speed": "fast" })), Speed::Fast);
        assert_eq!(read_speed(&json!({ "speed": "standard" })), Speed::Standard);
        assert_eq!(read_speed(&json!({})), Speed::Standard);
    }

    fn write_session(dir: &Path, name: &str, lines: &[Value]) {
        std::fs::create_dir_all(dir).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(dir.join(name), body.join("\n") + "\n").unwrap();
    }

    fn assistant(session: &str, id: &str, request: &str, input: u64) -> Value {
        json!({
            "type": "assistant",
            "sessionId": session,
            "requestId": request,
            "uuid": format!("{session}-{id}-{request}"),
            "timestamp": "2026-09-08T04:00:00.000Z",
            "cwd": "/work/proj",
            "message": {
                "id": id,
                "model": "claude-opus-5",
                "usage": { "input_tokens": input, "output_tokens": 5 }
            }
        })
    }

    #[test]
    fn counts_one_call_written_across_several_content_block_lines_once() {
        let root = std::env::temp_dir().join(format!("aitu-claude-blocks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_session(
            &root.join("projects").join("-work-proj"),
            "s1.jsonl",
            &[
                assistant("s1", "msg_1", "req_1", 100),
                assistant("s1", "msg_1", "req_1", 100),
                assistant("s1", "msg_1", "req_1", 100),
            ],
        );

        let mut seen = HashSet::new();
        let scan = scan("h", &root, Clock::Utc, &mut seen);

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].tokens.input, 100);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn counts_a_call_replayed_into_a_forked_session_once() {
        let root = std::env::temp_dir().join(format!("aitu-claude-fork-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("projects").join("-work-proj");
        write_session(&dir, "s1.jsonl", &[assistant("s1", "msg_1", "req_1", 100)]);
        // A fork carries the same message under a new session id.
        write_session(
            &dir,
            "s2.jsonl",
            &[
                assistant("s2", "msg_1", "req_1", 100),
                assistant("s2", "msg_2", "req_2", 700),
            ],
        );

        let mut seen = HashSet::new();
        let scan = scan("h", &root, Clock::Utc, &mut seen);
        let total: u64 = scan.events.iter().map(|e| e.tokens.input).sum();

        assert_eq!(scan.events.len(), 2);
        assert_eq!(total, 800);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keeps_distinct_retries_of_one_message_apart() {
        let root = std::env::temp_dir().join(format!("aitu-claude-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_session(
            &root.join("projects").join("-work-proj"),
            "s1.jsonl",
            &[
                assistant("s1", "msg_1", "req_1", 100),
                assistant("s1", "msg_1", "req_2", 100),
            ],
        );

        let mut seen = HashSet::new();
        let scan = scan("h", &root, Clock::Utc, &mut seen);

        assert_eq!(scan.events.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn warns_instead_of_failing_when_there_are_no_sessions() {
        let mut seen = HashSet::new();
        let scan = scan("h", Path::new("/nonexistent/aitu"), Clock::Utc, &mut seen);

        assert!(scan.events.is_empty());
        assert_eq!(scan.warnings.len(), 1);
    }
}
