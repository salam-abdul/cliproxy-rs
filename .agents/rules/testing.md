# Testing and debugging

## Diagnose before fixing

- Unless the error names its cause, as a compile or lint error does, write down the hypotheses, the evidence for each, and a check that would disprove the leading one. Then run that check, read-only where you can.
- Explain why the similar cases that work do not fail; that difference is the lead.
- A retry, a longer timeout or a cleared cache is not a fix for a failure you have not explained.

## Isolation

- Every server test builds its runtime with `cpa_server::testing::runtime` and a private `auth-dir` in a temp directory. That runtime sends every non-local credential to a dead loopback proxy. Never let a test resolve the default auth directory or read real credentials.
- Upstreams are local mocks on loopback. CI runs the tests in a network namespace with only loopback up, so a test that reaches a real host fails there.
- PostgreSQL store tests start a throwaway cluster from local server binaries; set `CPA_TEST_PG_BIN` to their directory. Without binaries they skip and pass. `CPA_TEST_NO_SKIP=1` turns that skip into a failure, as CI does.

## Traps

- Nextest skips doc tests. CI runs `cargo test --workspace --doc` after it.
- The heap gate (`crates/cpa-server/tests/alloc_budget.rs`, budgets in `tests/fixtures/allocation_limits.txt`) and the tests that bound wall-clock time run alone only when `.config/nextest.toml` lists them. Add a new timing test there.
- The heap budgets are measured on the toolchain pinned in `.github/workflows/ci.yml` (`RUST_TOOLCHAIN`). Re-measure them when it changes.
- The Codex Live media relay tests run only with the feature on: `cargo nextest run -p cpa-server --features media-relay --lib`.
- Code behind `cfg(windows)` is checked only by the Windows clippy job.
- `vendor/gjson` is outside the workspace, so `--workspace` commands skip it.
- `ui/dist` is checked in. CI rebuilds it and fails on any difference, so commit the rebuilt assets with the source.
- `python3 native/test.py` runs `target/debug/cliproxy` and the `native_router_fixture` example. Rebuild both first or it tests stale code.
- Before merging a prompt-cache change, qualify warm, changed-context and idle continuations with `tools/claude-cache-live.sh` and read the returned cache-read and cache-write usage.

## Tests that can fail

- When you add a test for a guard, a refusal or a claimed behavior, prove it fails without the code it covers: revert that code by hand, watch the test fail on an assertion, then restore it.
- The fixture must contain the case under test. An expected value equal to the unfiltered baseline proves nothing.
- A test asserts an outcome: a response, a refusal, bytes on the wire, a persisted file. A test that only checks which mock ran, or passes because nothing panicked, asserts nothing.
- Flaky by construction: a port bound, dropped and bound again; a wall-clock dependence outside the serialized list; an assertion on the order of an unordered collection.
- Test names state the guarantee (`a_lost_response_is_not_replayed`), not the bug report.

## Before commit

- Run `cargo fmt --all --check`, clippy with `-D warnings`, and the tests of every crate you touched, on the tree you will commit.
- Name each touched suite you could not run, with the reason, in your report and the commit or PR description.
