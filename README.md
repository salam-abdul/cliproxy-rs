# cliproxy-rs

A personal Rust fork of [cliproxy-rs](https://github.com/vayungodara/cliproxy-rs), based on [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). Build and update it from source; this fork does not publish binaries or packages.

## Workflows

- **Native Claude:** one conversation stays on one eligible subscription account across Claude Code, Desktop and Zed ACP. The CONNECT transport preserves request bodies, model names, beta headers and Remote Control. Unknown quota fails closed; ambiguous transport failures are not replayed.
- **Translating proxy:** the existing Anthropic, OpenAI and Gemini endpoints support built-in providers, API keys, streaming, account selection and failover.
- **Local management:** the embedded dashboard manages accounts, configuration, usage and client setup.

Native Claude setup is in [native/README.md](native/README.md). Custom base URLs use the translating proxy and have different semantics.

## Build and run

```sh
cargo build --release --locked -p cliproxy
./target/release/cliproxy --config /path/to/config.yaml
```

The dashboard assets are checked in. After changing the dashboard, run `npm ci` and `npm run build` in `ui/` before building Rust. See [installation](docs/INSTALL.md) for prerequisites and local updates.

Keep the listener on loopback, configure separate client and management keys, and protect the credential directory. Use one refresh owner per auth directory. Live credentials, conversations and operational records stay outside this repository.

## Documentation

- [Configuration](docs/CONFIGURATION.md): settings, routing and environment variables.
- [Local installation](docs/INSTALL.md): builds and manual updates.
- [Native Claude](native/README.md): Claude Code, Desktop and Zed setup and qualification.
- [Upstream checkpoints](docs/UPSTREAM-REVIEW.md): reviewed hashes and remaining ports.

## Development

Use focused Rust regression tests with private auth directories and local mock
upstreams. CI checks the workspace with external networking denied.

```sh
cargo fmt --all --check
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 cargo test --workspace --locked
```

MIT. Preserve [LICENSE](LICENSE) and third-party notices.
