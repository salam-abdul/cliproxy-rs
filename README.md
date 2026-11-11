# cliproxy-rs

A personal Rust fork of [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs), itself based on [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). Build it from source or run the container image CI publishes to GHCR ([installation](docs/INSTALL.md#container)); this fork publishes no binaries or packages.

## What it does

- **Native Claude routing.** One Claude Code, Desktop or Zed conversation stays on one eligible subscription account. Request bodies, model names, beta headers and Remote Control pass through unchanged. Unknown quota fails closed, and an ambiguous transport failure is never replayed. Setup: [native/README.md](native/README.md).
- **Translating proxy.** Anthropic, OpenAI and Gemini endpoints over built-in providers and API keys, with streaming, account selection and failover. Custom base URLs use this path, with different semantics from native routing.
- **Local management.** The embedded dashboard manages accounts, configuration, usage and client setup.

## Build and run

```sh
cargo build --release --locked -p cliproxy
./target/release/cliproxy --config /path/to/config.yaml
```

Keep the listener on loopback, configure separate client and management keys, and protect the credential directory. Use one credential-refresh owner per auth directory. Live credentials, conversations and operational records stay outside this repository.

## Documentation

| Page | Covers |
| --- | --- |
| [Configuration](docs/CONFIGURATION.md) | Settings, routing and environment variables |
| [Installation](docs/INSTALL.md) | Prerequisites, builds, manual updates and the local container |
| [Native Claude](native/README.md) | Claude Code, Desktop and Zed setup, routing behavior and qualification |
| [Dashboard](ui/README.md) | Building the embedded dashboard |
| [Upstream review](docs/UPSTREAM-REVIEW.md) | Reviewed upstream hashes and pending ports |
| [Patched crates](vendor/README.md) | Vendored third-party crates and their changes |

## Development

Tests use private auth directories and local mock upstreams; CI runs them with external networking denied. These are the checks CI runs, scoped down for local use:

```sh
cargo fmt --all --check
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 cargo test -p <crate> <filter>
```

The dashboard's built assets in `ui/dist` are checked in and embedded. After changing `ui/src`, rebuild them as described in [ui/README.md](ui/README.md) and commit the result. Contributor and agent rules are in [AGENTS.md](AGENTS.md).

MIT. Preserve [LICENSE](LICENSE) and third-party notices.
