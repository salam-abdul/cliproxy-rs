# Native Claude routing

The Rust binary owns the transparent CONNECT transport, account sign-in and refresh, quota admission, persistent session affinity, certificate renewal and dashboard counters. There is no Node routing daemon or Python supervisor. The small management command and Zed ACP adapter use Node only when invoked.

Inference bodies, system prompts, models, tools, history, cache markers and beta headers pass through unchanged. Non-inference Anthropic requests retain the native login for Remote Control. A conversation stays on its account until it is disabled, unavailable or out of included quota, then stays on the replacement. Inference uses a ten-minute read timeout, matching the native SDK timeout allowance. Ambiguous lost responses and partial streams are never automatically replayed. Local transport-failure diagnostics record only an error classification and elapsed time, without raw error strings or request contents.

New sessions choose the eligible account whose confirmed weekly reset is soonest. Existing sessions stay pinned, even when another account resets sooner; failover chooses the soonest eligible replacement and persists that new pin. Unknown or past reset dates rank last, with the configured preferred account breaking ties.

The routing policy uses fresh five-hour, weekly and applicable model limits. Paid extra must be explicitly disabled or confirmed fully spent; unknown or spendable extras fail closed. No inference keepalive requests are sent. Cache state belongs to the provider account, so a genuine failover may have one cold cache.

## Install and manage

Build this fork, not the upstream release:

```sh
cargo build --release -p cliproxy
python3 native/install.py target/release/cliproxy
```

The installer prints the pinned Rust binary and its configuration path. Start that binary with the printed `--config` argument and `--local-model`, or use it directly as your login service's executable. The installer does not change shell or Zed defaults. Sign into Claude Code normally for Remote Control, then sign the inference accounts into the proxy:

```sh
cliproxy-native login
cliproxy-native accounts
cliproxy-native rename ACCOUNT "Account-A"
cliproxy-native enable Account-A
cliproxy-native disable Account-A
cliproxy-native refresh Account-A
cliproxy-native status
cliproxy-native dashboard --copy-key
```

`refresh` runs through the same Rust owner as automatic refresh. The management command never exchanges refresh tokens. The dashboard manages accounts and displays native request activity. Enable `observability.usage.usage-statistics-enabled` for the Usage page's provider counters; the installer enables it on new setups. `status` also shows recent cache counters and persistent affinity, without making model requests.

The native transport is enabled by `native-claude.config-file` in the manager's YAML configuration. Management binds loopback20148, TLS health/status20149, authenticated HTTP CONNECT20152. The Claude launcher sets `HTTPS_PROXY` and process-scoped `NODE_EXTRA_CA_CERTS`; it retains `api.anthropic.com` as the endpoint. No system certificate trust changes are needed. Permission mode remains Claude's setting.

## Zed Claude ACP and automatic Remote Control

Build the pinned adapter from official packages and the small Remote Control patch:

```sh
python3 native/acp/install.py
```

It pins ACP0.89.1 and Claude Agent SDK0.3.293 with a lockfile and verifies the upstream bytes before patching. The SDK itself is unchanged. The adapter calls its native `enableRemoteControl` method for new and resumed sessions and starts the consumer for phone messages. The qualified native CLI version is2.1.296; upgrades need qualification.

After a small routing/cache/Remote Control check, point `~/.local/share/cliproxy-native/current` at the installed Rust release. Use the absolute ACP launcher path printed by the installer in Zed's `~/.config/zed/settings.json`:

```json
{
  "agent_servers": {
    "claude-acp": {
      "type": "custom",
      "command": "/absolute/path/to/cliproxy-native/acp/releases/0.89.1-native1/launch",
      "args": [],
      "env": {},
      "default_config_options": {
        "mode": "auto",
        "model": "opus",
        "effort": "medium"
      }
    }
  }
}
```

Set `remoteControlAtStartup: true` in Claude settings. In Zed choose **Claude Agent** under External Agents; saved ACP threads resume their native conversation. The dashboard's **Use with tools** page shows the configured launcher path. If using a different ACP release, update `native-claude.acp-launcher` in the manager configuration as well.

Do not replace Anthropic with the proxy's management address in `ANTHROPIC_BASE_URL`: the generic SDK/custom-endpoint examples use the separate translating executor and do not preserve native Remote Control.

## State, upgrades and verification

Configuration and account names/preferences: `~/.config/cliproxy-native/`. Private OAuth files, keys, affinity JSON and sanitized routing events: `~/.local/state/cliproxy-native/`. Immutable releases: `~/.local/share/cliproxy-native/releases/`. OAuth files use upstream JSON protected by directory0700/file0600. Never commit private state.

Updates are manual and pinned. Stop the login service, then run `python3 native/install.py target/release/cliproxy --upgrade`. This preserves credentials/settings, migrates the predecessor's SQLite pins and cooldowns to atomic JSON, and prints the new executable. Rust refuses an unmigrated SQLite configuration. The Rust process holds the exclusive refresh-owner lock before starting automatic refresh. Do not run another refresh owner against the same auth directory.

Qualification uses private loopback fixtures:

```sh
cargo build -p cliproxy --bin cliproxy -p cpa-server --example native_router_fixture
cargo test -p cpa-server native_claude --lib
python3 native/test.py
```

The wire tests cover byte-preserving bodies and beta headers, credential separation, compressed replies, small/large SSE and JSON counters, cancellation, no replay after lost/partial responses, manager toggles, token rotation and affinity across restart. Live cache and phone control require a small separate qualification using included quota. Keep live qualification records outside the repository; use synthetic fixtures for committed tests.
