# Upstream review checkpoint

Last review: **2026-10-25**. Fork reviewed: `873efc7160da7c56235cae373ebf13570e8caa52`.
Independent source and fork-fit reviews were reconciled against
`fa0cd8b5a4ef61474500d04885334fc07d15ec30` on the same date. That intervening commit
adds model-specific native status and quota UI; it does not resolve the U items below.

These are **review boundaries, not merge or implementation boundaries**. The commit
inventories were screened for relevance, and the recommendations below were checked
against upstream patches and this fork's source. This was not an exhaustive correctness
audit of every upstream change. Runtime ports and their validation are recorded below.
Existing [compatibility references](PARITY.md) remain unchanged.

| Repository | Branch | Review start (exclusive) | Reviewed through (inclusive) | Commits screened |
| --- | --- | --- | --- | --- |
| [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | `main` | `6fecc6e5567912661654a4eaf9b8f5436facd1c2` | `307803510bce87f986fcf7b1a84190f105c7b4aa` (v8.0.43) | 395, including 42 merges |
| [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs) | `master` | `d1261f28738a8c793b8f5ff893a73dea8c9994f8` (v0.2.6, already integrated) | `0ccf7e4da7676994a8f342d7de5deb6f1362123c` | 129 |
| [sub-api-proxy](https://github.com/DarkEden-coding/sub-api-proxy) | `main` | `81afb74428e158b0b7149b1d44972573ed5c4eda` (shared Rust ancestor) | `b9c875c3d9060a55f9caf377d6b87d1ffc4b0008` | 71, including 14 merges |

The Go range includes the selectively adopted v8.0.27 reference
`bf9341629ef2300ac9d846a2e80fe2756f63a671`; 228 commits follow that reference.
Rust upstream now targets Go v8.0.35, so its ports can still need later Go fixes.
Sub-API-Proxy's inherited Rust history is covered by the Rust range; its separate
range covers the changes since the verified common ancestor, without counting
inherited commits twice.
Scope is merged default-branch history through these snapshots; open PRs and other
branches were not reviewed.

## Port status

Completed locally on **2026-10-25**, not yet committed or installed:

| ID | Implementation and focused validation |
| --- | --- |
| U07 | Both catalog startup entry points honor the test runtime's network guard. `cargo test --offline -p cpa-server --lib model_updater::tests -- --test-threads=1`: 3 passed; `--test model_updater`: 1 passed against loopback sources. The two new regressions failed before the fix without starting a network task. |
| U01 | Claude refresh retries only explicit HTTP server errors. `cargo test --offline -p cpa-exec --lib oauth::tests::`: 30 Claude/Codex tests passed, including single-flight, cancellation, 429 blocking and 5xx recovery. Lost, truncated, gzip-invalid, JSON-invalid and missing-field responses each made one exchange; all six cases made three exchanges before the fix. |
| U03 | Incoming RESP parsing enforces 1,024 arguments, 64 KiB line/bulk limits and 64 KiB total raw argument payload, including subscription commands. `cargo test --offline -p cpa-server --lib resp::tests::`: 5 passed; `--test resp_protocol`: 8 passed. Coverage includes pre-auth rejection without EOF/payload waits, exact limits, cumulative budgets, UTF-8 replacement, pipelining and existing management commands. Three new parser regressions failed before the fix. |

Other items remain pending. Record resolving fork commits after these changes are committed.
Formatting and `cargo clippy --offline -p cpa-exec -p cpa-server --lib --tests -- -D warnings`
also passed. Validation used focused tests and local mocks, not the full suite or live providers.
An independent implementation and test review on **2026-10-25** found no actionable
issues in U07, U01 or U03. The eight catalog/RESP unit tests and formatting checks
were rerun successfully after that review was requested.

## Recommended ports

Prefer small ports with their regression fixtures. Source locations below refer to
the fork at the reviewed commit. Preserve native CONNECT byte fidelity, credential
isolation, bounded diagnostics, and the rule against replay after ambiguous inference
responses. Plugin portions of mixed commits must be omitted.

Recommended sequence:

1. **U07 → U01 → U03** is completed locally with focused regression tests.
   Preserve refresh ownership/single-flight and supported RESP management commands.
   RESP shares the normal listener; it is not a separately disabled service.
2. **U02 plus U14's credential locking**, as a coordinated lifecycle change covering
   refresh, edits, reload, deletion, credential-ID changes and cooldown preservation.
   U14's ETag/If-Match improvement is independent.
3. **U08**, as dedicated JSON safety work with all affected built-in callers and
   deep parse/marshal/drop fixtures; it is not a small cleanup.
4. **U04–U06 and U09–U13**, as focused maintenance of retained translating/provider
   paths. Split U05's stream completion, relay cancellation and Codex SSE work, U09's
   request and response semantics, and U10's quota and connection-setup changes.
   These do not automatically improve or change native CONNECT handling.

### Safety and correctness

| ID | Change and evidence in this fork | Upstream starting points and port conditions |
| --- | --- | --- |
| U01 | **Do not retry ambiguous Claude OAuth refresh failures.** `cpa-exec/src/oauth.rs::OAuth::refresh` retries every error with status >=500, including transport/read failures. A successful rotation whose reply was lost can consume the refresh token before that retry. | [Rust 73bd4228](https://github.com/vayungodara/cliproxy-rs/commit/73bd4228), [Go c163bae4](https://github.com/router-for-me/CLIProxyAPI/commit/c163bae4). Retry only an explicit eligible HTTP error response; cover dropped responses, malformed success bodies, 429 and explicit 5xx with local mocks. |
| U02 | **Preserve credentials across refresh/edit/reload races and unreadable scans.** `runtime.rs::prepare_credential` turns a concurrent revision change into a commit error; it can discard a newly rotated token after an unrelated edit. `watching.rs` records the observed snapshot rather than the bytes actually loaded. `config/credentials.rs::from_auth_dir` returns an empty set on directory read failure, and `CredentialStore::reconcile` replaces the set. | [Rust 95773e7f](https://github.com/vayungodara/cliproxy-rs/commit/95773e7f), [ffbc0fc2](https://github.com/vayungodara/cliproxy-rs/commit/ffbc0fc2), [f48fca45](https://github.com/vayungodara/cliproxy-rs/commit/f48fca45), [d07c7c76](https://github.com/vayungodara/cliproxy-rs/commit/d07c7c76), then [Go d5816d5e](https://github.com/router-for-me/CLIProxyAPI/commit/d5816d5e), [5fe203d5](https://github.com/router-for-me/CLIProxyAPI/commit/5fe203d5), [57a12996](https://github.com/router-for-me/CLIProxyAPI/commit/57a12996), [32dd2c86](https://github.com/router-for-me/CLIProxyAPI/commit/32dd2c86). Keep true deletion distinct from incomplete scans; retain newer tokens and active quota cooldowns. Existing revision checks already prevent a simple stale overwrite; this is a broader lifecycle port. |
| U03 | **Bound RESP parsing before authentication.** `cpa-server/src/resp.rs::read_line` grows until newline; `read_command` accepts an unbounded argument count. The existing 64 MiB per-bulk cap does not bound a whole command. | [Go 888f2382](https://github.com/router-for-me/CLIProxyAPI/commit/888f2382): 1,024 arguments and 64 KiB line/bulk/frame budgets, plus malformed-length checks. Port parser limits; Go panic-recovery machinery is not a Rust implementation requirement. |
| U04 | **Reauthorize established Responses WebSockets.** `access.rs` authenticates the HTTP upgrade; `websocket.rs` retains that caller without checking later turns against changed client keys. | [Go b9bb031d](https://github.com/router-for-me/CLIProxyAPI/commit/b9bb031d), [3e7a21e2](https://github.com/router-for-me/CLIProxyAPI/commit/3e7a21e2), [6340fe43](https://github.com/router-for-me/CLIProxyAPI/commit/6340fe43). Check normal and duplex turns and retain the bound principal. Use built-in authentication only; the interceptor-specific model rewrite is conditional on an equivalent path. Test key revocation while a socket stays open. |
| U05 | **Detect incomplete streams and release abandoned relay requests.** `claude/stream.rs::end(None)` can finish without `message_stop`; `wsrelay.rs` can lose a close error when its queue is full and wait for upstream after the receiver drops. `aistudio.rs` translates an entire relay payload as one event. | [Go 4f2e5270](https://github.com/router-for-me/CLIProxyAPI/commit/4f2e5270), [Rust dcb7336b](https://github.com/vayungodara/cliproxy-rs/commit/dcb7336b), [1639d8d6](https://github.com/vayungodara/cliproxy-rs/commit/1639d8d6), [Go 30780351](https://github.com/router-for-me/CLIProxyAPI/commit/30780351). Also take [Rust 24c32be3](https://github.com/vayungodara/cliproxy-rs/commit/24c32be3) for multiline Codex SSE and assess [Go 0fe4cc2c](https://github.com/router-for-me/CLIProxyAPI/commit/0fe4cc2c), [90fe5b23](https://github.com/router-for-me/CLIProxyAPI/commit/90fe5b23) against Rust's channel model. Preserve partial output and terminal errors; do not turn detection into inference replay. |
| U06 | **Separate internal error headers from downstream headers, and respect stopping bootstrap failures.** `dispatch.rs::upstream_headers` forwards a non-direct error's classification header map when passthrough is enabled. Existing framing/Set-Cookie filtering helps but does not implement Go's explicit error-header contract. | [Rust b5ae1ea3](https://github.com/vayungodara/cliproxy-rs/commit/b5ae1ea3) plus its necessary follow-up [7e76e295](https://github.com/vayungodara/cliproxy-rs/commit/7e76e295). For non-direct errors expose only intended Addon headers; retain direct-response headers and scheduler Retry-After behavior. Prevent a stopping bootstrap error from entering another retry round. |
| U07 | **Keep test runtimes off remote catalog endpoints.** `model_updater.rs::start` and `codex_client_catalog_wanted` do not inspect the test runtime's `deny_external` flag. Executor credential guards do not cover these separate background fetches. | [Rust 046c9423](https://github.com/vayungodara/cliproxy-rs/commit/046c9423). Small, useful prerequisite for subsequent port validation; test both startup and the Codex catalog request path. |
| U08 | **Remove native recursion from GoValue decoding, marshaling and destruction.** `cpa-common/src/json.rs::GoValue` recursively decodes, serializes and drops containers. Valid deeply nested input can exhaust the thread stack. | [Rust 541b8e47](https://github.com/vayungodara/cliproxy-rs/commit/541b8e47) and [e563f51a](https://github.com/vayungodara/cliproxy-rs/commit/e563f51a). Carry the affected non-plugin callers and depth-limit fixtures together, preserving numeric and wire semantics. These commits also fix selected walkers; they do not eliminate recursion from every JSON API. AnyValue processing and derived cloning need separate assessment. |

### Semantics, efficiency and management

| ID | Recommendation | Evidence and boundaries |
| --- | --- | --- |
| U09 | **Honor explicit tool concurrency and stop reasons.** | Both `codex_chat_request.rs` and `codex_responses.rs` force `parallel_tool_calls: true`: port [Go 99d1698f](https://github.com/router-for-me/CLIProxyAPI/commit/99d1698f). `claude_chat_response.rs` defaults context-window exhaustion to `stop`; `claude_responses_response.rs::incomplete` recognizes only `max_tokens`. Use the final behavior in [c2dfec28](https://github.com/router-for-me/CLIProxyAPI/commit/c2dfec28) and [d495e24c](https://github.com/router-for-me/CLIProxyAPI/commit/d495e24c), not the superseded intermediate error behavior in aadb34f9. Review [43f27e3a](https://github.com/router-for-me/CLIProxyAPI/commit/43f27e3a) separately for actual prompt-overflow errors. |
| U10 | **Distinguish per-minute throttling from subscription exhaustion; separately bound connection setup.** | `cpa-exec/src/quota.rs` selects resets from headers without the newer explicit per-minute error-body exception. [Rust b2ea411e](https://github.com/vayungodara/cliproxy-rs/commit/b2ea411e) ports [Go db1bab01](https://github.com/router-for-me/CLIProxyAPI/commit/db1bab01), 345cc9d5 and b3dc6c3f. Rust's timeout covers DNS, TCP and proxy CONNECT as well as TLS. Keep bounded cooldown probes and fresh quota admission. Any retry must prove failure occurred before inference bytes were sent; preserve native CONNECT's existing no-automatic-retry client and connection timeout. |
| U11 | **Avoid repeated full-body walks.** | `payload.rs::edit` repeatedly resolves and edits against the whole body for each wildcard match; use [Rust 34a2dab6](https://github.com/vayungodara/cliproxy-rs/commit/34a2dab6). `codex_claude_response.rs` rebuilds `short_to_original` for each emitted tool call; use [Go 95e15777](https://github.com/router-for-me/CLIProxyAPI/commit/95e15777). The related [74e7ff60](https://github.com/router-for-me/CLIProxyAPI/commit/74e7ff60), [83189e5f](https://github.com/router-for-me/CLIProxyAPI/commit/83189e5f), [f4a2d040](https://github.com/router-for-me/CLIProxyAPI/commit/f4a2d040) and [3b153145](https://github.com/router-for-me/CLIProxyAPI/commit/3b153145) suggest the same per-stream metadata pattern. Preserve untouched bytes and measure scaling with large bodies; no speedup is established by this review. |
| U12 | **Restore fully wrapped caller MCP tool names.** | [Rust 954e6e76](https://github.com/vayungodara/cliproxy-rs/commit/954e6e76), from [Go 02fc76b1](https://github.com/router-for-me/CLIProxyAPI/commit/02fc76b1), adds a missing exact passthrough-name case in `claude/alias.rs::Resolver`. Keep ambiguous-name rejection. This applies to the translating Claude path, not native CONNECT shaping. |
| U13 | **Update schema cleanup and translated tool arguments as a coordinated port.** | [Rust 0ccf7e4d](https://github.com/vayungodara/cliproxy-rs/commit/0ccf7e4d) and [81afb744](https://github.com/vayungodara/cliproxy-rs/commit/81afb744) contain useful Gemini/Antigravity schema and response fixes. However, the Rust head's new `claude_tool_args.rs::claude_tool_is_parameterless` still treats implicitly open empty object schemas as parameterless. Include [Go e1fb2521](https://github.com/router-for-me/CLIProxyAPI/commit/e1fb2521) to retain real arguments, plus [29150a22](https://github.com/router-for-me/CLIProxyAPI/commit/29150a22) and [b2fda98e](https://github.com/router-for-me/CLIProxyAPI/commit/b2fda98e) for reference siblings and array items. Do not blindly import the older Rust cleanup. |
| U14 | **Add config concurrency checks and shorten credential-store lock holds.** | `management.rs::config_sync` lacks ETag/If-Match; [Rust 49c23c50](https://github.com/vayungodara/cliproxy-rs/commit/49c23c50) ports [Go fa68946a](https://github.com/router-for-me/CLIProxyAPI/commit/fa68946a). Extract that behavior without the unrelated management bundle. `runtime.rs::apply_patch` holds the credential-set write lock through disk persistence; [Rust 898f884e](https://github.com/vayungodara/cliproxy-rs/commit/898f884e) provides per-credential write serialization and a reload gate. Coordinate with U02 and retain this fork's byte-preserving config edits. |
| U15 | **Harden request-log filenames across platforms.** | `request_logging.rs::filename` strips `/` but retains backslashes and parent-directory sequences. [Go 179d43dc](https://github.com/router-for-me/CLIProxyAPI/commit/179d43dc) fixes Windows path traversal and validates the final component. Useful defense in depth; this is not a demonstrated macOS traversal. Preserve the shared redacting capture sink. |

## Feature candidates and deferred work

These were screened as potentially useful, but need focused design/validation before
implementation. They remain in the backlog even though their commits precede the checkpoint.

| ID | Decision |
| --- | --- |
| F01 | **Consider Codex-to-Claude tool search and Responses compaction when that translating workflow is needed.** [Rust 2fcbd71f](https://github.com/vayungodara/cliproxy-rs/commit/2fcbd71f) plus [7f9e2afd](https://github.com/vayungodara/cliproxy-rs/commit/7f9e2afd), and [f264daa5](https://github.com/vayungodara/cliproxy-rs/commit/f264daa5). They do not benefit native Claude by rewriting its traffic. Preserve capability gating, unsupported-upstream fallback, and stable cache prefixes; cache-related changes require the repository's manual warm/changed/idle qualification before merge. |
| F02 | **Consider registry-driven thinking controls.** [Rust 827445c1](https://github.com/vayungodara/cliproxy-rs/commit/827445c1) brings `between_tools` and registry disable capabilities; retain the fork's fallback for models that refuse `disabled`, and omit plugin appliers. Related Go thinking-binding and cloak/header changes need separate native-versus-translated review. |
| F03 | **Defer wholesale payload-finalizer and Claude translator bundles.** Rust 22abc036, e3487de5, 8e1a5af5, b264ea8b, ff9266b8 and d82874c5 implement [Go 868e137f](https://github.com/router-for-me/CLIProxyAPI/commit/868e137f); [85c7cf34](https://github.com/vayungodara/cliproxy-rs/commit/85c7cf34) is another broad translator bundle. Final override ordering, token-default handling and attachment preservation are useful, but alter semantics and refusal behavior. Extract targeted fixes with no-rules byte fixtures; do not adopt them as a general refactor. |
| F04 | **Defer Sign in with ChatGPT as a feature; retain its locking lessons.** Rust 2b3e4394, 98ec7d63 and 93cb1027 introduce binding/execution/CLI support. If adopted, include [1f79a524](https://github.com/vayungodara/cliproxy-rs/commit/1f79a524), [92803328](https://github.com/vayungodara/cliproxy-rs/commit/92803328), and later [Go d946c4fb](https://github.com/router-for-me/CLIProxyAPI/commit/d946c4fb) and binding fixes. Do not add another refresh owner merely to gain this feature. |
| F05 | **Defer provider expansion until needed.** [Rust 87c3c33d](https://github.com/vayungodara/cliproxy-rs/commit/87c3c33d) adds xAI speech; [5905c48e](https://github.com/vayungodara/cliproxy-rs/commit/5905c48e) adds Antigravity sign-in and dashboard Vertex import, both currently deferred by scope. Go d9451cb6/d44f8682 add Vertex OpenAPI routes, and [27f3abfb](https://github.com/router-for-me/CLIProxyAPI/commit/27f3abfb) adds session-scoped OAuth proxies. These are optional workflow additions, not prerequisites for native Claude. |
| F06 | **Retain secondary built-in correctness work for focused follow-up.** Catalog/source configuration (Rust 07a161a0, a66d8c56, bb7b4c7a, aa98808d); usage/finish reasons (Go ab151f5d, dfe0a0ef, b12db8c3, 3f5d3a16, bdde391d, 07b5a7a1, b90190db); thinking/signature continuity (Go 80aa2b1d, da21f1e5, 38706390, e5434f65, 009f8d7a); Devin/Kimi/xAI/Vertex fixes (Go b946d54e, ab139b45, 0c61b12c, ba06ec58, 88732ad5, 3dae075d). Preserve the existing Antigravity backlog in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md#antigravity). These families were screened, not fully audited or proven unnecessary. |
| F07 | **Retain non-plugin Home/store/config work at lower priority.** Rust 193bc704, d3f89ff5, 1a775b09 and 481dbdf9 improve catalogs, safe config seeding and sessions; Go 0d3ca1d3, ce433b37 and 294ed097 cover store recovery, subscriber backoff and discovery. Home rebalance/latency additions need a deployment-specific need. Historical YAML aliases, scalar/case/float parity and config response details are compatibility work, not a reason to replace the fork's config writer. |
| F08 | **Consider a configurable HTTP body limit without removing existing ceilings.** [Rust 1f6f40f4](https://github.com/vayungodara/cliproxy-rs/commit/1f6f40f4), 1349d4cd and c5107a32 implement Go's route-aware limit. This fork already caps normal HTTP extraction at 64 MiB and OAuth callbacks at 64 KiB. Upstream's setting is off by default; a port must retain useful caps and deliberately cover management readers, compressed bodies and request capture. RESP limits in U03 are separate. |

Rejected-token recovery (Rust 0d0e1d1e, Go 4138edb1/21553509), cooldown races
(Rust a45d4f1e), and cancellation accounting are part of U02/U05 follow-up, not
permission to replace the fork's scheduler wholesale. Existing bounded cooldown
generations and native quota checks overlap some upstream fixes.

## Sub-API-Proxy: selective ideas, not a wholesale merge

This fork adds eight `sap-*` crates: client-thread identification, account scoring,
a local router across primary/fallback servers, node management, SQLite persistence,
authenticated metadata replication over Tailscale, shared contracts, and a CLI.
It also adds a dashboard, service installers and a macOS menu-bar app. Provider
credentials remain node-local; account metadata, thread assignments, quotas and
usage can be replicated. It is a distributed subscription-routing application
built on the Rust proxy, not another Go-parity update.

| ID | Decision and evidence |
| --- | --- |
| S01 | **Adapt explicit model-entitlement classification on translating paths.** [84d9e570](https://github.com/DarkEden-coding/sub-api-proxy/commit/84d9e570) distinguishes model-specific denial from authentication, rate limits and generic permission errors. Our `classify.rs` already recognizes several model-support errors, but lacks this complete explicit-code/403 evidence handling. Extract classification and fixtures without `SapHooks`; retain provider-specific cooldowns. Native routing handles statuses directly, so changing `classify.rs` alone does not fix native entitlement handling. A native port needs separate bounded error inspection that preserves bytes, model-scoped cooldowns and continuation rules. |
| S02 | **Consider a dedicated bounded Codex usage fetcher.** The same commit's `cpa-exec/src/usage_probe.rs` fixes the provider origin, rejects redirects, limits responses to 1 MiB and bounds the whole fetch to 10 seconds. `sap-server/src/reset_aware.rs` coalesces concurrent refreshes and caps account probes at four. Our dashboard already queries Codex usage through the management API-call route; this is backend consolidation/hardening, not a missing quota display. Reuse our token-keyed shared Claude usage cache; preserve its reset-aware expiration, network-denial guard, credential formats and single refresh owner. Do not copy SAP's reduced Claude window projection over our model-specific quota admission. |
| S03 | **Consider thread metadata as a diagnostics experiment.** [17a87eba](https://github.com/DarkEden-coding/sub-api-proxy/commit/17a87eba) identifies Claude agents/parents and Codex forks, prewarm, compaction and frame-level metadata without retaining conversation fields; [12cb0448](https://github.com/DarkEden-coding/sub-api-proxy/commit/12cb0448) preserves identity when optional metadata is malformed. Our native extractor already supports session IDs and older encoded IDs, but does not model agent lineage. Retain legacy formats, hashing and bytes; parent metadata must not change affinity keys or collapse child sessions. Separately, [85418f7c](https://github.com/DarkEden-coding/sub-api-proxy/commit/85418f7c) shows activity before first output. Our current UI labels missing final outcomes as pending history, not proof of an active request. Preserve that distinction and first-byte timing; do not adopt automatic compaction/account moves for native incremental continuations. |
| S04 | **Keep weighted routing as offline research.** At [b9c875c3](https://github.com/DarkEden-coding/sub-api-proxy/commit/b9c875c3), new-thread shares use remaining weekly allowance, time to reset, plan/manual capacity, load, a 36-hour drain window and a five-hour taper above 90%. The capacity learner was removed; older documents describing learning are stale. Our default native policy already preserves eligible pins and prefers model-window headroom (5% default), then earliest weekly reset; shadow mode is available. Compare against that policy, behind our fresh-quota and model-window checks: SAP permits stale observations and assumes renewed allowance after elapsed resets. No demonstrated improvement or current need warrants implementation. |
| S05 | **Defer multi-node routing and replication until that deployment is wanted.** [9a7e4cc8](https://github.com/DarkEden-coding/sub-api-proxy/commit/9a7e4cc8), [336a679b](https://github.com/DarkEden-coding/sub-api-proxy/commit/336a679b), [0495c1e5](https://github.com/DarkEden-coding/sub-api-proxy/commit/0495c1e5) and [262e7090](https://github.com/DarkEden-coding/sub-api-proxy/commit/262e7090) provide connection-before-body failover, HMAC-authenticated bounded sync, causal cursors, conflict records and tombstones. [cac8af8a](https://github.com/DarkEden-coding/sub-api-proxy/commit/cac8af8a) avoids refusing every request solely because probes failed. The router never replays a sent HTTP request, but health loss can terminate an active stream when another server is stable. That behavior and SAP's account-move hooks ([9227fffc](https://github.com/DarkEden-coding/sub-api-proxy/commit/9227fffc)) need separate native-continuation review. HTTP/private-network assumptions and replication complexity are not warranted for the current single-node fork. |
| S06 | **Retain persistence lessons; defer the new store and analytics product.** [3aeab635](https://github.com/DarkEden-coding/sub-api-proxy/commit/3aeab635) skips zero usage deltas, moves compaction off async workers and avoids unconditional VACUUM. These fix SAP's new SQLite/op-log path, which our fork does not have. Apply the lessons if persistence is introduced. Peer telemetry provenance/staleness ([abc262c6](https://github.com/DarkEden-coding/sub-api-proxy/commit/abc262c6), [7469e7a5](https://github.com/DarkEden-coding/sub-api-proxy/commit/7469e7a5)) remains a distributed-deployment reference. At the fork recheck, native status already exposes model-specific availability, included windows and validity, and the quota UI labels stale readings; do not duplicate that work. Usage-price estimates, the menu app, packaging and operator setup remain optional scope. |

Do not port SAP's Claude client setup over native CONNECT. Its
[client-configuration evidence](https://github.com/DarkEden-coding/sub-api-proxy/blob/b9c875c3d9060a55f9caf377d6b87d1ffc4b0008/docs/sub-api-proxy/client-config.md)
reports that the chosen custom-base-URL/subscription-login mode retains subscription
betas but loses native message threads and shared cache scope, sending full history
instead. That evidence came from local client/mock checks, not live inference.
Our native workflow deliberately preserves those capabilities. SAP's UI, installation
and routing changes therefore are not substitutes for this fork's native path.

## Already covered or outside scope

- Rust v0.2.6 cache telemetry/replay is integrated. The credential-ID fixture isolation
  addressed by Rust 450fca7f was also fixed in this fork's integration; do not repeat
  the entire merge or import upstream parity-count documentation.
- Existing v8.0.27 Claude cache/shaping fixes, overage-reset filtering, selected
  Antigravity fixes, request-log redaction, bounded WebSocket writes and native
  ambiguous-response handling remain intentional. Newer upstream does not replace
  these decisions automatically.
- Plugin ABI, schedulers, executors, hooks, quota plugins and Home plugin sync stay
  excluded. Public releases/packages/images, sponsors, review-bot configuration,
  upstream operator instructions and upstream benchmark numbers are not ports.
- `pprof` remains deferred as already recorded in [PARITY.md](PARITY.md).
  Test-only changes should accompany relevant ports; upstream release/CI machinery
  is not needed merely because it changed.

## Next review

1. Read this file and the current compatibility scope before collecting history.
   Reconcile pending U/F/S items with any ports made since this review; record the fork
   commit that resolves an item instead of silently deleting it.
2. Fetch each upstream into a separate read-only review clone. Resolve its default
   branch to an exact SHA. Check `git merge-base --is-ancestor <reviewed-through> <new-head>`.
   If it fails, investigate rewritten history; do not treat a date or tag as equivalent.
3. Review `git log --reverse <reviewed-through>..<new-head>` and the corresponding
   patches. The recorded boundary is exclusive on the next pass. Read older commits
   only for dependencies, unresolved items above, or a changed scope decision.
4. Advance a repository's boundary only after screening its whole new range and
   recording dispositions. A pending port does not block advancing the screening
   boundary, but its backlog entry must survive. Record review date and fork SHA.
5. A port updates its fixtures and applicable compatibility differences separately.
   These checkpoints never claim that Go parity, live-provider qualification, a merge,
   or installation has occurred.
