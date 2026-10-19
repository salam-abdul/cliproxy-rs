# cliproxy-rs

A private personal fork. Preserve native Claude CLI/Desktop/Zed workflows and existing built-in providers. Do not publish packages, releases or images, or replace the running installation without an explicit request.

A Rust rewrite of CLIProxyAPI (https://github.com/router-for-me/CLIProxyAPI) preserving useful compatibility: same `config.yaml`, same auth JSON files in `auth-dir`, built-in HTTP routes and response shapes, and v8 Management API compatibility within the scope in `docs/PARITY.md`.

## Reference

- The reference is CLIProxyAPI at commit `6fecc6e`: clone https://github.com/router-for-me/CLIProxyAPI, check out that commit and treat the clone as read-only. Cite Go file paths in comments only where behaviour is non-obvious. The Claude prompt-cache and prompt-shaping fixes named in `docs/PARITY.md` follow v8.0.27 instead.
- When Go behaviour and documentation disagree, the Go code wins. Port behaviour, not structure: idiomatic Rust over transliterated Go.
- A deliberate difference from Go is listed in `docs/DIFFERENCES-FROM-GO.md`, and tests that compare against Go goldens encode it explicitly.

## Layout

- `crates/cpa-core`: config and credential file formats. No networking.
- `crates/cpa-common`: provider-neutral, Go-exact helpers shared by translators and executors.
- `crates/cpa-translate`: format translation between the Anthropic, OpenAI and Gemini APIs.
- `crates/cpa-exec`: one module per upstream provider (wire format, auth headers, HTTP client profile).
- `crates/cpa-server`: axum routes, client auth, credential selection, management API.
- `crates/cpa-store`, `crates/cpa-home`: remote config/auth stores and Home mode.
- `crates/cliproxy`: the binary. Go-style single-dash flags are accepted.
- `ui/` is the dashboard, `harness/` sends the same requests to Go and Rust and compares them.

## Rules

- Do not delegate unless the user explicitly requests it.
- Keep docs and comments concise and non-overlapping. Add feature comments only after implementation is complete. Keep progress logs and architecture reports outside the repository.
- Review upstream changes against this fork's scope; do not copy upstream operator instructions.

- Never commit private operational evidence: real account or organization details, quota/reset measurements, session identifiers or URLs, conversation/file contents, credential material, local user paths, or live test logs. Keep those outside the repository. Committed examples and fixtures must be synthetic and use generic account labels.

- The full test suite is slow and memory-heavy (the BoringSSL build and the release link). While iterating, run the affected crate or a filter, for example `cargo test -p cpa-exec kimi`; CI runs fmt, `clippy -D warnings` and the whole suite. On a shared or small machine, add `CARGO_BUILD_JOBS=2 nice -n 19`.
- PostgreSQL tests require local server binaries; CI runs them in a network-denied namespace. For a local Go/Rust differential run on Linux, use `./harness/run` (see `harness/README.md`).
- Request bodies are forwarded byte for byte unless a ported rule rewrites them. Never re-serialize JSON just to pass it through.
- Upstream credentials and client keys must never be logged or forwarded to the wrong side.
- Upstream request and response capture goes through the shared sink in `crates/cpa-server/src/request_logging.rs`. It redacts `Cookie` and `Set-Cookie` values, credentials in URLs and short keys, and spools large sections to disk. Don't format or log captured bytes anywhere else.
- Keep request preparation linear in the body size. Coding agents send bodies from 100 KB to several MB, so walk a JSON body once and edit by path (see `crates/cpa-exec/src/claude/signals.rs`) instead of searching it again from the start for each element.
- Performance and memory claims come from `docs/BENCHMARKS.md`. `bench/run.sh`, `bench/messages.sh` and `bench/latency.sh` reproduce its scripted workloads; field observations there are labelled as such and are not reproducible on demand.
- Tests use local mock upstreams and never send traffic to real provider accounts. Give every test a private `auth-dir`: without one, credential loading falls back to `~/.cli-proxy-api`, where real logins live. Build test runtimes with `cpa_server::testing::runtime`.
- Mark deliberate simplifications with a `ponytail:` comment naming the ceiling and the upgrade path.
- Prompt-cache changes must keep request semantics and stable prefixes, and show cache-read and cache-write usage for warm, changed-context and idle continuations before merge. Never add inference keepalive pings. `bench/claude-cache-live.sh` runs that check against a live Claude login, by hand.

## Review guidelines

Flag, in order: behaviour that differs from Go at `6fecc6e` without an entry in `docs/DIFFERENCES-FROM-GO.md`; request or response bytes changed on passthrough; credentials or client keys reaching logs or the wrong side; tests that can touch the network or a real `auth-dir`; work on a request body that grows faster than linearly with its size. Skip style nits that `cargo fmt` and clippy already cover.
