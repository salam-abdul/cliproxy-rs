# Usage insights: ideas and design

Status: proposed. Nothing here is built. Every route, dashboard element, stored column and file location below needs approval before it is built.

Read at fork `8e66c70` and [cpa-usage-keeper](https://github.com/Willxup/cpa-usage-keeper) `e5e8f1a` (K in [Upstream review](UPSTREAM-REVIEW.md)).

## Why

The server records every request it sends, and the dashboard shows little of it. Three questions have no answer today:

1. Which providers and accounts earn their keep.
2. Which tools and skills do more harm than good.
3. What slows responses.

Cost in dollars is not wanted. Keeper's price tables and cost panels are out.

## Goals and limits

| | |
| --- | --- |
| Goals | Answer questions 1 and 3 from the dashboard with body history off. Answer question 2 from stored history. |
| Not goals | Dollar cost. Per-client-key views. Compatibility with Keeper, its login, backups or priority editing. Judging whether a tool's result was useful. Live streaming panels. |
| Must not happen | A request waits on any of this. A body is scanned on the request path. Prompts, outputs, tool arguments or tool results reach the always-on store. A body is re-serialized or replayed. |

## What exists today

The fork has two stores. Neither answers the three questions alone.

| | Activity store (SQLite) | Body history (Parquet) |
| --- | --- | --- |
| On by default | Yes, when native routing runs | No, `observability.history.enable` |
| Fed by | Native Claude routing only (`crates/cpa-server/src/native_claude.rs:452`, `:1331`) | Every upstream send on both paths |
| Grain | One row per client request | One row per upstream send |
| Tokens | Input, output, cache read, cache write | The same, plus reasoning and 5-minute and 1-hour cache writes |
| Latency, time to first byte | Not stored | `latency_ms`, `ttft_ms` |
| Quota readings | 5-hour and weekly use per Claude account | None |
| Bodies | None | Both, up to 32 MiB each |
| Kept | 31 days, 250,000 requests (`native_claude/history.rs:16-17`) | Until `max-total-size-mb` |

Three facts shape the design:

- **The activity store starts inside native routing** (`native_claude.rs:1279`) and lives beside its event log. With native routing off there is no store.
- **Native events already carry timings** that the store drops: preparation, session wait, admission and upstream headers (`native_claude.rs:1065`), first byte and total (`native_claude/observation.rs:458`).
- **A translating-proxy send is measured only when the usage queue or body history is on** (`crates/cpa-server/src/usage_record.rs:2152`). Its record then has provider, model, tokens, latency, time to first byte and stage timings (`usage_record.rs:1305`).

The dashboard's Overview shows 24-hour, 7-day and 30-day native history: requests and failures, tokens by category, top models, and each account's quota use over time. Nothing reads tool calls for statistics.

## Ideas from Keeper

| Keeper panel | What it shows | Verdict |
| --- | --- | --- |
| Quota efficiency | Per Codex quota cycle: usage between observed drops in remaining percent | Adopt for Claude and Codex, in tokens, not dollars. Part 2. |
| Cache read rate | `cache_read / input` over time, by model and account | Adopt with one definition for every provider. Part 2. |
| Model speed | `output * 1000 / latency` tokens per second, and decode speed `output * 1000 / (latency - ttft)` | Adopt both formulas. Part 3. |
| Latency scatter | One point per request, time to first token against total, with 95th percentile lines | Adopt the percentiles, not the chart. It needs raw points and a chart library, and the dashboard's script budget is 56,800 bytes gzip (`ui/scripts/size.mjs:6`). Part 3. |
| Service health grid | Time blocks colored by success rate, with steps at 50, 65 and 80 percent | Adopt per provider. Part 2. |
| Request table | Per-request time to first token, latency, speed and cache rate | Later. Activity already lists attempts; add columns once the store has them. |
| Token share ribbons | Share of tokens by model and account | Extend the existing top-models panel with a provider split. Part 2. |
| Activity heatmap, daily averages | Requests by hour and day | Skip. They answer none of the three questions. |
| Cost and pricing | Price rules per model, cost by model, key and provider | Rejected: cost is not wanted. |
| Per-key views, login, OIDC, backups, priority editing, Redis queue | Operating a shared service | Rejected: the fork is a private single-operator server. |

Keeper has nothing on tools, skills or where time goes inside the proxy. Parts 3 and 4 go past it.

## Approaches

**Where the numbers come from.**

| | One activity store for both paths | Panels read body history | A second store for the proxy |
| --- | --- | --- | --- |
| Works with body history off | Yes | No | Yes |
| Has quota readings and proxy wait times | Yes | No | Split across two files |
| Cost | A schema change and a second feed | Scans gigabytes of bodies per page load | Two stores to join in every query |

Chosen: one activity store for both paths. It is the stated direction, and the only option that answers questions 1 and 3 with body history off.

**Where tool and skill facts come from.** Tool definitions, calls and results exist only inside bodies.

| | Background indexer | Command-line report | Documented queries |
| --- | --- | --- | --- |
| How | The server reads each closed history file once and keeps per-tool facts in the activity store | A command scans a date range and prints a table | DuckDB queries in the docs |
| Dashboard panel | Yes | No | No |
| Facts outlive the history size cap | Yes | No | No |
| Cost | A background job, a parser per body format, a backfill, and deletes must reach the facts | The same parsers, run by hand | None |

Chosen: the background indexer, reading stored history only. Scanning every request as it passes was rejected: tool facts are produced only while body history is on.

## Design

Four parts, built in order. Part 1 unlocks parts 2 and 3. Part 4 depends only on part 1's store.

### Part 1: one activity store for every provider

**Ownership.** The store moves out of `native_claude/history.rs` into a server-level module that the runtime owns and starts with the server. Native routing becomes one of its two feeds.

**Grain.** One row per upstream send on both paths, as body history has. Each row names its client request and its send number, and marks the send that ended the request. Request-level panels count the ending sends; provider panels count every send, because each one landed on one account. A request refused before any send (no quota, no account) stays one row with no account.

**New columns on `requests`**, all nullable so existing rows stay valid:

| Column | Meaning |
| --- | --- |
| `source` | `native` or `proxy` |
| `provider` | `claude`, `codex`, and so on |
| `credential` | The credential file ID, the same key on both paths. `account` stays for native rows. |
| `exchange`, `turn`, `send`, `last` | The client request, its WebSocket turn, the send number within it, and whether this send ended it. The first three match body history's `exchange_id`, `turn` and `send`, so part 4 can find a send's row. |
| `latency_ms`, `ttft_ms` | Start of the send to its end, and to its first response byte |
| `proxy_wait_ms` | Time inside the server before the send went out |
| `reasoning_tokens`, `stream` | As body history records them |

**Token columns mean the same for every provider.** `input_tokens` is uncached input. The translating proxy already separates total, uncached, cache-read and cache-write input (`usage_record.rs:31`); the feed stores the uncached figure.

**Feeds.**

- Native routing: as today, plus one row per failover send, and the timings it already computes. `proxy_wait_ms` is preparation plus session wait plus admission.
- Translating proxy: the store becomes a third consumer where an attempt is settled (`usage_record.rs:2145`), so the record is built whenever the store runs. `proxy_wait_ms` is `upstream_started_ms`.

**Quota readings.** `account_status` keeps Claude's 5-hour and weekly use. Codex readings join it: the executor already keeps each account's latest quota headers (`crates/cpa-exec/src/codex_quota.rs:427`), and a reading is recorded when a response changes it, at most once per time bucket. Other providers report no quota, so their accounts show work, reliability and speed only.

**Unchanged contracts.**

- Writes never block a request: the bounded queue and `try_send` stay, and dropped rows are counted in `lost_events`.
- The store holds no session IDs, prompts or outputs.
- Retention stays 31 days. The row cap is an open decision below: per-send rows for every provider fill 250,000 sooner.

**Route.** One new management route returns the store's panels for a range, with optional provider and credential filters. `observability/native/history` (`crates/cpa-server/src/management.rs:470`) keeps answering until the dashboard has moved, then goes.

### Part 2: which providers earn their keep

Measured per provider, and per account under it, over the selected range:

| Group | Measure | Definition |
| --- | --- | --- |
| Work | Sends, output tokens, and each one's share of the total | Counts over rows |
| Reliability | Failure rate | Sends that did not complete, excluding client cancels, over sends |
| | Failovers away | Sends that ended in a failover |
| | Time cooling | Share of quota readings in which the account was cooling. Native Claude accounts only at first. |
| Quota | Peak use per cycle | Highest reading among those that share a reset time |
| | Unused at reset | 100 minus the last reading before the reset |
| | Tokens per percent | Tokens sent in the cycle over the rise in use across it. A cycle first seen partway is marked partial, as Keeper does. |
| Cache | Cache read rate | `cache_read / (input + cache_read + cache_write)` |
| Speed | Median time to first byte, tokens per second | From part 3 |

There is no single score. An account that serves little, leaves most of its weekly quota unused or fails often is visible in its row, and a composite number would hide which of the three it is.

**Dashboard.**

- Overview gains a Providers table: one row per provider, opening to its accounts, with the measures above. It sorts by unused weekly quota.
- A health strip per provider: one block per time bucket, colored by success rate with Keeper's steps at 50, 65 and 80 percent.
- The existing history panels gain a provider filter, and top models gains a provider split.

### Part 3: what slows responses

Each send's time splits into three stored parts, and a fourth derived one:

| Part | Stored as | Native routing | Translating proxy |
| --- | --- | --- | --- |
| Waiting in the server | `proxy_wait_ms` | Preparation, session wait and quota admission | Accepting the request to starting the send |
| Upstream until first byte | `ttft_ms` | First response byte | First response byte |
| Generation | `latency_ms - ttft_ms` | | |
| Earlier sends of the same request | Sum over sends before the last | Failovers | Retries and failovers |

Speed uses Keeper's two formulas: tokens per second is `output * 1000 / latency_ms`, and decode speed is `output * 1000 / (latency_ms - ttft_ms)` when time to first byte is below latency. A send with no output tokens or no latency has no speed.

Percentiles are computed in SQL when the page asks, over the rows in range.

**Slices.** Provider, model, account, request size (uncached input plus cache tokens: under 10k, 10k to 50k, 50k to 200k, over 200k), cache hit or miss, streamed or not, and hour of day.

**Dashboard.** A Speed section on Overview:

- A table by provider and model: median and 95th percentile time to first byte and total time, and tokens per second.
- One stacked bar per provider showing the time split.
- A short list of slow slices: any slice with at least 30 sends whose 95th percentile time to first byte is 50 percent above the overall figure.

### Part 4: tools and skills

Facts come from stored body history only. With `observability.history.enable` off the panel says so and shows nothing. With it on, the panel names the dates it covers.

**What is read.** Calls come from response bodies and results from the request that follows, so each is seen once:

| Format | Definitions | Calls | Results |
| --- | --- | --- | --- |
| Claude Messages | `tools[]`: name, and the element's size in bytes | `tool_use` blocks in the response: ID and name | `tool_result` blocks in a request: ID, `is_error`, content size |
| OpenAI Responses (Codex) | `tools[]` function entries | `function_call` items: call ID and name | `function_call_output` items: call ID, output size. No error flag. |
| OpenAI Chat | `tools[].function` | `tool_calls[]`: ID and name | `tool` messages: ID, content size. No error flag. |

A call is counted once by its ID. A skill is a call to the `Skill` tool; its name is the one argument the indexer reads, and the size of its result is what the skill loads into the conversation. Tool names of the form `mcp__<server>__<tool>` roll up by server. No other argument and no result content is read or stored.

**Stored facts**, in the activity store:

- One row per tool call: day, provider, tool, a hash of its call ID, error flag, result size.
- One row per day, provider and tool for definitions: the number of sends that carried it and their summed size.
- On each send's `requests` row: how many tools it carried, their summed definition size, and the tool-result bytes in its request. This lets part 3 slice time to first byte by tool-result size.
- The list of history files already indexed.

**The indexer.**

- Runs on its own thread, never on the request path or an async worker. It writes through the store's existing writer.
- Is told of a file when the history writer closes it (`crates/cpa-server/src/request_logging/history/writer.rs:571`). At start it catches up on files not yet indexed, newest first, one at a time.
- Reads one row at a time. `read_rows` loads a whole file (`request_logging/history/file.rs:598`), which can be 512 MiB, so the indexer needs a reader that yields rows one by one. Memory is bounded by one row's two bodies, at most 64 MiB.
- Walks each body once.
- Marks a send whose body was cut at the 32 MiB cap as partly covered.

**Deletes.** `cliproxy history delete` removes rows by time, session, account or model (`request_logging/history/delete.rs:90`), also while the server is stopped. The facts are derived, so this must hold: once the server next runs, no fact from a deleted row remains. A delete drops the facts of every day it touched and re-indexes those days from the files that remain. The indexed-file list records enough about each file to notice one that was rewritten or removed while the server was down.

Files the server itself removes under the size cap are different: their facts stay until the store's 31 days pass. They hold names, counts and sizes.

**Measures per tool**, over the range:

| Measure | Definition |
| --- | --- |
| Carried | Sends whose request defined the tool, and their share of all indexed sends |
| Weight | Mean definition size in bytes |
| Use | Calls, and calls per 100 sends that carried it |
| Failure | Error results over calls. Claude format only. |
| Result size | Mean and 95th percentile result bytes, and the total |

**Labels, not a score.** A tool gets a label when a rule holds:

| Label | Rule |
| --- | --- |
| Dead weight | Carried by at least 20 percent of sends, with no call in the range |
| Heavy | In the top tenth by definition size times sends carrying it |
| Failing | At least 10 calls, with 20 percent or more ending in an error |
| Bloating | 95th percentile result of 100 KB or more |

The server cannot tell whether a result helped. The panel says so.

**Dashboard.** A Tools tab under Activity: one row per tool with the measures and labels, grouped by MCP server, filtered by provider, with skills in their own table.

## Build order

1. The activity store for every provider: module move, columns, both feeds, Codex quota readings, the route.
2. Providers table, health strip, quota cycles, cache read rate.
3. Speed section.
4. Tool and skill indexer, then its tab.

Each is its own change with its own tests and doc updates: `docs/CONFIGURATION.md` for history and the store, `native/README.md` for native state, `ui/README.md` for pages.

## Decisions that need approval

| Decision | Proposal |
| --- | --- |
| Where the store's file lives when native routing is off | Keep today's path when native routing is configured, so existing rows carry over. Otherwise a file in the server's own state folder. No setting. |
| Row cap | Raise 250,000 to 1,000,000. The page already reports the oldest row it has, so a shorter reach stays visible. |
| The new management route, and when the native one goes | One route for the store's panels; the native route is removed in the change that moves the dashboard. |
| Dashboard elements | Providers table, health strip, Speed section, Tools tab. Each must fit the bundle budgets. |
| Recording Codex quota readings | As described in part 1. |
| Whether the indexer has its own switch | No new setting: it runs whenever body history is on. Revisit after measuring it on real files. |
| Label thresholds | The numbers in parts 3 and 4 are first guesses, to be tuned on real data. |

## Left to the builder

Module and table names, SQL, the route's JSON shape, the split into Svelte components, fixtures, and how the per-row reader is written.

## Not verified

- **Skills on the wire.** That a skill appears as a `Skill` tool call with its name in `input.skill` is assumed, not read from a stored request.
- **Codex quota resets.** The Codex reading is a map of response headers. Which header carries the reset time was not read.
- **Proxy wait on the translating proxy.** `upstream_started_ms` runs from accepting the request to starting the send; whether that matches the native figure closely enough to share a column is untested.
- **Other body formats.** Gemini and Antigravity tool calls carry no call ID and are not designed here. Kimi and xAI are assumed to use the OpenAI Chat shape.
- **Indexing cost.** No file was indexed. The time to parse a 512 MiB file and its effect on a busy server are unmeasured.
- **Calls with no later request.** A call in the last response of a session is counted, but its result never arrives, so its error flag and result size stay empty.
