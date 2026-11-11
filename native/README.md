# Native Claude routing

Native routing keeps one Claude Code, Desktop or Zed conversation on one subscription account. It runs inside the Rust binary as a transparent CONNECT transport for `api.anthropic.com`, and owns account sign-in and refresh, quota admission, persistent session affinity, certificate renewal and dashboard counters. Node runs only for the management command and the Zed ACP adapter, and only when invoked.

## What reaches Anthropic

- Inference bodies, system prompts, models, tools, history, cache markers and beta headers pass through unchanged.
- Non-inference requests keep the native Claude login, so Remote Control works.
- Inference uses a ten-minute read timeout, the native SDK's allowance.
- A lost response or partial stream is never replayed. The proxy neither holds requests until quota resets nor resubmits conversation turns, and sends no inference keepalive requests.
- Transport-failure diagnostics record only an error class and elapsed time, never raw error strings or request contents.

## Account selection

- **New sessions** choose the eligible account whose confirmed weekly reset is soonest. Unknown or past reset dates rank last; the configured preferred account breaks ties.
- **Minimum remaining quota.** New sessions prefer an account with at least `newSessionMinRemaining` percent remaining when an eligible alternative exists. This never displaces an eligible pinned session or rejects the only eligible account. `selectionShadow: true` records the preference without applying it.
- **Existing sessions** stay pinned to their account until it is disabled, unavailable or out of included quota, even when another account resets sooner. Failover chooses the soonest eligible replacement and pins the session to it. Failover can cost one cold prompt cache.
- **Limits checked:** five-hour, weekly and applicable model limits. Exhausted response headers override cached quota and affinity on the next request. Elapsed time alone never establishes that quota is available.
- **Paid overage.** The dashboard's **Allow paid overage** switch defaults to off and saves at once through `PATCH /v8/management/routing/native-policy` with `{"allowExtraUsage": true}` or `false`. When on, confirmed extra credits are used only when no account with included quota is usable. Disabled accounts stay excluded. Usage reporting and concurrent requests can cross a limit before it is observed, so this is not an absolute billing cap.

Restart the binary to apply native configuration changes.

| Setting or rule | Value |
| --- | --- |
| `newSessionMinRemaining` | Default 5, range 0 to 50 percent |
| `maxAttempts` (accounts tried per request) | Default 3, range 1 to 16 |
| Admission deadline | 60 seconds |
| Cooldown on explicit 401, 403 or 429 | Whole account |
| Quota reset used from the dashboard | Clears the account's cooldowns and stored limit readings |
| Cooldown on 503 or 529 | Account and model pair; at 128 live model cooldowns on one account, further overloads cool the whole account |
| Valid `Retry-After` | Bounded to 1 second to 15 minutes |
| Cooldown without `Retry-After` | 60 seconds for quota, 15 seconds otherwise |
| Bare 404 | Reported, without replay or model exclusion |
| Usage snapshot lifetime | 30 seconds or the next reset, whichever comes first |

When possible, a local 429 carries Claude's unified reset and limit headers and a `Retry-After` for the earliest account's recovery, after all its blocking windows. Claude Code's [interactive automatic continuation](https://code.claude.com/docs/en/interactive-mode#wait-for-a-usage-limit-to-reset) resumes the task. Remote Control needs that wait started by hand; background and print sessions do not offer it.

## Diagnostics and history

**Activity** reads `GET /v8/management/observability/native/events`: recent selections, failover attempts and final outcomes. Stream errors, a missing `message_stop`, transport failures and client cancellations are reported separately, because an HTTP 200 alone does not establish that a stream succeeded. Observer failures show as incomplete observation. Provider errors keep only allowlisted types, and usage keeps only known numeric token counters.

**Overview** reads `GET /v8/management/observability/native/history?range=24h|7d|30d` from a private SQLite file beside the event log. It keeps final request outcomes, token counters and account availability, and excludes sessions, request bodies and body fingerprints. Account status is sampled without inference calls. An unavailable history file does not stop routing. History starts when this version first runs; older event logs are not backfilled. The dashboard marks incomplete token totals, dropped records and possible gaps after an unclean shutdown.

