# Embedded dashboard

Svelte and TypeScript source is in `src/`. The built `dist/` is checked in and embedded by `crates/cpa-server/build.rs`; the dashboard is not published separately.

```sh
npm ci
npm run check   # svelte-check
npm test        # unit tests of the pure modules
npm run build   # bundle, then enforce the gzip JS and CSS budgets in scripts/size.mjs
```

Commit the rebuilt `dist/` with the source change; CI rebuilds it and fails on any difference. CI also runs the browser checks, which intercept every request so no server or provider is contacted:

```sh
npx playwright-core install chromium
node scripts/dashboard-check.mjs
```

## Pages

The top navigation has four sections. Each opens its first page; a section with several pages shows them as tabs.

- **Overview:** current accounts, quota windows, the default new-session order, and 24-hour, 7-day or 30-day native Claude history. Provider quotas and Claude and Codex quota resets are checked only with Check quotas or when an account drawer opens; Check quotas reads resets at most five minutes old, and the drawer accepts a read up to an hour old. Between checks each account shows the server's passive readings and their age. The drawer lists each reset with its expiry and spends one with Use after a confirmation; a used reset clears the account's stored limit readings and cooldowns. With native routing on, Provider activity lists each provider for the selected period, opening to its accounts: sends and output tokens with their shares, failure rate without cancels and 429 answers, 429 answers, failovers, weekly peak, unused weekly quota at the last ended reset (summed per provider in accounts), tokens per percent and cache read rate, with a health strip per provider. Providers sort by unused weekly quota, those without an ended cycle last. Under it, Speed shows completed requests for the same period: one bar per provider and source (proxy or native) splitting the mean total into time in the server, earlier sends, first response and generation; a table by model with p50 and p95 first response and total time, tokens per second and decode speed, marking rows whose first response is the first translated event rather than the first byte; and the slices (model, account, request size, cache, stream, hour) whose p95 first response is slow against the rest of their provider. Speed reloads every 5 minutes and on a range change. A failed reload keeps the loaded figures; with nothing loaded it shows that the activity store is not running (409) or that speed could not be loaded.
- **Accounts:** every account by provider, enabling, disabling, downloading or deleting several at once, the paid-overage policy, sign-in and file import. Overview and Accounts open the same account drawer.
- **Activity:** recent request attempts grouped into outcomes and errors, and the server logs, filtered by text, method, status class and path.
- **Settings:** client setup, provider keys (an OpenAI-compatible group can import the model list its provider offers), client keys, models, payload rules, configuration and system facts.

Legacy `quotas` and `usage` links redirect to Overview and Activity.

## Rules

- Keep credentials out of URLs and logs. Client keys and the management key have distinct roles.
- Show an unavailable operation as unavailable rather than hiding it, and keep every control reachable by keyboard.
- Use the server's terms for accounts, usage and configuration.
