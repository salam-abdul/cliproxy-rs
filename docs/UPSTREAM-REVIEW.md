# Upstream port queue

Last review: 2026-11-09. Each hash is the inclusive end of the default-branch history screened so far. A screened range is not a merge or a full source audit.

| Source | Branch | Reviewed through |
| --- | --- | --- |
| G: [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | `main` | `681ce41b6e44c45b15b79378b003909d6ba29e68` |
| R: [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs) | `master` | `2a810cb103839174d14385d6dfb424f95980f081` |
| S: [sub-api-proxy](https://github.com/DarkEden-coding/sub-api-proxy) | `main` | `d4c10513147821bafe99315ad520e265d46dfe69` |

The 2026-10-31 review screened `3078035..dbb7062` (89 Go commits), `0ccf7e4..b7ab292` (48 Rust commits) and `b9c875c..d4c1051` (3 sub-api-proxy commits) against fork `a4e81c0`, with focused source comparisons. The 2026-11-09 review screened `dbb7062..681ce41` (130 Go commits) and `b7ab292..2a810cb` (13 Rust commits) by subject and changed files, with source comparisons for the ports and decisions below; sub-api-proxy had no new commits. Neither review ran live provider calls or installation qualification. Resolved ports and their tests are in Git history.

## Pending decisions

| Decision | Source commits | Gate before porting |
| --- | --- | --- |
| Stream terminal and retry rules | G `aa4130e`, `f013eb6`, `86a1aab`; R `80977ac`, `186ee6e` | Existing truncation errors are request-scoped. Establish protocol-specific EOF and partial-usage contracts with local mocks, and keep explicit early 429 failover separate from truncation. Never replay after ambiguous delivery or emitted output. |
| Mid-conversation Claude tool additions | G `12d97cf` | Confirm the native Claude-to-Codex path receives `tool_addition`, then test its placement and tool-name identity with synthetic fixtures. |
| Codex compaction capsules | G `dbb7062` | Confirm recognized CPA capsules reach Codex, then preserve native OpenAI capsules unchanged. |
| Forced-tool restrictions | G `1d45a5f` | Upstream relaxes required or named tool choice to `auto`. Decide whether that downgrade is acceptable for a supported model. |
| Codex credential recovery | G `514d09f` | Define sanitized terminal-renewal state, account binding and race handling before adding targeted re-sign-in. Never verify by automatic inference. |
| Claude stream connection reuse | G `ce38c75c` | The relay stops reading at `message_stop` and drops the body, so the connection is not pooled. Drain a bounded remainder after the terminal event without forwarding it or extending the client's wait. |
| Model-scoped Claude 429s | G `0130d6f3` | `overage_only` lacks the rejected-with-absent-5h-utilization case, and native routing cools the whole account on any 429. Decide how a model-only weekly rejection maps to a native model cooldown. |
| Codex input ID characters | G `606e667a` | `sanitize_input_ids` adds prefixes but keeps characters outside `[a-zA-Z0-9_-]`. Port with the collision seeds unchanged. |
| Codex output budgets | G `01b24f1f`, `9be48133`, `bfac8586` | Codex requests drop `max_output_tokens`. Confirm subscription accounts accept it and the minimum before forwarding. |
| Permanently refused refresh tokens | R `6a3cb256` | Refresh keeps retrying an `invalid_grant` credential every 30 minutes. Remove it from rotation until its tokens change. |
| Smaller translator and auth fixes | G `05e668cf`, `69637a66`, `e6931d51`, `1c537211`, `e5827065`, `95f125ff` | Compare each with the matching translator or refresh path before porting. |
| Structured usage and speed metrics | G `89e472f` through `82deffb`; S `385b972` | Compare with durable history. Define time to first token, token-rate denominators, failures and missing coverage before adding fields. |

Out of scope: the plugin host, SIWC Codex sign-in, per-key connection pools, the key-value store, packaged releases, upstream CI, Go parity fixtures, benchmarks, discovery and unrelated dashboard branding. Omit plugin and distributed-product parts of mixed commits.

## Recorded differences

- A Codex account sign-in with no `websockets` setting uses the upstream Responses WebSocket; Go keeps it on HTTP. Its continuations then send only new input instead of the whole transcript. API keys keep Go's default.

- `cpa_core::config::go_url` follows Go 1.27.3 `parseHost` (a `[` only at the host start, strict colons for http and https only, no empty zone), not the older Go behind `xai_auth_go.json`: that parser accepts `https://attacker.example\[::1%25.x.ai]/` as an x.ai host while the HTTP client would contact `attacker.example`, so it now fails closed as `invalid IP-literal`; `xai_url::same_authority` stays as the second guard.

## Next review

1. Fetch separate review clones. Check each saved hash is an ancestor of its new head; investigate rewritten history first.
2. Screen `saved-hash..new-head` for each source. Advance a hash only after its whole range is screened. Pending decisions stay in the table when hashes advance.
3. Before porting a decision, reconcile it with current code and establish its local contract with focused Rust regressions, private auth directories and local mock upstreams. Record the finished port and its tests in the resolving commit, and keep any unfinished part here.

A finished port does not imply installation, publishing or live-provider testing.
