# aitu

AI Token Usage — track what Claude Code and Codex sessions cost, across
several agent home directories.

Reads the agents' own session logs. Stateless: every run rescans, so there is
no cache to go stale and no stored state to get poisoned by a parser bug.
Scanning ~500 session files takes under a second.

```
$ aitu
aitu summary  (local days)

agent   sessions  calls  input  cache wr  cache rd  output  total       cost
------  --------  -----  -----  --------  --------  ------  -----  ---------
claude        90  25235   111k      108M     4.71B   17.2M  4.83B   $3635.71
codex          2     49   170k         0     5.65M   35.0k  5.85M        n/a
all           92  25284   282k      108M     4.71B   17.2M  4.84B  $3635.71+
```

## Commands

| Command         | Shows                                              |
| --------------- | -------------------------------------------------- |
| `summary`       | Totals per agent, and per home (the default)       |
| `days`          | Daily totals, most recent first                    |
| `projects`      | Totals per working directory                       |
| `models`        | Totals per model, and which models have no price   |
| `sessions`      | Totals per session, most expensive first           |
| `homes`         | Configured homes and what was found in each        |
| `init`          | Write a starter config file                        |

Flags, all global: `--home/-H NAME` (repeatable) to pick homes, `--agent/-a`
to pick one agent, `--since`/`--until` for a day range, `--limit/-n` for rows,
`--utc` to bucket days by UTC instead of the local timezone.

## Configuration

`$XDG_CONFIG_HOME/aitu/config.toml`, else `~/.config/aitu/config.toml`;
override with `AITU_CONFIG`. Run `aitu init` to write a starter file.

```toml
[[home]]
name   = "personal"
claude = "~/.claude"          # the directory holding projects/
codex  = "~/.codex"           # the directory holding sessions/

[[home]]
name  = "work"
codex = "~/.codex-work"    # either key may be omitted

# USD per million tokens. Only input and output are required; the cache rates
# default to 1.25x input (5-minute write), 2x (1-hour write) and 0.1x (read).
[prices."some-new-model"]
input      = 5.00
output     = 25.00
cache_read = 0.50
```

With no config file, one home named `default` is used, honouring
`CLAUDE_CONFIG_DIR` and `CODEX_HOME` and otherwise falling back to
`~/.claude` and `~/.codex`.

## Current model pricing

Built-in standard rates, verified September 21, 2026 (Opus 5.5: September 24, 2026), in USD per million tokens:

| Model | Input | Cache read | Cache write | Output |
| ----- | ----: | ---------: | ----------: | -----: |
| [Claude Fable 5.1](https://platform.claude.com/docs/en/models/fable-5-1/overview) | $10 | $0.25 | $12.50 (5m), $20 (1h) | $50 |
| [Claude Opus 5.5](https://platform.claude.com/docs/en/models/opus-5-5/overview) | $4 | $0.20 | $5 (5m), $8 (1h) | $20 |
| [GPT-6 Astra](https://developers.openai.com/api/docs/models/gpt-6-astra) | $10 | $1 | $12.50 | $50 |

Fable 5.1, Opus 5.5 and Astra usage is priced automatically (Opus 5.5 fast mode at
2x standard), including provider-prefixed
model names such as `openai/gpt-6-astra`. Astra prompts over 272,000 tokens
(including cached tokens) use 2x input/cache rates and 1.5x output rates for
the entire call. This is applied per call before report totals are summed.
Config overrides replace built-in pricing with fixed rates, including for
long prompts. Costs are API-rate estimates, not subscription charges;
Codex service-tier premiums and discounts are not applied.

## How the numbers are arrived at

Token accounting is easy to get subtly wrong, so the decisions are explicit:

- **One row per API call, deduplicated globally.** A Claude assistant message
  is written across several JSONL lines that each repeat the same cumulative
  `usage`; and forking or resuming a session replays earlier calls into a new
  file under a *new* session id. Calls are therefore keyed on message identity
  alone (`message.id` + `requestId`), across every file and every home. Keying
  on anything that includes the session id over-counts — measurably ~2.7% on a
  real month of logs.
- **Cache writes are split by TTL.** A 1-hour cache write costs 2x the input
  rate, a 5-minute write 1.25x. The split lives in Claude's nested
  `usage.cache_creation` object; `cache_creation_input_tokens` is the
  authority on the sum, and anything the breakdown doesn't account for is
  treated as a 5-minute write.
- **Reasoning tokens are not added to output.** For both agents they are a
  *breakdown* of `output_tokens`, not an extra category (Codex records satisfy
  `input + output == total_tokens`). Adding them double-bills reasoning.
- **Days are bucketed in one timezone, the same for both agents.** Slicing
  `YYYY-MM-DD` off the raw timestamp buckets by UTC; at UTC+8 that puts ~20%
  of calls on the wrong day. `--utc` if you want the raw behaviour.
- **Codex is read from `token_usage_record`** — one per response, carrying a
  `response_id` and a per-response delta. Older rollouts that only have the
  cumulative `event_msg`/`token_count` event fall back to differencing
  consecutive totals. A rollout with both is read from the former only.
- **An unpriced model is a gap, never zero.** Unknown models are listed
  explicitly, their totals show `n/a`, and a partly-priced total is labelled a
  lower bound. Fill the gap with a `[prices."..."]` entry.
- **Claude fast mode is priced separately**, from the `usage.speed` marker.
- **`total` is a weak spend proxy.** It is dominated by cache reads, the
  cheapest tokens there are. "freshly processed" (everything but cache reads)
  and the cost column are the meaningful figures.

Only usage metadata is read. Prompt and response bodies, tool arguments and
shell commands are never parsed; Codex reading is scoped to the `sessions/`
subtree, so `history.jsonl` at the codex home root is left alone.

## Caveats

- Older OpenAI/Codex prices in the built-in table, other than GPT-6 Astra,
  are **not verified** against OpenAI's published pricing. Override them if
  the dollar figures matter.
- Recent Codex versions also index threads in SQLite (`state_5.sqlite`).
  Only the JSONL rollouts under `sessions/` are read; a thread that exists
  solely in SQLite would be invisible.
