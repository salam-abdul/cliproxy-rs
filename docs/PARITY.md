# Compatibility scope

This personal fork preserves existing built-in providers and protocols where useful. Complete Go parity is not a goal. Tests and [deliberate differences](DIFFERENCES-FROM-GO.md) define the supported behavior; historical audit counts are not maintained.

## References

- CLIProxyAPI `6fecc6e5567912661654a4eaf9b8f5436facd1c2` is the baseline for config, credentials, routes and response shapes.
- Claude prompt shaping/cache behavior and selected Antigravity fixes follow v8.0.27, `bf9341629ef2300ac9d846a2e80fe2756f63a671`. The fixture README files identify their reference generators.
- Rust upstream through `d1261f2` contributes opt-in final-wire Claude cache telemetry and offline replay. Native CONNECT traffic uses its own byte-preserving transport.

## Supported paths

The built-in Anthropic, OpenAI and Gemini routes support buffered and streaming responses, translation, account selection and credential refresh. Codex WebSocket, realtime/media, image/video and provider-specific paths retain their existing tests. The dashboard and terminal UI manage the local runtime.

Native Claude preserves request bodies, cache markers, model names and beta headers. It requires a session identity, confirms included quota and avoids replay after an ambiguous lost response. Its qualification suite is described in [the native guide](../native/README.md).

## Scope decisions

| Upstream unfinished work | Personal-fork decision |
| --- | --- |
| Plugin ABI, schedulers, translators, callbacks and store | Excluded; use reviewed built-in features. |
| Home-managed plugin synchronization, tasks and status | Excluded with plugins. |
| Public packages and releases | Excluded; build locally. |
| Antigravity login | Deferred; existing authenticated execution remains available. |
| Dashboard Vertex import | Deferred; CLI import exists. |
| `pprof` listener | Deferred; use offline benchmarks and platform profiling. |
| Unported Go cases | Prioritize credential isolation, wire fidelity, cancellation, retries and bounded resources. |

Existing non-plugin Home and remote-store paths remain available. Local mocks and Go goldens establish only the behavior they exercise; they do not prove live-provider compatibility. Live checks require separate qualification and private evidence.

## Reviewed plugin ideas

- [Quota Router](https://github.com/kpatdev/quota-router): reset-aware selection and passive quota signals are already built in. This fork improves native quota refresh concurrency and bounds cache state; it does not import the plugin.
- [CPA Usage](https://github.com/andyWang1688/cpa-usage): a second usage database/dashboard adds operational cost; retain the embedded dashboard and usage queue.
- [Gemini CLI plugin](https://github.com/router-for-me/cpa-plugin-gemini-cli): defer its additional provider path. Existing Gemini APIs remain supported.
