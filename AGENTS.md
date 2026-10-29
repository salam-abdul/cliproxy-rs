# cliproxy-rs

A private personal fork. Preserve native Claude CLI/Desktop/Zed workflows and existing
built-in providers. Do not publish packages, releases or images, or replace the running
installation without an explicit request.

## Scope and layout

Production behavior and focused Rust regressions define this fork. Reuse upstream fixes
selectively; record reviewed-through hashes and unfinished work in
`docs/UPSTREAM-REVIEW.md`. Do not add Go code, differential harnesses, upstream parity
corpora or historical benchmark reports.

- `cpa-core`: configuration and credential formats; no networking.
- `cpa-common`: shared provider-neutral helpers.
- `cpa-translate`: Anthropic, OpenAI and Gemini translation.
- `cpa-exec`: provider wire formats, authentication and HTTP transports.
- `cpa-server`: routes, client authentication, credential selection and management.
- `cpa-store`, `cpa-home`: remote stores and Home mode.
- `cliproxy`: binary and CLI; `ui/`: dashboard; `native/`: native client integration.

Keep operation docs in README, `docs/CONFIGURATION.md`, `docs/INSTALL.md` and
`native/README.md`. Keep plans, progress logs and architecture reports outside the repo.
Comments should explain completed behavior and non-obvious constraints concisely.

## Delegation

- Delegate only when the user explicitly requests it; use the fewest agents needed.
- Default to one focused agent; never create more than three direct agents for a task
  without a new explicit request. Agents must not delegate further.
- Use medium reasoning for authorized agents unless the user requests higher.
- Run review agents only when explicitly requested.

## Behavior and safety

- Forward request/response bytes unchanged unless a deliberate rule requires edits.
  Never re-serialize JSON just to pass it through.
- Preserve credential isolation, fail-closed authorization and fresh native quota
  admission. Never replay inference after an ambiguous response or transport failure.
- Request preparation must be linear in body size: walk once and edit by path.
  `crates/cpa-exec/src/claude/signals.rs` is an example.
- Upstream capture uses `crates/cpa-server/src/request_logging.rs`, which redacts
  sensitive headers/URLs and spools large sections. Never log captured bytes elsewhere.
- Never commit credentials, real account/organization details, private quota records,
  sessions, conversations, local user paths or live test logs. Fixtures are synthetic.
- Mark deliberate implementation ceilings with a `ponytail:` comment and an upgrade path.

## Validation

Use focused Rust tests and local mocks. Every server test gets a private `auth-dir` and
uses `cpa_server::testing::runtime`; never fall back to real credentials or provider
accounts. PostgreSQL tests need local server binaries; CI denies external networking.

The whole suite is slow and memory-heavy. Iterate on affected crates or filters, e.g.
`CARGO_BUILD_JOBS=2 cargo test -p cpa-exec kimi`. CI runs formatting, clippy with warnings
as errors and the workspace tests. Keep meaningful resource bounds and regressions;
performance claims require a controlled measurement of this fork.

Prompt-cache changes preserve semantics and stable prefixes. Before merge, manually
qualify warm, changed-context and idle continuations with returned cache-read/write
usage using `tools/claude-cache-live.sh`. Never send inference keepalive pings.

Review byte fidelity, credential/log isolation, authorization and no-replay behavior,
test isolation and superlinear body processing, in that order. Skip formatter/linter nits.
