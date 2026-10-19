# Moving from Go

This personal fork reads legacy and v8 configuration and built-in provider credential files. It preserves useful compatibility, not every upstream extension. Review [scope](PARITY.md) and [deliberate differences](DIFFERENCES-FROM-GO.md) first.

## Switch

1. Stop the Go server. Keep a private backup of configuration and credentials.
2. Remove `plugins.enabled: true` and plugin CLI flags. Replace any plugin-dependent routing with built-in providers.
3. Build [from source](INSTALL.md), then start `cliproxy --config /path/to/config.yaml`.
4. Check credentials in `/management.html` and qualify the client workflows you use. Native Claude has [separate installation and qualification](../native/README.md).

Only one server may refresh a given account's OAuth tokens. A copied auth directory does not isolate refresh tokens; use synthetic credentials and local mocks for tests.

## Compatibility

- Built-in config, credential, client-key and management-key formats carry over. Management writes preserve unrelated text and migrate legacy settings to v8.
- Built-in provider routes, translation, streaming, retries, cooldowns, aliases and remote stores retain their tests. Plugin execution and management endpoints are absent.
- The dashboard is embedded. Panel download settings have no effect. Latest-version endpoints return `501 source_build_only` without contacting a release service.
- Home retains configuration, credentials, usage and shared state; plugin synchronization, tasks and status are absent.
- `pprof`, dashboard Antigravity login and dashboard Vertex import remain unsupported. See `cliproxy --help` for available CLI paths.

## Switch back

Stop this server before starting Go with the same files. Credentials use upstream formats, but a backup's refresh tokens may already have rotated. Go treats `soonest-reset` as round-robin and ignores this fork's native transport and Codex keep-alive settings. Keep a separate configuration when using fork-specific features.