With `observability.history` on, every native send is also recorded with its request and response bodies in the model-traffic history ([Configuration](../docs/CONFIGURATION.md#history)). The SQLite history above stays free of content either way.

Timing fields separate body preparation, session waiting, quota admission, upstream headers across attempts and response streaming. First-byte time is measured from request arrival and can precede generated text, so it is not time to first token. The fields are elapsed time, not CPU cost.

| Store | Bound |
| --- | --- |
| Events in memory | 512 since startup |
| Event write queue | 256 events, 8 KiB per event, on a background thread |
| Event file | Rotates at 8 MiB, keeps one previous file |
| History | 31 days, trimmed hourly to 250,000 requests and 50,000 status records |
| Account status samples | Every 5 minutes |
| Session affinity | 4,096 bindings, expired after 30 idle days, checkpointed hourly |

Diagnostics never block inference on disk I/O; the view reports dropped or unwritten events. Routing changes do wait for an atomic, durable write on a blocking worker. Active bindings are never evicted, and a legacy binding gets an activity timestamp when it is first loaded for selection.

## Install

Build this fork; upstream releases do not contain native routing.

```sh
cargo build --release --locked -p cliproxy
python3 native/install.py target/release/cliproxy
```

The installer prints the pinned binary and its configuration path. Start that binary with the printed `--config` argument and `--local-model`, or use it as your login service's executable. The installer does not change shell or Zed defaults.

Sign into Claude Code normally (Remote Control uses that login), then sign the inference accounts into the proxy:

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

`refresh` asks the Rust credential owner to refresh; the management command never exchanges refresh tokens itself. `status` shows recent cache counters and session affinity without model requests. `dashboard` opens the owner's dashboard: Overview shows remaining quota, the default new-session order, history and token counts; Settings has account controls; Activity shows recent errors and switches. Session affinity and model limits can override the displayed order. Request counters for other providers need `observability.usage.usage-statistics-enabled`, which the installer turns on.

| Port | Use |
| --- | --- |
| 20148 | Management, standalone install. A combined install uses its existing dashboard port. |
| 20149 | TLS health and status |
| 20152 | Authenticated HTTP CONNECT |

The Claude launcher sets `HTTPS_PROXY` and a process-scoped `NODE_EXTRA_CA_CERTS`, and keeps `api.anthropic.com` as the endpoint. No system certificate trust changes are needed. Permission mode stays Claude's setting.

### Attach to an existing server

The native transport runs in the same process as the API server. To attach it to a server you already run, set `native-claude.config-file` in that server's YAML, and point the native JSON's `managerConfig`, `managementUrl`, `managementKeyFile` and `authDir` at that server. Keep native account IDs when you consolidate credentials so affinity stays valid. Stop both refresh owners before moving files, and disable the retired service so only one owner refreshes.

## Zed and Remote Control

Build the pinned ACP adapter from official packages plus a small Remote Control patch:

```sh
python3 native/acp/install.py
```

It pins ACP 0.89.1 and Claude Agent SDK 0.3.293 with a lockfile and verifies the upstream bytes before patching. The SDK is unchanged. The adapter calls the SDK's `enableRemoteControl` for new and resumed sessions and starts the consumer for phone messages. The qualified native CLI version is 2.1.296; upgrades need qualification.

After a small routing, cache and Remote Control check, point `~/.local/share/cliproxy-native/current` at the installed release. Put the absolute launcher path the installer printed in `~/.config/zed/settings.json`:

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

Set `remoteControlAtStartup: true` in Claude settings. In Zed, choose **Claude Agent** under External Agents; saved ACP threads resume their native conversation. The dashboard's **Use with tools** page shows the configured launcher path. For a different ACP release, also update `native-claude.acp-launcher` in the server configuration.

To accept work from your phone, run `claude rc` (or `claude remote-control`) in the project directory and keep the terminal running, then connect from the Code tab on your phone. First remove legacy gateway `ANTHROPIC_BASE_URL` and `apiKeyHelper` overrides from Claude settings. Server mode uses the native Claude login, its child sessions inherit the launcher's proxy environment, and it rejects the `--settings` override the interactive launcher uses.

Do not set `ANTHROPIC_BASE_URL` to the proxy's management address. Custom base URLs go through the translating executor, which does not preserve native Remote Control.

## State and upgrades

| Path | Contents |
| --- | --- |
| `~/.config/cliproxy-native/` | Configuration, account names and preferences |
| `~/.local/state/cliproxy-native/` | OAuth files, keys, affinity JSON and sanitized routing events |
| `~/.local/share/cliproxy-native/releases/` | Immutable releases |

OAuth files use the upstream JSON format, with directories at mode 0700 and files at 0600. Never commit this state.

Updates are manual and pinned. Stop the login service, then run:

```sh
python3 native/install.py target/release/cliproxy --upgrade
```

This keeps credentials and settings, migrates the predecessor's SQLite pins and cooldowns to atomic JSON, and prints the new executable. The binary refuses an unmigrated SQLite configuration. It takes the exclusive refresh-owner lock before automatic refresh starts; never run another refresh owner against the same auth directory.

## Qualification

The wire tests use private loopback fixtures:

```sh
cargo build -p cliproxy --bin cliproxy -p cpa-server --example native_router_fixture
cargo test -p cpa-server native_claude --lib
python3 native/test.py
```

They cover byte-preserving bodies and beta headers, credential separation, compressed replies, small and large SSE and JSON counters, cancellation, no replay after lost or partial responses, manager toggles, token rotation and affinity across restart. Live cache and phone control need a small separate qualification that uses included quota. Keep live qualification records outside the repository.
