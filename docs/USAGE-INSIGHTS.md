# Usage insights: ideas and design

Status: proposed. Parts 1 to 4 are built; nothing else here is. Every route, dashboard element, stored column and file location below needs approval before it is built. When a part is built, its text moves to the page that owns the behavior and leaves this file.

Read at fork `8e66c70` and [cpa-usage-keeper](https://github.com/Willxup/cpa-usage-keeper) `e5e8f1a` (K in [Upstream review](UPSTREAM-REVIEW.md)). File and line references are to that fork commit.

## Why

The server records every request it sends, and the dashboard shows little of it. Three questions have no answer today:

1. Which providers and accounts earn their keep.
2. Which tools and skills do more harm than good.
3. What slows responses.

Cost in dollars is not wanted, so Keeper's price tables and cost panels are out.

## Goals and limits

| | |
| --- | --- |
| Goals | Answer questions 1 and 3 from the dashboard with body history off. Answer question 2 from stored history. |
| Not goals | Per-client-key views. Compatibility with Keeper, its login, backups or priority editing. Judging whether a tool's result was useful. Live streaming panels. |
| Must not happen | A request waits on any of this. A stored body is parsed on the request path. Prompts, outputs or tool results are copied out of body history. A body is re-serialized or replayed. |

## What existed before part 1

The fork had two stores. Neither answers the three questions alone.

| | Activity store (SQLite) | Body history (Parquet) |
| --- | --- | --- |
| On by default | Yes, except in Home mode without native routing | No, `observability.history.enable` |
| Fed by | Native Claude routing only (`crates/cpa-server/src/native_claude.rs:452`, `:1331`) | Every upstream send on both paths |
| Grain | One row per client request, keyed by request ID | One row per upstream send |
| Tokens | Input, output, cache read, cache write | The same, plus reasoning and 5-minute and 1-hour cache writes |
| Latency, time to first byte | Not stored | `latency_ms`, `ttft_ms`, timed from the send |
| Quota readings | 5-hour and weekly use per Claude account | None |
| Bodies | None | Both, up to 32 MiB each; WebSocket turns as lists of frames |
| Kept | 31 days, 250,000 requests, 50,000 quota readings (`native_claude/history.rs:16-18`) | Until `max-total-size-mb` |

Facts that shape the design:

- **The activity store sits beside native routing's event log while native routing runs, and otherwise opens in the state folder** (`start_default_activity` in `crates/cliproxy/src/main.rs`).
- **Its `requests` table is keyed by request ID and written with `INSERT OR REPLACE`** (`native_claude/history.rs:121`, `:375`), so a second row for the same request replaces the first.
- **Native events carry timings the store drops**, measured from the request's arrival and summed across failovers (`native_claude.rs:849`, `native_claude/send.rs:200`; `native_claude/observation.rs:458`). Body history times the same send from its own start (`crates/cpa-server/src/request_logging/history/native.rs:133`).
- **A translating-proxy request is measured only when the usage queue, body history or the activity store is on** (`crates/cpa-server/src/dispatch.rs:908`). Otherwise no usage tracker runs and no response is scanned for token counts.
- **Time to first byte depends on the route and provider.** Native routing measures the first response byte. On the translating proxy Claude, Codex, xAI, Kimi, Devin, Gemini, Vertex and Antigravity report the first upstream body byte, or over a WebSocket the first non-empty message; Meta reports the first byte of a whole answer and the first packet of a stream. AI Studio and OpenAI-compatible providers report nothing, so the figure is the first translated event or the response headers (`crates/cpa-server/src/usage_record.rs:2286`).
- **Stage timings exist for Claude only** on the translating proxy (`usage_record.rs:1299`), and they count from the request's arrival, not the send.
- **A quota-refused native request served by a fallback model** is recorded by native routing and sent through the translating proxy under a second ID (`native_claude/fallback.rs:151`).
- **Stored bodies hold the upstream form of tool names.** The Claude executor renames a client's tools to aliases keyed by the client key when it cloaks a request (`crates/cpa-exec/src/claude/alias.rs:4`), and the Codex translator shortens names over 64 bytes (`crates/cpa-translate/src/codex_names.rs:17`). Native rows keep the client's names.

The dashboard's Overview shows 24-hour, 7-day and 30-day native history: requests and failures, tokens by category, top models, and each account's quota use over time. Nothing reads tool calls for statistics.

## Ideas from Keeper

| Keeper panel | What it shows | Verdict |
| --- | --- | --- |
| Quota efficiency | Per Codex quota cycle: usage between observed drops in remaining percent | Adopt for Claude and Codex, in tokens. Part 2. |
| Cache read rate | `cache_read / input` over time, by model and account | Adopt with one definition for every provider. Part 2. |
| Model speed | `output * 1000 / latency` tokens per second, and decode speed `output * 1000 / (latency - ttft)` | Adopt both, with the limits in part 3. |
| Latency scatter | One point per request, time to first token against total, with 95th percentile lines | Adopt the percentiles, not the chart. It needs raw points and a chart library, and the dashboard's script budget is 64,000 bytes gzip (`ui/scripts/size.mjs:6`). Part 3. |
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
| Has quota readings and server wait times | Yes | No | Split across two files |
| Cost | A new table, a second feed, and usage tracking always on for the proxy | Scans gigabytes of bodies per page load | Two stores to join in every query |

Chosen: one activity store for both paths. It is the stated direction, and the only option that answers questions 1 and 3 with body history off.

**Where tool and skill facts come from.** Tool definitions, calls and results exist only inside bodies.

| | Background indexer | Command-line report | Documented queries |
| --- | --- | --- | --- |
| How | The server reads each finished history file once and keeps per-tool facts in an index | A command scans a date range and prints a table | DuckDB queries in the docs |
| Dashboard panel | Yes | No | No |
| Facts outlive the history size cap | Yes | No | No |
| Cost | A background job, a parser per body format, and deletes must reach the facts | The same parsers, run by hand | None |

Chosen: the background indexer, reading stored history only. Scanning every request as it passes was rejected: tool facts exist only while body history is on.

## Design

Four parts, built in order. Part 1 unlocks parts 2 and 3. Part 4 stands on body history and needs none of the others.

### Part 1: one activity store for every provider

Built. [Configuration](CONFIGURATION.md#activity-store) and [Native Claude](../native/README.md#diagnostics-and-history) own it. Facts of the built store that later parts rely on:

- The store's file stays beside the native routing event log while native routing runs, and is `activity.sqlite` in the server's state folder otherwise.
- The translating proxy writes no request row, and `requests` gained no columns. Each send stores `start_ms`, the time from its request's arrival to the send's start, so a request's total and its time in the server follow from its sends.
- On the translating proxy `send` numbers the request's attempts, counting on across bootstrap retry rounds. Body history numbers each request it sends on a counter of its own, so the two differ once an attempt resends its request or fails before sending.
- Quota readings come from response headers for Claude and Codex alike, on both paths, from the snapshots the executors keep. The native status loop and `account_status` are unchanged.
- With the store on, a 306 KB proxy request allocates 4.75 percent more (2,082 to 2,181 allocations) and a 1.9 MB one 1.67 percent more, with peak heap up under 0.2 percent.

### Part 2: which providers earn their keep

Built. [Configuration](CONFIGURATION.md#activity-store) owns the route's totals and quota cycles, and the [dashboard](../ui/README.md#pages) the Provider activity section on Overview. Facts of the built part that later parts rely on:

- Quota cycles cover the weekly window only. The 5-hour window has none.
- Tokens per percent counts exact sends only: the uncached input, cache write and output tokens of sends whose token counts are exact.
- The section has no speed columns; speed has its own section (part 3).

### Part 3: what slows responses

Built. [Configuration](CONFIGURATION.md#activity-store) owns the speed route, its time parts, percentiles and slow slices, and the [dashboard](../ui/README.md#pages) the Speed section on Overview. Facts of the built part that later parts rely on:

- The route is `GET /v8/management/observability/speed`, with the activity route's `range`, `provider` and `credential` and an `offset` in minutes east of UTC for the hour slice.
- Codex sends record the first upstream byte, as Claude's do, so new Codex rows have kind `byte`. Codex sends stored before that mark keep kind `event`, and rows of different kinds are never merged.
- AI Studio and OpenAI-compatible providers report no first-byte mark: their first response is the first translated event, kind `event`, and the Speed table marks those rows, as it does streamed Meta rows, timed by their first packet.
- On the translating proxy a send starts when its attempt starts, so its first response includes the executor's preparation before the upstream request, such as a WebSocket dial or token refresh.

### Part 4: tools and skills

Built. [Configuration](CONFIGURATION.md#tool-index) owns the tool index, its retention and the tools route, and the [dashboard](../ui/README.md#pages) the Tools page under Activity. Facts of the built part that later parts rely on:

- Only Claude Messages bodies (provider `claude`, native and proxy) are read. Codex, OpenAI Chat, Gemini and Antigravity sends count as skipped by format.
- Measures count conversations, not sends. A thread continuation joins its parent's conversation through the response message ID, so the index reads response bodies as well as requests.
- Tool, MCP server and skill names stay in `tool-index.sqlite` in the history folder, never in the activity store or the logs.
- Cloaked aliases are read back through the prefixes of the configured client keys and the default caller.

## Build order

1. The activity store for every provider. Built.
2. Providers table, health strip, quota cycles, cache read rate. Built.
3. Speed section. Built.
4. Tool and skill index and the Tools page. Built.

Each is its own change.

## Decisions that need approval

| Decision | Proposal |
| --- | --- |
| Label thresholds | The numbers in parts 3 and 4 are first guesses, to be tuned on real data. |

## Left to the builder

Module, table and file names, SQL, the split into Svelte components, and fixtures.

## Not verified

- **Codex WebSocket frames.** Which frame carries a finished `function_call` item was not read from a stored turn.
- **Other body formats.** Gemini and Antigravity tool calls carry no call ID and are not designed. Kimi and xAI are assumed to use the OpenAI Chat shape.
- **Costs.** Not measured: indexing a 512 MiB file or the index's size.
