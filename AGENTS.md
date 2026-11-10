# cliproxy-rs

A private Rust fork of CLIProxyAPI. One binary serves a translating proxy (Anthropic, OpenAI and Gemini endpoints over built-in providers), native Claude routing for Claude Code, Desktop and Zed, and an embedded management dashboard.

| Path | Owns |
| --- | --- |
| `crates/cpa-core` | Configuration and credential formats. No networking. |
| `crates/cpa-common` | Provider-neutral helpers, including Go string, URL and JSON semantics. |
| `crates/cpa-translate` | Anthropic, OpenAI and Gemini translation. |
| `crates/cpa-exec` | Provider wire formats, authentication and HTTP transports. |
| `crates/cpa-server` | Routes, client authentication, credential selection, native Claude routing and management. |
| `crates/cpa-store`, `crates/cpa-home` | Remote configuration and credential stores, and Home mode. |
| `crates/cliproxy` | Binary and CLI. |
| `ui/` | Dashboard. `ui/dist` is checked in and embedded in the binary. |
| `native/` | Native client installer, launcher, management command and Zed ACP adapter. |
| `vendor/` | Patched third-party crates. |

## Rules

These hold for every task. A direct instruction from the user for the task wins over them: say once that you are following it.

- **Fork scope.** Preserve native Claude CLI, Desktop and Zed workflows and the existing built-in providers. Port upstream fixes selectively and record reviewed-through hashes and pending decisions in `docs/UPSTREAM-REVIEW.md`. Never add Go code, differential harnesses, upstream parity corpora or benchmark reports.
- **Running installation.** The installed service, its auth directory and `~/.config/cliproxy-native` and `~/.local/state/cliproxy-native` belong to the user. Do not stop, replace or reconfigure them, or sign accounts in or out, without an explicit request. Never run a second credential-refresh owner against an auth directory. Never publish packages, releases or images.
- **Byte fidelity.** Forward request and response bytes unchanged unless a deliberate rule requires an edit, and then edit by path. Never re-serialize JSON to pass it through.
- **No replay.** Never resend inference once the upstream may have received it: after an ambiguous response, a partial stream, a transport failure or a cancellation after the send. Failover happens only before the request reached the upstream.
- **Isolation and admission.** Keep each credential's tokens, cooldowns and proxy settings separate. Authorization fails closed, and native quota admission uses fresh state: unknown quota refuses rather than guessing.
- **Logs.** Upstream capture goes only through `crates/cpa-server/src/request_logging.rs`, which redacts sensitive headers and URLs and spools large sections, and its `request_logging/history` module, which stores bodies and never headers, URLs, tokens or keys. Never log captured bytes, tokens or keys anywhere else.
- **Private data.** Never commit credentials, account or organization details, quota records, sessions, conversations, local user paths or live test logs. Fixtures are synthetic.
- **Failing tests.** Never change an expected value, or loosen, skip or delete a test, to make a failure go away. Find the cause; if the test itself is wrong, show the evidence.
- **Scope.** Add no doc, comment, refactor or file the task does not need. A config key, CLI flag, route, dashboard element, dependency or workflow that the request does not name needs approval first: say what it is for and which existing path you checked.
- **Code text.** No plan IDs, ticket numbers or "see the plan" in code, comments, test names or docs. Write the reason itself. Plans, progress logs and reports stay outside the repo.
- **Evidence.** Say "works", "fixed", "safe", "covered" or "pre-existing" only with the command output or file:line that shows it, from a check that could have failed. Name what you could not check. A subagent's report is a claim: check it on disk before you act on it.
- **Delegation.** Delegate only when the user asks. Default to one agent and never run more than three for a task without a new request. Agents do not delegate further, use medium reasoning unless the user asks for more, and review only when asked.
- **Writing.** Lead with the point. No acknowledgment, praise, filler or em dashes.

Read these when the task calls for them:

| Read | When |
| --- | --- |
| [`rust.md`](.agents/rules/rust.md) | Changing or reviewing Rust code |
| [`testing.md`](.agents/rules/testing.md) | Writing, running or debugging tests, and before any commit |
| [`writing.md`](.agents/rules/writing.md) | Writing docs, comments, commit messages or reports |

## Commands

| Command | Use |
| --- | --- |
| `CARGO_BUILD_JOBS=2 cargo test -p <crate> <filter>` | Iterate on the crates you touch. The whole suite is slow and memory-heavy. |
| `cargo fmt --all --check` | Format gate. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Lint gate. CI also runs it for `x86_64-pc-windows-msvc`. |
| `cargo nextest run --workspace --locked --profile ci` | The CI test run. `.config/nextest.toml` runs the heap gate and wall-clock tests alone. |
| `cargo test --locked --manifest-path vendor/gjson/Cargo.toml` | Tests of the patched gjson, which is outside the workspace. |
| `npm ci && npm run check && npm test && npm run build` (in `ui/`) | Dashboard type check, unit tests, bundle and size budgets. |
| `node scripts/dashboard-check.mjs` (in `ui/`) | Dashboard browser checks with every request mocked. |
| `python3 native/test.py` | Native routing wire tests. Build the binaries first (`native/README.md`). |
| `tools/claude-cache-live.sh` | Live prompt-cache qualification. Spends real tokens; run it only by hand. |

## Docs

Operation docs live only in these files. When a behavior changes, update the page that owns it.

- [README](README.md): what the fork is, build and run, development checks.
- [Configuration](docs/CONFIGURATION.md): settings, routing and environment variables.
- [Installation](docs/INSTALL.md): prerequisites, builds, manual updates and the local container.
- [Native Claude](native/README.md): routing behavior, install, state and qualification.
- [Upstream review](docs/UPSTREAM-REVIEW.md): reviewed upstream hashes and pending ports.
- [Dashboard](ui/README.md): build, budgets and page layout.
- [Patched crates](vendor/README.md): what each vendored crate changes.
