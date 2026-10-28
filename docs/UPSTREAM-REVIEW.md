# Upstream port queue

Reviewed **2026-10-25**, against fork `fa0cd8b5a4ef61474500d04885334fc07d15ec30`.
These are inclusive boundaries for screened default-branch history, not merge or full-audit claims.

| Source | Branch | Reviewed through |
| --- | --- | --- |
| G — [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | `main` | `307803510bce87f986fcf7b1a84190f105c7b4aa` |
| R — [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs) | `master` | `0ccf7e4da7676994a8f342d7de5deb6f1362123c` |
| S — [sub-api-proxy](https://github.com/DarkEden-coding/sub-api-proxy) | `main` | `b9c875c3d9060a55f9caf377d6b87d1ffc4b0008` |

## Completed

- **U07, U01, U03 — `cc3ffc1`**: test catalog isolation, Claude OAuth retry safety,
  RESP limits. Independently reviewed; 47 focused tests passed again on 2026-10-28.
- **U02 + U14 credential locking — `28f94dc`**: preserve refresh results across
  metadata edits, fence replaced identities, retain credentials on incomplete scans,
  and move disk writes outside the selection lock. On 2026-10-28, 106 focused tests,
  formatting and clippy passed. U14 config concurrency remains below.
- **U08 core — `67fbb4d`**: iterative GoValue decode/marshal/drop and built-in
  walkers from R `541b8e47`/`e563f51a`, plus iterative clone/equality. Go's 10000-depth
  limit and wire semantics are covered; 261 tests, formatting and clippy passed on
  2026-10-28. Remaining recursive paths are split below.

## Pick up next

**Next: U08b**, then U08c and U04. Source prefixes below refer to the repositories
above. Preserve the acceptance boundaries when splitting work.

| ID | Work and acceptance boundaries | Starting commits |
| --- | --- | --- |
| U08b | Remove AnyValue conversion/marshal/drop recursion; conversion at depth 10000 still overflows. Preserve gjson's first-duplicate-key, permissive input, invalid UTF-8 and nonfinite-number behavior. Include small-stack regressions and translator fixtures. | Local `cpa-common/src/json.rs`; not covered by R `541b8e47`/`e563f51a` |
| U08c | Remove remaining recursive GoValue walkers in Codex namespace restoration, Gemini/Kimi schema processing and Antigravity replay normalization. Audit serde conversion/debug boundaries separately; add deep-input tests at the consuming paths. | Local `codex_client.rs`, `gemini_schema.rs`, `kimi.rs`, `antigravity_replay.rs` |
| U04 | Reauthorize established Responses WebSockets after client-key changes, including normal/duplex turns; retain the bound principal. | G `b9bb031d`, `3e7a21e2`, `6340fe43` |
| U05 | Split incomplete Claude streams, relay cancellation, AI Studio event framing and Codex multiline SSE. Preserve partial output and terminal errors; never replay ambiguous inference. | G `4f2e5270`, `30780351`, `0fe4cc2c`, `90fe5b23`; R `dcb7336b`, `1639d8d6`, `24c32be3` |
| U06 | Separate internal classification headers from downstream non-direct errors and stop bootstrap retry loops. Keep direct headers and scheduler Retry-After. | R `b5ae1ea3` **plus** `7e76e295` |
| U09 | Respect explicit tool concurrency and context-window stop reasons; split request and response changes. Review actual prompt-overflow errors separately. | G `99d1698f`, `c2dfec28`, `d495e24c`, `43f27e3a` |
| U10 | Separate per-minute throttling from subscription exhaustion. Independently assess connection-setup timeouts; retain fresh quota checks and native CONNECT's no-retry policy. | R `b2ea411e`; G `db1bab01`, `345cc9d5`, `b3dc6c3f` |
| U11 | Make wildcard payload edits linear and cache tool-name mappings per stream. Preserve untouched bytes and measure large-body scaling. | R `34a2dab6`; G `95e15777`, `74e7ff60`, `83189e5f`, `f4a2d040`, `3b153145` |
| U12 | Restore fully wrapped MCP tool names on translated Claude requests; keep ambiguous-name rejection. | R `954e6e76`; G `02fc76b1` |
| U13 | Port schema cleanup and translated arguments together. Include later Go fixes so implicitly open empty objects retain real arguments; cover reference siblings and array items. | R `0ccf7e4d`, `81afb744`; G `e1fb2521`, `29150a22`, `b2fda98e` |
| U14 config | Add ETag/If-Match concurrency checks without the unrelated management bundle; preserve byte-preserving config edits. Independent of credential locking. | R `49c23c50`; G `fa68946a` |
| U15 | Harden request-log filename components across platforms; keep the shared redacting sink. | G `179d43dc` |
| S01 | Adapt explicit model-entitlement error classification on translating paths. Native handling requires separate bounded inspection, unchanged bytes and model-scoped cooldowns. | S `84d9e570` |

## Handoff

- Before porting, reconcile this queue with current code. Use focused regressions,
  private auth directories and local mock upstreams. Move completed IDs above with
  their fork commit and validation; preserve unfinished parts of split items.
- Keep native Claude CLI/Desktop/Zed fidelity and built-in providers. Omit plugin
  and distributed-product portions of mixed commits. Existing scope is in
  [PARITY.md](PARITY.md) and [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).
- Next upstream review: fetch separate review clones, verify each saved hash is an
  ancestor of its new head, then screen `saved-hash..new-head`. Investigate rewritten
  history first. Advance a boundary only after its full range is screened; pending
  ports survive checkpoint advancement.
- Speculative features and deferred product ideas are intentionally omitted. A
  completed port does not imply installation, publishing or live-provider testing.
