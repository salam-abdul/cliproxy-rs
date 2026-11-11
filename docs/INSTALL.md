# Local installation

Build this checkout. Upstream releases do not contain this fork's native routing.

## Prerequisites

| Need | For |
| --- | --- |
| Rust with edition 2024, at least the `rust-version` in `Cargo.toml` | Every build. CI pins the exact toolchain in `.github/workflows/ci.yml`. |
| CMake, Clang, libclang and Perl | BoringSSL, through `btls-sys` |
| NASM | Native Windows builds only |
| Node.js and npm | Dashboard development only |

```sh
cargo build --release --locked -p cliproxy
```

The dashboard assets in `ui/dist` are checked in and embedded in the binary; rebuild them only after changing `ui/src` ([ui/README.md](../ui/README.md)).

## Native Claude

Follow [the native guide](../native/README.md) for installation, Claude Code, Desktop and Zed setup and qualification. Its installer takes a local binary and keeps releases apart from private credentials and configuration.

Updates are manual. Build and qualify the candidate, then stop the installed service and replace its binary. Keep the previous release for rollback. Never run two credential-refresh owners against the same auth directory.

## Translating proxy

1. Create a private directory for the [configuration](CONFIGURATION.md), auth files and logs.
2. In the configuration, set `server.host: 127.0.0.1`, separate client and management keys, and an explicit `oauth.auth-dir`.
3. Start the binary with an absolute configuration path:

   ```sh
   ./target/release/cliproxy --config /path/to/config.yaml
   ```

4. Open `http://127.0.0.1:8317/management.html` (with your configured port) and sign in with the management key. With `management.local-dashboard: true` and a loopback `server.host` (`127.0.0.1` or `::1`), the dashboard opens without the key.

Client keys go in client settings; never share either key. For access from another machine, bind to a private interface, require client keys and list trusted proxies explicitly. `--local-model` turns off model catalog downloads.

## Container

CI publishes `ghcr.io/salam-abdul/cliproxy-rs` for linux/amd64 and linux/arm64 after the tests pass on `main` (`.github/workflows/docker.yml`). You can also run the workflow by hand from `main`, which publishes without waiting for CI. Tags are `sha-<first 7 characters of the commit>` and `latest`, which moves only to a published commit that is still the tip of `main`. While the package is private, run `docker login ghcr.io` first.

```sh
docker run --rm -p 127.0.0.1:8317:8317 -v /path/to/private/data:/data ghcr.io/salam-abdul/cliproxy-rs:latest
```

To build the same image locally:

```sh
docker build -t cliproxy-local .
docker run --rm -p 127.0.0.1:8317:8317 -v /path/to/private/data:/data cliproxy-local
```

The image runs as UID and GID 10001 and reads `/data/config.yaml`. Give that identity access to the mounted directory, and set a listener address the port mapping can reach.
