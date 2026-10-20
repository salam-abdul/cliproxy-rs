# Local installation

Build this checkout. Public upstream releases do not contain this fork's native routing changes.

## Prerequisites

Use Rust with edition 2024 support and the version required by `Cargo.toml`. BoringSSL needs CMake, Clang, libclang and Perl; native Windows builds also need NASM. Dashboard development requires Node.js and npm.

```sh
cargo build --release --locked -p cliproxy
```

Checked-in `ui/dist` assets are embedded in the binary. Rebuild them after UI changes:

```sh
cd ui
npm ci
npm run check
npm test
npm run build
```

## Native Claude

Follow [the native guide](../native/README.md) for installation, certificate trust, Claude/Desktop/Zed setup and local qualification. Its installer accepts a local binary and keeps releases separate from private credentials and configuration.

Updates are manual. Build and qualify the candidate before stopping the installed service and replacing its binary. Keep the previous local release for rollback. Never run two credential refresh owners against the same auth directory.

## Translating proxy

Create a private directory for the [configuration](CONFIGURATION.md), auth files and logs. Set `server.host: 127.0.0.1`, separate client and management keys, and an explicit `oauth.auth-dir`. Start the binary with an absolute configuration path:

```sh
./target/release/cliproxy --config /path/to/config.yaml
```

Open `http://127.0.0.1:8317/management.html`, using the configured port. For automatic local access, set `management.local-dashboard: true` with `server.host: 127.0.0.1` (or `::1`). Otherwise, sign in with the management key. Client keys still belong in client settings; never share either key.

If another machine needs access, use the [remote-access guidance](MULTI-ACCOUNT.md#reach-the-proxy-from-other-machines). Restrict trusted proxies explicitly. Model catalog downloads can be disabled with `--local-model`.

## Local container

```sh
docker build -t cliproxy-local .
docker run --rm -p 127.0.0.1:8317:8317 -v /path/to/private/data:/data cliproxy-local
```

The image runs as UID/GID 10001 and reads `/data/config.yaml`. Grant that identity access to the mounted directory and use a container listener reachable through the port mapping. The Dockerfile does not create or publish a registry image.
