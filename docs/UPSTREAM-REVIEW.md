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
  formatting and clippy passed.
- **U08 core — `67fbb4d`**: iterative GoValue decode/marshal/drop and built-in
  walkers from R `541b8e47`/`e563f51a`, plus iterative clone/equality. Go's 10000-depth
  limit and wire semantics are covered; 261 tests, formatting and clippy passed on
  2026-10-28. Remaining recursive paths are split below.
- **U08b — `ca733eb`**: iterative AnyValue conversion/marshal/drop/clone/equality
  and Claude-to-OpenAI tool-schema normalization. Deep and malformed inputs retain
  gjson behavior; 60000 generated cases preserved the previous output bytes. On
  2026-10-28, 133 tests, formatting and clippy passed.
- **U14 config + U15 — `1962b30`**: conditional config writes, atomic management
  saves, cross-platform log paths and `value` query redaction. On 2026-10-29,
  61 focused control-plane tests and the management routing golden passed.
- **U11, U12 + U09 requests/responses — `8fbb788`**: batched element-local
  wildcard payload edits, cached tool mappings, wrapped MCP-name restoration, explicit tool
  concurrency and context-window stop reasons. Gemini tool caching was already
  present; its behavior was verified.
- **U13 + U08c walkers — `8856586`**: bounded shared reference expansion, final
  schema/argument semantics, indexed argument edits and iterative consuming
  walkers. Ambiguous argument paths preserve the original document, as recorded
  in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md). Schema batching remains below.
- **U04, U05, U06 + split admission fixes — `42af939`**: per-turn WebSocket
  authorization, stream completion/framing/cancellation, error-header isolation
  and no replay after locally detected incomplete responses. Includes U09 prompt
  overflow, U10 per-minute throttling and S01 translating-path entitlement errors.
  Connection setup and native entitlement handling remain below.

Validation on **2026-10-29**: 318 selected executor tests, 145 server unit tests,
55 protocol/admission integration tests and 61 control-plane tests passed.
The common/translate suites passed, including 11224 translator vectors and
2079 final-Go schema outputs. Affected-crate clippy, CLI checks and formatting
passed. These are focused checks, not a whole-workspace run.

## Pick up next

Only unfinished work is listed. Source prefixes refer to the repositories above.

| ID | Work and acceptance boundaries | Starting points |
| --- | --- | --- |
| U08c — schema batching | Remove repeated whole-schema path scans/rebuilds from the legacy cleaner using an indexed document and batched edits. Preserve the 2079 Go outputs, complex-path behavior and consuming-path depth tests; add bounded deep/wide scaling regressions. Ordinary walkers, reference expansion and serde/debug boundary audit are complete. Do not port the 1 GiB stack workaround. | Local `cpa-common/src/gemini_schema.rs`, `schema_path.rs`; R `541b8e47`, `e563f51a`, `0ccf7e4d` |
| U10 — connection setup | Assess bounded connection-setup settings separately from the completed per-minute fix. Prove timeout/retry behavior occurs before request-body transmission. Retain fresh quota checks and native CONNECT's no-retry policy. | R `b2ea411e`; G `345cc9d5`, `b3dc6c3f` |
| S01 — native entitlement | Reconcile concurrent native changes before adding bounded error inspection and account/model-scoped cooldowns. Preserve response bytes and no replay. Translating-path classification is complete. | S `84d9e570`; local `native_claude/` |

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
