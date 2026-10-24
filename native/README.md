# Native Claude routing

The Rust binary owns the transparent CONNECT transport, account sign-in and refresh, quota admission, persistent session affinity, certificate renewal and dashboard counters. There is no Node routing daemon or Python supervisor. The small management command and Zed ACP adapter use Node only when invoked.

Inference bodies, system prompts, models, tools, history, cache markers and beta headers pass through unchanged. Non-inference Anthropic requests retain the native login for Remote Control. A conversation stays on its account until it is disabled, unavailable or out of included quota, then stays on the replacement. Inference uses a ten-minute read timeout, matching the native SDK timeout allowance. Ambiguous lost responses and partial streams are never automatically replayed. Local transport-failure diagnostics record only an error classification and elapsed time, without raw error strings or request contents.

New sessions choose the eligible account whose confirmed weekly reset is soonest. Existing sessions stay pinned, even when another account resets sooner; failover chooses the soonest eligible replacement and persists that new pin. Unknown or past reset dates rank last, with the configured preferred account breaking ties.

New sessions prefer at least `newSessionMinRemaining` percent remaining (default 5, range 0–50) when an eligible alternative exists. This preference does not displace an eligible pinned session or reject the only eligible account. Set `selectionShadow: true` in the native JSON configuration to record this preference without changing reset-first selection. Restart to apply native configuration changes.

Explicit 401/403/429 rejections cool the account; 503/529 cool only the account/model pair. Valid `Retry-After` delays are bounded to 1 second–15 minutes. Otherwise quota cooldowns last 60 seconds and other cooldowns 15 seconds. At 128 live model cooldowns on one account, further model overloads temporarily cool the whole account. Each request tries at most `maxAttempts` accounts (default 3, range 1–16), with a 60-second admission deadline. Bare 404s are reported without replay or model exclusion.

The routing policy checks five-hour, weekly and applicable model limits. **Allow paid overage** in the native dashboard defaults off and saves immediately through `PATCH /v8/management/routing/native-policy` (`{"allowExtraUsage":true|false}`). Account billing settings need not change: enabled accounts with included quota are always preferred. With the toggle on, confirmed available extra credits can be used only when no included account is usable. Disabled accounts remain excluded. Usage reporting and concurrent requests can cross a limit before it is observed; this guard is not an absolute billing cap.

Exhausted response headers override cached quota and affinity on the next request. Usage snapshots expire after 30 seconds or at their next reset, whichever comes first; elapsed time alone never establishes available quota. When possible, a local 429 carries Claude's unified reset/limit headers and `Retry-After` for the earliest enabled account's recovery (after all its blocking windows). Claude Code's [interactive automatic continuation](https://code.claude.com/docs/en/interactive-mode#wait-for-a-usage-limit-to-reset) owns resuming the task. Remote Control requires starting that wait manually; background and print sessions do not offer it. The proxy neither holds requests for hours nor resubmits conversation turns. No inference keepalive requests are sent. Failover can have one cold cache.

## Diagnostics and retention

The dashboard’s Usage page shows recent native selections, failover attempts and final outcomes through the authenticated, nondestructive `GET /v8/management/observability/native/events` route. It retains 512 events in memory since startup. Stream errors, missing `message_stop`, transport failures and client cancellations are distinct; an HTTP 200 alone does not establish streaming success. Observer failures are reported as incomplete observation. Provider errors retain only allowlisted types; usage retains only known numeric token counters.

Timing fields separate body preparation, session waiting, quota admission, upstream headers across attempts and response streaming. First-byte time is measured from request arrival and may precede generated text; it is not time to first token. These fields describe elapsed time, not isolated CPU cost.

Event writes use a background thread with a 256-event queue and an 8 KiB event limit. The current file rotates at 8 MiB and retains one previous file. The view reports dropped or unwritten events; diagnostics do not block inference on disk I/O. Routing changes still wait for atomic durable persistence on a blocking worker. Affinity retains up to 4,096 bindings, expires bindings after 30 idle days and checkpoints activity hourly. Active bindings are protected from eviction; legacy bindings receive an initial activity timestamp when first loaded for selection.

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

`refresh` runs through the same Rust owner as automatic refresh. The management command never exchanges refresh tokens. The dashboard manages this owner’s accounts and displays native request activity. A separate proxy with another auth directory has independent switches; use `cliproxy-native dashboard` for native Claude accounts. The dashboard identifies its scope through `GET /v8/management/observability/native/status`. Enable `observability.usage.usage-statistics-enabled` for the Usage page's provider counters; the installer enables it on new setups. `status` also shows recent cache counters and persistent affinity, without making model requests.

The native transport runs inside the same Rust process as the API server. To attach it to an existing server, set `native-claude.config-file` in that server's YAML and point the native JSON's `managerConfig`, `managementUrl`, `managementKeyFile` and `authDir` at that same owner. Preserve native account IDs when consolidating credentials so existing affinity remains valid; stop both refresh owners before moving files. Disable the retired service to prevent duplicate refreshes.

The standalone install defaults to management20148; a combined installation uses its existing dashboard port. TLS health/status uses20149 and authenticated HTTP CONNECT20152. The Claude launcher sets `HTTPS_PROXY` and process-scoped `NODE_EXTRA_CA_CERTS`; it retains `api.anthropic.com` as the endpoint. No system certificate trust changes are needed. Permission mode remains Claude's setting.

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

For a server that accepts work from your phone, run `claude rc` (or `claude remote-control`) in the project directory. Remove legacy gateway `ANTHROPIC_BASE_URL` and `apiKeyHelper` overrides from Claude settings first; server mode uses the native Claude login and its child sessions inherit the launcher's proxy environment. Server mode rejects the `--settings` override used by the interactive launcher. Keep the terminal running and connect from the Code tab on your phone.

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
