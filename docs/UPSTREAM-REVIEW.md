# Upstream port queue

Last review: 2026-10-31. Each hash is the inclusive end of the default-branch history screened so far. A screened range is not a merge or a full source audit.

| Source | Branch | Reviewed through |
| --- | --- | --- |
| G: [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | `main` | `dbb706215a3afd2f0ea3c96482d4233856466caa` |
| R: [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs) | `master` | `b7ab292f47c61263da16d0e07358f492ad4dabf4` |
| S: [sub-api-proxy](https://github.com/DarkEden-coding/sub-api-proxy) | `main` | `d4c10513147821bafe99315ad520e265d46dfe69` |

The 2026-10-31 review screened `3078035..dbb7062` (89 Go commits), `0ccf7e4..b7ab292` (48 Rust commits) and `b9c875c..d4c1051` (3 sub-api-proxy commits) against fork `a4e81c0`, with focused source comparisons. It ran no live provider calls and no installation qualification. Resolved ports and their tests are in Git history.

## Pending decisions

| Decision | Source commits | Gate before porting |
| --- | --- | --- |
| Stream terminal and retry rules | G `aa4130e`, `f013eb6`, `86a1aab`; R `80977ac`, `186ee6e` | Existing truncation errors are request-scoped. Establish protocol-specific EOF and partial-usage contracts with local mocks, and keep explicit early 429 failover separate from truncation. Never replay after ambiguous delivery or emitted output. |
| Mid-conversation Claude tool additions | G `12d97cf` | Confirm the native Claude-to-Codex path receives `tool_addition`, then test its placement and tool-name identity with synthetic fixtures. |
| Codex compaction capsules | G `dbb7062` | Confirm recognized CPA capsules reach Codex, then preserve native OpenAI capsules unchanged. |
| Forced-tool restrictions | G `1d45a5f` | Upstream relaxes required or named tool choice to `auto`. Decide whether that downgrade is acceptable for a supported model. |
| Codex credential recovery | G `514d09f` | Define sanitized terminal-renewal state, account binding and race handling before adding targeted re-sign-in. Never verify by automatic inference. |
| Structured usage and speed metrics | G `89e472f` through `82deffb`; S `385b972` | Compare with durable history. Define time to first token, token-rate denominators, failures and missing coverage before adding fields. |

Out of scope: the plugin host, packaged releases, upstream CI, Go parity fixtures, benchmarks, discovery and unrelated dashboard branding. Omit plugin and distributed-product parts of mixed commits.

## Recorded differences

- `cpa_core::config::go_url` follows Go 1.27.3 `parseHost` (a `[` only at the host start, strict colons for http and https only, no empty zone), not the older Go behind `xai_auth_go.json`: that parser accepts `https://attacker.example\[::1%25.x.ai]/` as an x.ai host while the HTTP client would contact `attacker.example`, so it now fails closed as `invalid IP-literal`; `xai_url::same_authority` stays as the second guard.

## Next review

1. Fetch separate review clones. Check each saved hash is an ancestor of its new head; investigate rewritten history first.
2. Screen `saved-hash..new-head` for each source. Advance a hash only after its whole range is screened. Pending decisions stay in the table when hashes advance.
3. Before porting a decision, reconcile it with current code and establish its local contract with focused Rust regressions, private auth directories and local mock upstreams. Record the finished port and its tests in the resolving commit, and keep any unfinished part here.

A finished port does not imply installation, publishing or live-provider testing.
