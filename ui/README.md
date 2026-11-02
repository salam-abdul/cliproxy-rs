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

- **Overview:** current accounts, quota windows, the default new-session order, and 24-hour, 7-day or 30-day native Claude history. Provider quotas are checked only with Check quotas; between checks each account shows the server's passive readings and their age.
- **Accounts:** every account by provider, the paid-overage policy, sign-in and file import. Overview and Accounts open the same account drawer.
- **Activity:** recent request attempts grouped into outcomes and errors, and the server logs.
- **Settings:** client setup, provider keys, client keys, models, payload rules, configuration and system facts.

Legacy `quotas` and `usage` links redirect to Overview and Activity.

## Rules

- Keep credentials out of URLs and logs. Client keys and the management key have distinct roles.
- Show an unavailable operation as unavailable rather than hiding it, and keep every control reachable by keyboard.
- Use the server's terms for accounts, usage and configuration.
