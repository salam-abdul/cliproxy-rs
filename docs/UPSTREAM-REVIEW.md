# Upstream port queue

Initial review **2026-10-25**, against fork `fa0cd8b5a4ef61474500d04885334fc07d15ec30`.
These are inclusive boundaries for screened default-branch history, not merge or full-audit claims.

| Source | Branch | Reviewed through |
| --- | --- | --- |
| G — [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | `main` | `dbb706215a3afd2f0ea3c96482d4233856466caa` |
| R — [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs) | `master` | `b7ab292f47c61263da16d0e07358f492ad4dabf4` |
| S — [sub-api-proxy](https://github.com/DarkEden-coding/sub-api-proxy) | `main` | `d4c10513147821bafe99315ad520e265d46dfe69` |

## Completed ports

Implementation details live in the resolving commits.

| IDs / completed portion | Fork commit | Result |
| --- | --- | --- |
| U07, U01, U03 | `cc3ffc1` | Isolated test catalogs, safe OAuth retry, RESP limits. |
| U02 + U14 credentials | `28f94dc` | Preserve rotated credentials across edits and reloads. |
| U08 core | `67fbb4d` | Stack-safe GoValue operations and built-in walkers. |
| U08b | `ca733eb` | Stack-safe AnyValue operations and Claude tool-schema normalization. |
| U14 config + U15 | `1962b30` | Conditional atomic config saves, log-path safety and query redaction. |
| U11, U12 + U09 requests/responses | `8fbb788` | Payload batching, tool-name caching/restoration, concurrency and context stops. |
| U13 + U08c walkers | `8856586` | Bounded references, schema/argument cleanup and iterative consuming walkers. |
| U04, U05, U06 + U09 server, U10 rate limits, S01 translating | `42af939` | WebSocket reauthorization, stream/error delivery safety and scoped admission fixes. |
| U08c — schema batching | `4e33741` | Indexed ordinary/complex schema passes, structural merges and batched edits; 18 focused regressions passed. |
| U10 — setup | `77b50f7` | Setup-only timeouts, reload-isolated transports and pre-send-only retry; 480 executor tests and 2 integration tests passed. |
| S01 — native | `a8b825d` | Bounded rejection inspection and account/model-scoped entitlement cooldowns; 35 native tests passed. |

Latest verification: **2026-10-29**. All 480 executor tests, 18 schema regressions,
35 native tests and 58 selected server integration tests passed, along with the
core/translation suites, workspace clippy and formatting. This was not a
whole-workspace test run or installation qualification.

Schema rewrites treat property names literally and use the last duplicate value.
Complex rewrites may compact JSON or reorder `required` entries. Passthrough bytes
remain unchanged. Connection setup timeouts are opt-in; native CONNECT retains its
existing no-replay behavior.

Cleanup `562303d` removed Go sources/generators, differential and historical
benchmark tooling, bulk parity corpora and obsolete docs. Focused Rust regressions
remain. `docs/` contains configuration, installation and this checkpoint.

## Review of 2026-10-31

The saved hashes were ancestors of all three source heads. Compared with fork
`a4e81c0` and this dashboard update. Screened
`3078035..dbb7062` (89 Go commits), `0ccf7e4..b7ab292` (48 Rust commits) and
`b9c875c..d4c1051` (3 sub-api-proxy commits). This was a commit-range screen
with focused source comparisons, not a full source audit or a port. No live
provider calls or installation qualification were run.

| Candidate | Source commits | Fork disposition |
| --- | --- | --- |
| Disable the AI Studio WebSocket in Home mode | G `a6b4477` | Priority authorization fix: the fork registers `/v1/ws` in Home mode while `home::overlay` disables `ws-auth` and clears local API keys. An available Home node accepts an unauthenticated relay connection. |
| Bound pre-auth request capture and zstd log decoding | G `477605d`, `ad8bdd3` | Priority safety fix: `request_logging::capture` can eagerly read a body before client authentication, and its decode has no output limit. Preserve raw wire bytes while bounding capture. |
| Preserve Claude tool-result errors | G `81de4b9`, `068fe15` | Port to Gemini and Antigravity translators: the fork currently writes `response.result` even when `tool_result.is_error` is true. |
| Classify SIWC credentials as OAuth | R `77267c1` | Port the `use_siwc: true` precedence before API-key classification; cover mixed metadata. |
| Reconcile stream terminal and retry rules | G `aa4130e`, `f013eb6`, `86a1aab`; R `80977ac`, `186ee6e` | Compare with the fork's existing truncation checks. Retain partial usage; never cool a credential for a locally detected truncation or replay after output. |
| Preserve Claude and Codex conversation features | G `12d97cf`, `dbb7062`, `1d45a5f` | Evaluate mid-conversation `tool_addition`, recognized compaction capsules and forced-tool restrictions with synthetic native-client fixtures. |
| Codex credential recovery | G `514d09f`, `43068c8` | Consider targeted re-sign-in and sanitized terminal-renewal status. Check relative auth-dir cooldown paths first; do not add automatic inference verification. |
| Usage and dashboard history | G `89e472f` through `82deffb`, S `385b972`; R `7c3a83e` | Reconcile structured usage and speed metrics with the fork's durable history. Quota-window retention is present in the dashboard; review its 403 guidance. |
| WebSocket late interrupt | S `d4c1051` | Applies only if local HTTP-turn interrupt support is added; no matching fork control path exists now. |

Plugin host, packaged releases, CI, Go parity fixtures, benchmarks, discovery,
and unrelated dashboard branding stay outside this fork's scope. The reviewed
boundaries advance despite unfinished candidates; preserve this queue until each
is resolved or explicitly dropped.

## Pick up next

Start with the Home WebSocket gate and bounded request capture, then Claude
tool-result semantics and SIWC classification. Reconcile the other candidates
against current code before porting. The next upstream review starts from the
saved hashes above.

## Handoff

- Before porting, reconcile this queue with current code. Use focused Rust regressions,
  private auth directories and local mock upstreams; no Go parity infrastructure.
  Record completed IDs with their fork commit and validation; preserve unfinished
  parts of split items.
- Keep native Claude CLI/Desktop/Zed fidelity and built-in providers. Omit plugin
  and distributed-product portions of mixed commits. Scope and operational setup
  are in [README](../README.md) and the [native guide](../native/README.md).
- Next upstream review: fetch separate review clones, verify each saved hash is an
  ancestor of its new head, then screen `saved-hash..new-head`. Investigate rewritten
  history first. Advance a boundary only after its full range is screened; pending
  ports survive checkpoint advancement.
- Speculative features and deferred product ideas are intentionally omitted. A
  completed port does not imply installation, publishing or live-provider testing.
