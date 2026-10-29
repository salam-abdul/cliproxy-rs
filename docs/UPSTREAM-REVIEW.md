# Upstream port queue

Reviewed **2026-10-25**, against fork `fa0cd8b5a4ef61474500d04885334fc07d15ec30`.
These are inclusive boundaries for screened default-branch history, not merge or full-audit claims.

| Source | Branch | Reviewed through |
| --- | --- | --- |
| G — [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | `main` | `307803510bce87f986fcf7b1a84190f105c7b4aa` |
| R — [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs) | `master` | `0ccf7e4da7676994a8f342d7de5deb6f1362123c` |
| S — [sub-api-proxy](https://github.com/DarkEden-coding/sub-api-proxy) | `main` | `b9c875c3d9060a55f9caf377d6b87d1ffc4b0008` |

## Completed ports

Implementation details live in the resolving commits. Partial IDs
retain their unfinished work below.

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

Latest focused verification: **2026-10-29**. Executor, server, common/translate,
reference fixtures, CLI checks, clippy and formatting passed. This was not a
whole-workspace run or installation qualification. Intentional deviations are in
[DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).

## Pick up next

Only unfinished work is listed. Source prefixes refer to the repositories above.

| ID | Work and acceptance boundaries | Starting points |
| --- | --- | --- |
| U08c — schema batching | Remove repeated whole-schema path scans/rebuilds from the legacy cleaner using an indexed document and batched edits. Preserve the 2079 Go outputs, complex-path behavior and consuming-path depth tests; add bounded deep/wide scaling regressions. Ordinary walkers, reference expansion and serde/debug boundary audit are complete. Do not port the 1 GiB stack workaround. | Local `cpa-common/src/gemini_schema.rs`, `schema_path.rs`; R `541b8e47`, `e563f51a`, `0ccf7e4d` |
| U10 — connection setup | Assess bounded connection-setup settings separately from the completed per-minute fix. Prove any retry occurs before request-body transmission; an ambiguous timeout must remain terminal. Retain fresh quota checks and native CONNECT's no-retry policy. | R `b2ea411e`; G `345cc9d5`, `b3dc6c3f` |
| S01 — native entitlement | Reconcile uncommitted native changes, then reuse the bounded error observer to distinguish explicit model entitlement from authentication/quota failures. Cool only that account/model; preserve response bytes and no replay. Translating-path classification is complete. | S `84d9e570`; local `native_claude/` |

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
