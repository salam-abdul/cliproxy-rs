# Rust

## Find callers with the language server

Before changing a trait method, a shared signature, or anything on a seam with a twin, find every caller with the language server: `findReferences`, `goToImplementation`, `incomingCalls`. Grep finds a string, not the symbol: it misses trait impls, generic instantiations, renaming re-exports and macro-generated calls. The usual bug here is a rule applied at one seam and not its twin:

- local dispatch (`crates/cpa-server/src/dispatch.rs`) and Home dispatch (`crates/cpa-server/src/remote.rs`);
- each protocol pair in `cpa-translate`, request and response;
- each provider executor in `cpa-exec`, streaming and non-streaming;
- the translating proxy and native Claude routing.

Grep is still right for a literal, a log string or a config key. If the language server is down, say so and treat the grep result as a lower bound.

## Request bodies

- Preparation is linear in body size: walk the body once and edit by path. `crates/cpa-exec/src/claude/signals.rs` is the model. Parse a body once per request, not once per rule.
- Prompt-cache changes preserve semantics and stable prefixes. Never send inference keepalive pings.

## Design

- Reuse before you write. Go string, URL and JSON semantics live in `cpa-common` (`gostr`, `json`) and `cpa-core` (`config/go_url.rs`, and `go_trim_space` in `config/trusted.rs`). When you find two copies of a helper, keep the correct one in the more generic crate.
- Make a wrong state unrepresentable instead of guarding it: an enum for a closed set of strings, a newtype for an ID, a typed `Config` field with its default in `cpa-core` instead of a raw YAML lookup at the use site.
- No `unwrap`, `expect`, indexing or `panic!` on a path that a request, a file or a config value can reach. Lock with `unwrap_or_else(PoisonError::into_inner)`, as the rest of `cpa-server` does.
- Keep work out of shared locks: compute and allocate before you take the scheduler or store lock. Keep blocking file I/O off async workers.
- Group functions that share state or the same five arguments into a struct with methods. Move a cohesive concept out of a large file into its own module, and large inline test modules into a sibling `*_tests.rs`.
- `ponytail:` marks a deliberate implementation ceiling and names its upgrade path.

## Review order

Byte fidelity, credential and log isolation, authorization and no-replay, test isolation, then superlinear body processing. Skip formatter and linter nits: CI enforces them.
