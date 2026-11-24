# Configuration

cliproxy-rs reads one YAML file, `config.yaml`. Pass its path with `--config`; without it, the server reads `config.yaml` in the current directory. It accepts the v8 layout shown here and the older flat layout. Unsupported settings remain inert; `plugins.enabled: true` is rejected. Native CONNECT settings and client setup are in [the native guide](../native/README.md).

The server watches the file and applies most changes within a second, without a restart. `server.host`, `server.port`, `server.tls` and `server.trusted-proxies` are read at start, so restart after changing them. When the dashboard or the Management API changes a setting, only that key is rewritten; the rest of the file, comments included, stays as it was.

## A complete small example

```yaml
config-version: 8
server:
  host: "127.0.0.1"
  port: 8317
access:
  api-keys:
    - "your-client-key"
management:
  secret-key: "your-management-key"
oauth:
  auth-dir: "~/.cliproxy-rs/auth"
routing:
  strategy: round-robin
  session-affinity: true
  retry:
    request-retry: 2
observability:
  logs:
    logging-to-file: true
```

## server

| Setting | What it does |
|---|---|
| `host` | The address to listen on. `"127.0.0.1"` accepts only this computer; `""` listens on every interface. |
| `port` | The port, `8317` by default. |
| `tls.enable`, `tls.cert`, `tls.key` | Serve HTTPS on the same port with this certificate and key file. |
| `trusted-proxies` | Addresses of reverse proxies or tunnels whose `X-Forwarded-For` header is trusted. Set it when a tunnel on the same machine forwards requests, for example `[127.0.0.1, "::1"]`. See [installation](INSTALL.md). |

## access

| Setting | What it does |
|---|---|
| `api-keys` | The client keys your tools send, as `Authorization: Bearer <key>`, `x-api-key`, `x-goog-api-key` or `?key=`. With an empty list the proxy API takes requests without a key. On a loopback listener it then refuses, with a 403, a request whose `Host` is not `localhost` or a loopback IP, or whose `Origin` is anything other than `http://` or `https://` on `localhost` or a loopback IP, so a web page on another site cannot use your accounts. That also refuses desktop-app and extension origins (`app://`, `chrome-extension://`, `null`), other names for this computer such as `host.docker.internal`, and the AI Studio relay page on `/v1/ws`: add a key to use those. Only these two headers are checked, so list a tunnel or reverse proxy in front of the listener in `server.trusted-proxies`. Home mode is exempt. |
| `allow-unauthenticated` | `false` by default. `true` lets an exposed listener serve the proxy API with an empty `api-keys` list, so anyone who can reach it can use your accounts. |

The listener is exposed when `server.host` is neither a loopback IP nor `localhost` (`""` listens on every interface) or `server.trusted-proxies` is set, since a tunnel in front of a loopback listener forwards outside requests. With an exposed listener, an empty `api-keys` list and `allow-unauthenticated` not `true`, the server refuses to start. While the server runs, a reloaded file or a dashboard or Management API change without client keys or the opt-in is rejected, and the previous settings stay in effect, when either the listener bound at start or the `server.host` and `server.trusted-proxies` in the new file are exposed. A new host or trusted proxies take effect only at the next start, but the change is refused when it is made. A loopback listener without trusted proxies may run without client keys. Home mode is exempt, because Home authenticates every client before it dispatches an account. Management access and native CONNECT routing have their own authentication and are unaffected.

## management

| Setting | What it does |
|---|---|
| `secret-key` | The dashboard and Management API password. A plaintext value is replaced by its bcrypt hash on first start. The `MANAGEMENT_PASSWORD` environment variable works too. Without a key, the management API is off unless `local-dashboard` is enabled on a loopback listener. |
| `local-dashboard` | `false` by default. `true` lets the dashboard connect without a key when the server binds to a loopback IP. Requires a local socket, a loopback Host, the dashboard request header, no forwarding headers, and a matching browser origin. Client API keys remain required. |
| `allow-remote` | `false` (the default) accepts management requests from this computer only. |
| `disable-control-panel` | `true` stops serving the dashboard at `/management.html`; the Management API keeps working. |

## oauth

| Setting | What it does |
|---|---|
| `auth-dir` | The folder for account sign-in files, `~/.cli-proxy-api` by default. `~` is expanded. The server watches it, so a file added, edited or removed there takes effect without a restart. |
| `providers.codex.chatgpt-keep-alive` | `false` by default. `true` keeps up to 2 idle connections to `chatgpt.com` per proxy for 90 seconds. Changes apply from the next request. |

