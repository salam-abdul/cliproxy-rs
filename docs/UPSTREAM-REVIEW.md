# Upstream port queue

Reviewed **2026-10-25**, against fork `fa0cd8b5a4ef61474500d04885334fc07d15ec30`.
These are inclusive boundaries for screened default-branch history, not merge or full-audit claims.

| Source | Branch | Reviewed through |
| --- | --- | --- |
| G — [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | `main` | `307803510bce87f986fcf7b1a84190f105c7b4aa` |
| R — [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs) | `master` | `0ccf7e4da7676994a8f342d7de5deb6f1362123c` |
| S — [sub-api-proxy](https://github.com/DarkEden-coding/sub-api-proxy) | `main` | `b9c875c3d9060a55f9caf377d6b87d1ffc4b0008` |

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

## Pick up next

The reviewed port queue is complete. Start the next upstream review from the saved
hashes above; no additional upstream history was screened during implementation.

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
