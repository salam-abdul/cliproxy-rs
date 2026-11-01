# Upstream port queue

Initial review **2026-10-25**, against fork `fa0cd8b5a4ef61474500d04885334fc07d15ec30`.
These are inclusive boundaries for screened default-branch history, not merge or full-audit claims.

| Source | Branch | Reviewed through |
| --- | --- | --- |
| G — [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | `main` | `dbb706215a3afd2f0ea3c96482d4233856466caa` |
| R — [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs) | `master` | `b7ab292f47c61263da16d0e07358f492ad4dabf4` |
| S — [sub-api-proxy](https://github.com/DarkEden-coding/sub-api-proxy) | `main` | `d4c10513147821bafe99315ad520e265d46dfe69` |

## Review of 2026-10-31

The saved hashes were ancestors of all three source heads. Compared with fork
`a4e81c0` and this dashboard update. Screened
`3078035..dbb7062` (89 Go commits), `0ccf7e4..b7ab292` (48 Rust commits) and
`b9c875c..d4c1051` (3 sub-api-proxy commits). This was a commit-range screen
with focused source comparisons, not a full source audit or a port. No live
provider calls or installation qualification were run.

After independent impact review, only the decisions below remain open. Resolved
work and its tests are in Git commits.

| Pending decision | Source commits | Gate before porting |
| --- | --- | --- |
| Stream terminal and retry rules | G `aa4130e`, `f013eb6`, `86a1aab`; R `80977ac`, `186ee6e` | Existing truncation errors are request-scoped. Establish protocol-specific EOF and partial-usage contracts with local mocks; keep explicit early 429 failover separate from truncation. Never replay after ambiguous delivery or emitted output. |
| Mid-conversation Claude tool additions | G `12d97cf` | Confirm the native Claude-to-Codex path receives `tool_addition`, then test its placement and tool-name identity with synthetic fixtures. |
| Codex compaction capsules | G `dbb7062` | Confirm recognized CPA capsules reach Codex, then preserve native OpenAI capsules unchanged. |
| Forced-tool restrictions | G `1d45a5f` | Upstream relaxes required or named tool choice to `auto`; decide explicitly whether that semantic downgrade is acceptable for a supported model. |
| Codex credential recovery | G `514d09f` | Define sanitized terminal-renewal state, account binding and race handling before adding targeted re-sign-in. Never perform automatic inference verification. |
| Structured usage and speed metrics | G `89e472f` through `82deffb`; S `385b972` | Compare with durable history; define TTFT, token-rate denominators, failures and missing coverage before adding fields. |

Plugin host, packaged releases, CI, Go parity fixtures, benchmarks, discovery,
and unrelated dashboard branding stay outside this fork's scope. The reviewed
boundaries advance despite unfinished candidates; preserve this queue until each
is resolved or explicitly dropped.

## Pick up next

Choose one pending decision only after its local contract is established. The
next upstream review starts from the saved hashes above.

## Handoff

- Before porting, reconcile this queue with current code. Use focused Rust regressions,
  private auth directories and local mock upstreams; no Go parity infrastructure.
  Record completed work and its tests in its resolving commit; preserve unfinished
  parts of split items here.
- Keep native Claude CLI/Desktop/Zed fidelity and built-in providers. Omit plugin
  and distributed-product portions of mixed commits. Scope and operational setup
  are in [README](../README.md) and the [native guide](../native/README.md).
- Next upstream review: fetch separate review clones, verify each saved hash is an
  ancestor of its new head, then screen `saved-hash..new-head`. Investigate rewritten
  history first. Advance a boundary only after its full range is screened; pending
  ports survive checkpoint advancement.
- Speculative features and deferred product ideas are intentionally omitted. A
  completed port does not imply installation, publishing or live-provider testing.