A failed token refresh is retried with backoff: from 1 minute doubling to 30 minutes after `invalid_grant`, and 5 minutes after any other failure.

Claude, Codex and Antigravity accounts can have their refresh token refused for good. That happens when the token endpoint answers that the refresh token expired (Anthropic's "Refresh token expired"), was reused, invalidated or revoked, and when a Codex refresh gets `invalid_grant`. A Claude or Antigravity `invalid_grant` counts once the account's access token no longer works: it has expired, its expiry cannot be read, or an upstream answered it with a 401 and the refresh that follows gets `invalid_grant`. Until then it keeps the backoff above. Other providers do not report the reason, so their failures always keep it.

From then on the server never refreshes those tokens again on its own. The account keeps serving requests while its access token is valid, and leaves rotation when that token expires or an upstream answers it with a 401. That holds for the translating proxy, native Claude routing, Codex live calls and Alpha Search. An account whose access token expiry cannot be read leaves rotation at once.

Codex refreshes a token only in the 24 hours before it expires. A Codex token revoked earlier gets no refresh after a 401: the account is cooled and tried again until that window opens.

New tokens bring the account back: a new sign-in, an upload, an edit of the file's token fields, or a successful refresh that an operator starts from the management API, which is the only refresh that sends a refused token again. For Codex that refresh sends nothing outside the 24-hour window, or within 5 minutes of a failed refresh of the same token. Other edits leave the account as it is. The refusal is kept in memory, so after a restart the server tries the refused token once more.

A Codex account sign-in serves Responses WebSocket clients (`GET /v1/responses`, for example Codex CLI with `supports_websockets = true`) over its own upstream socket, so each continuation sends only the new input instead of the whole transcript. Set `"websockets": false` in the sign-in file to use HTTP instead. Codex API keys use HTTP unless their entry sets `websockets: true`.

## api-keys

Provider API keys, as opposed to account sign-ins, go here, grouped by provider: `claude`, `codex`, `gemini`, `vertex`, `xai` and `openai-compatibility` for any service that speaks the OpenAI API. The dashboard's Provider keys page edits them. An OpenAI-compatible service, for example:

```yaml
api-keys:
  openai-compatibility:
    - name: openrouter
      base-url: "https://openrouter.ai/api/v1"
      keys:
        - api-key: "sk-or-..."
      models:
        - name: "moonshotai/kimi-k2"
          alias: "kimi-k2"
```

## routing

These settings control translating-proxy account selection. Native CONNECT has [separate routing](../native/README.md). Known exhausted accounts are excluded before affinity and ranking.

| Setting | What it does |
|---|---|
| `strategy` | `round-robin` (the default), `fill-first`, `weighted-round-robin`, or `soonest-reset` (experimental and opt-in: select the account whose weekly window resets soonest). |
| `session-affinity` | `true` keeps a conversation on its first account while eligible. Default `false`; the native installer configures its own session routing. |
| `session-affinity-ttl` | How long a conversation stays bound to its account, `"1h"` by default. |
| `retry.request-retry` | Extra rounds over the accounts after a failed attempt, `0` by default. |
| `retry.max-retry-credentials` | At most this many accounts tried per round; `0` means all of them. |
| `retry.max-retry-interval` | The longest wait, in seconds, for an account to come out of cooldown before retrying; `0` means do not wait. |
| `cooldown.disable-cooling` | `true` keeps failing accounts in rotation instead of resting them. |
| `cooldown.transient-error-cooldown-seconds` | How long to rest an account after a temporary upstream error; `0` means 60 seconds, a negative value turns it off. |
| `cooldown.save-cooldown-status` | `true` keeps cooldowns in `.cds` files in `auth-dir`, so a restart does not forget them. |
| `cooldown.max-trusted-cooldown` | Trust a provider reset for at most this duration (`"1h"` by default, at least 10 seconds), then let the next ordinary request check it. Repeated rejection doubles the rest up to the stated reset. `0` trusts the full reset; unreadable values use the default. |

A Claude stream whose first event is an upstream error fails over before the client receives any of it, as the same status returned as an HTTP error would: the status is the one Anthropic answers with for the event's `error.type`, such as 529 for `overloaded_error`, 429 for `rate_limit_error` and 500 for `api_error`, and the account cools down as it would for that status. Such an account is tried once per request, by local and Home dispatch alike. When no other account remains, the client receives the last account's stream as it arrived: HTTP 200 with the error event. A Responses client has no event to receive for the error, and a non-streaming translated client reads the stream whole, so both get the declared status as an HTTP error instead, also for an `invalid_request_error`. A non-streaming client's `authentication_error` refreshes the account's token in the background instead of resending the request. An `invalid_request_error`, an error after the stream's first event, and any error on a fast request end the request on the account that sent it. An unknown error type does too for a client that receives the stream; for the others it fails over as a 502.

An upstream error whose `code` is a safety-policy rejection (`bio_policy`, `cyber_policy`, `cyber_policy_violation`, `misalignment_policy_violation`, `content_policy_violation` or `image_content_policy_violation`) ends the request on the account that sent it, with or without an error `type`: no other account is tried and the account does not cool down. A 402 or 429 answer and a 401 authentication error keep their own handling. A Codex stream that fails with one of these codes or `invalid_prompt` before any of it reached the client answers 400, or the status the failure event states; once the client has part of the stream, the error arrives inside that HTTP 200 stream. The match reads the error's `code` field, never its message, which can repeat the prompt.

When the proxy collects a streamed upstream answer for a non-stream request, it holds at most 256 MiB of it; a larger answer ends the request with a 500, without trying another account or cooling this one. Providers that read a non-stream answer whole themselves are not under this cap: Claude, Codex, xAI, Meta and Devin, and answers translated between formats.

### Optional preset: fail over only on 429

This commented preset keeps 429 cooldown and failover for Claude and Codex OAuth accounts. It stops ordinary failover after a failed attempt for the listed non-429 statuses, without adding a cooldown. Credential-preparation failures can still cause another attempt, and so can `requests.streaming.bootstrap-retries` for an error the upstream sent before a stream's first payload. Unlisted statuses keep normal handling. The preset applies only to OAuth accounts and leaves plan limits and provider checks unchanged. A credential's own `request_scoped_errors` rules take precedence.

Uncomment the block to use it. `oauth-request-scoped-errors` is the legacy spelling of `oauth.request-scoped-errors`; use only one. `(?s).*` matches any body, including an empty one. Rules need a body matcher; status alone does not match.

```yaml
# oauth-request-scoped-errors:
#   claude: &plan-limits-only
#     - {status: 429, match-regexr: ["(?s).*"], action: continue-and-cooldown}
#     - {status: 400, match-regexr: ["(?s).*"], action: stop}
#     - {status: 401, match-regexr: ["(?s).*"], action: stop}
#     - {status: 402, match-regexr: ["(?s).*"], action: stop}
#     - {status: 403, match-regexr: ["(?s).*"], action: stop}
#     - {status: 404, match-regexr: ["(?s).*"], action: stop}
#     - {status: 408, match-regexr: ["(?s).*"], action: stop}
#     - {status: 500, match-regexr: ["(?s).*"], action: stop}
#     - {status: 502, match-regexr: ["(?s).*"], action: stop}
#     - {status: 503, match-regexr: ["(?s).*"], action: stop}
#     - {status: 504, match-regexr: ["(?s).*"], action: stop}
#   codex: *plan-limits-only
```

## requests

| Setting | What it does |
|---|---|
| `tls-handshake-timeout-seconds` | Bounds DNS, TCP, proxy CONNECT and TLS setup, in seconds, for the translating proxy's Anthropic native-profile requests and chatgpt.com Chrome-profile requests. Missing, null, zero or negative turns it off. It does not bound waits for response headers or streams, and does not apply to compact OAuth or native CONNECT. A root-level `tls-handshake-timeout-seconds` is accepted; this nested setting wins. |
| `streaming.bootstrap-retries` | Reruns a translated streaming request this many extra times, `0` by default, when the upstream answered with an error before the stream's first payload: an auth, quota, timeout or server-error status in an error event. A stream that broke off before its first payload (empty, cut off or reset) is never rerun: its account is tried once per request. Neither is a Claude stream whose first event is an error, which fails over instead (see [routing](#routing)). Home mode never reruns. |
| `proxy-url` | Outbound proxy, e.g. `socks5://127.0.0.1:1080` or `http://proxy:3128`. An account can override it with `proxy_url` in its auth file, or `"direct"` to skip proxies. |

Claude accounts ignore `HTTPS_PROXY` and `HTTP_PROXY`. Without an explicit proxy, they connect directly. Set `requests.proxy-url` or the account's `proxy_url` when you need one. Claude API keys pointed at a custom, non-Anthropic base URL use the standard transport, which can inherit environment proxies.

## observability

| Setting | What it does |
|---|---|
| `logs.logging-to-file` | `true` writes the log to `main.log` in a `logs` folder, rotated, instead of standard output. |
| `logs.logs-max-total-size-mb` | The most disk space the log files may use. |
| `logs.request-log` | `true` writes one file per request with client and upstream request/response sections. Recognised sensitive headers and URL fields are masked, but bodies can contain credentials, including Realtime client secrets, as well as private prompts and output. Keep the logs private. On Unix each file is created `0600`; the folder is not restricted, so other local users can list file names but not read the files. |
| `usage.claude-cache-telemetry` | `false` by default. With usage collection and management on, `true` adds content-free cache observations of the final request bytes to the usage queue and RESP channel. It costs parsing and hashing per request, sends no extra requests and changes no cache markers or request bytes. |
| `history.enable` | `false` by default. `true` records every request sent to a model and its response in Parquet files under `history.dir`. See [history](#history). |
| `history.dir` | The history folder, an absolute path outside any repository or synced folder; `~` is the home directory. Required: with `enable: true` and no `dir` or a relative one, history stays off and the log says so. |
| `history.max-total-size-mb` | `10240` by default. When the files exceed it, the oldest are deleted. `0` keeps everything. There is no age limit. |

### Activity store

The activity store is the SQLite file beside native routing's event log ([Native Claude](../native/README.md#diagnostics-and-history)), and runs only while native routing runs. It has no setting. It records one row per upstream send, from native routing and from the translating proxy, and Claude and Codex quota readings from response headers. It keeps no bodies, session IDs or tool names. While it runs, every translating-proxy request is tracked for usage, as when the usage queue or history is on. `GET /v8/management/observability/activity?range=24h|7d|30d` reads it per provider and per account, narrowed by the optional `provider` and `credential` parameters; it answers `409` while the store is not running.

The route returns totals per provider and per account (requests, sends, failed sends, 429 answers, failovers away, cancelled sends, sends with exact token counts, and the exact sends' uncached input, cache read, cache write and output tokens), sends, failures, 429 answers and cancellations per provider and time bucket, and weekly quota cycles per account. A failed send excludes cancelled sends and 429 answers, and a cancelled send excludes 429 answers, so the three counts never overlap.

A cycle is one account's weekly readings between two resets. A new cycle starts at a reading that comes after the cycle's latest stated reset and states a reset more than 10 minutes beyond it, or when use falls by more than 5 points. A cycle has ended when a later cycle exists or its stated reset has passed; its unused quota is 100 minus its last reading. Tokens per percent is the uncached input, cache write and output tokens of the account's exact sends during the cycle over the rise in use, with no figure for a rise under 5 points or when the cycle has no exact tokens; a cycle first seen at 5 percent or more is partial. The route returns every cycle with a reading in the range and, for an account with no ended cycle among them, its latest cycle that ended before the range, marked as outside it.

The store keeps the latest quota reading per account, window, 10 minutes and stated reset, so the last reading before a reset survives the first one after it.

### History

History keeps one row per upstream send, except in commercial mode, which turns it off: every request the translating proxy sends to a provider, including retries, failovers and each WebSocket turn, and every native Claude `POST /v1/messages`. Count-tokens and model-list calls, Realtime and live calls, and native passthrough of other paths are not recorded.

Each row has the exchange's request ID, the WebSocket turn and send number, start and end times, provider, requested, upstream and reported model, account (its email, or its credential ID) and its auth index, session, user agent, stream flag, upstream status, outcome (`complete`, `failed` or `cancelled`; native rows also use `failover`, `stream_error` and `transport_lost`, where the translating proxy records `failed`), error text, token counts including cache reads and writes, latency, time to first byte, the provider's request ID, and both bodies.

| Body | What is stored |
| --- | --- |
| Request | The body sent upstream. For Devin it is the logged form of the request, without the session token the wire body carries. |
| Buffered response | The body as received, decoded from its content encoding. |
| Streamed response, translating proxy | Each line the executor read, joined with `\n`. For Claude this is the event stream as received, without its last line ending; line endings, and any line an executor does not read, can differ from the bytes received. |
| Native response | The bytes received, decoded from their content encoding. |
| WebSocket frames | One list entry per frame, as text, in `request_frames` and `response_frames`. A turn starts at its `response.create` frame; steering frames join the turn in progress, and the row is written as soon as the turn's response ends. |
| Native failover | No response body. The router reads at most the first 64 KiB of a 400, 403, 404 or 422 response to tell a model-entitlement rejection apart, and keeps none of it. |

`request_bytes` and `response_bytes` are the full sizes as received, before the cap; for a native response that is its encoded size. Text bodies are in `request_body` and `response_body`; a body that is not UTF-8 goes in `request_body_raw` or `response_body_raw` instead. A body keeps its first 32 MiB, and `request_truncated` or `response_truncated` marks one that was cut.

Headers, URLs, tokens and keys are never stored. The bodies are full prompts and outputs, including anything pasted into a conversation. On Unix the folder is created `0700` and files `0600`.

Recording never makes a request wait. Up to 256 rows can wait for the writer and up to 512 MiB of bodies can be held in flight; past either, rows are dropped or bodies cut, and the log reports how many rows were not written. Files are `<dir>/date=YYYY-MM-DD/<HHMMSS>-<pid>-<seq>.parquet` (UTC). The file being written ends in `.partial` and becomes `.parquet` when it closes: every 10 minutes, at 512 MiB, at UTC midnight and at shutdown. A crash loses the open file, at most 10 minutes of rows; the writer deletes it once it runs again.

Query the folder with DuckDB:

```sql
SELECT date, provider, model, count(*) AS sends,
       sum(input_tokens) AS input, sum(cache_read_tokens) AS cache_read
FROM read_parquet('/srv/cliproxy/history/**/*.parquet', hive_partitioning = true)
GROUP BY ALL
ORDER BY date;
```

or Polars:

```python
import polars as pl

sends = pl.scan_parquet("/srv/cliproxy/history/**/*.parquet", hive_partitioning=True)
sends.filter(pl.col("outcome") == "complete").select("started_at", "model", "request_body").collect()
```

`cliproxy history delete` removes rows. It reads `history.dir` from `--config` (`config.yaml` by default), also while recording is off, and needs at least one selector:

| Flag | Rows deleted |
| --- | --- |
| `--from TIME` | Started at or after `TIME` (`YYYY-MM-DD` in UTC, or RFC 3339). |
| `--to TIME` | Started before `TIME`, which must be later than `--from`. |
| `--session ID` | Of this session. |
| `--account LABEL` | Of this account, ignoring case. |
| `--model NAME` | For this upstream model or the model the client asked for. |
| `--dry-run` | None; prints what would go. |

A file whose rows all match is deleted; a file with some matching rows is rewritten without them. Each delete also leaves a tombstone in `<dir>/.deletes` for a day: a running server drops matching rows that started before the delete and that it had not written yet, including sends still in flight, and rewrites a file that was open during the delete. Rows that start after the delete are kept. The command, size retention and those rewrites share a lock file, `<dir>/.lock`; while another process holds it, the command stops and asks to retry.

## worker-threads

`worker-threads: 4` at the top level sets how many threads serve requests. Without it, the server uses the smaller of the CPU count and two. Each extra thread can keep its own pool of freed memory, so more threads mean a larger resident size. The `TOKIO_WORKER_THREADS` environment variable overrides it.

The key is read once at start from the `--config` file (`./config.yaml` by default), before a remote store (`PGSTORE_*`, `OBJECTSTORE_*`, `GITSTORE_*`) or Home supplies a config. In a store's `config.yaml` or the config Home supplies it has no effect, and a value that differs from the running thread count logs a warning; set `TOKIO_WORKER_THREADS` instead.

## Environment

- `MANAGEMENT_PASSWORD`: the management key, instead of `management.secret-key`.
- `TOKIO_WORKER_THREADS`: the number of request threads, instead of `worker-threads`.
- `RUST_LOG`: overrides the log level, for example `RUST_LOG=debug`.
- A `.env` file in the working directory is loaded at start.
- `PGSTORE_*`, `OBJECTSTORE_*` and `GITSTORE_*` keep the config and the account files in PostgreSQL, an S3-compatible bucket or a git repository.
