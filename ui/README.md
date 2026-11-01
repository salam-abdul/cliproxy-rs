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
DASHBOARD_KIND=rust node scripts/dashboard-check.mjs
```

## Pages

- **Overview:** current accounts, quota windows, the default new-session order, and 24-hour, 7-day or 30-day native Claude history. Account details open from its list.
- **Activity:** recent request attempts grouped into outcomes and errors.
- **Settings:** account controls, paid-overage policy, provider keys and client setup.

Legacy `quotas` and `usage` links redirect to Overview and Activity.

## Rules

- Keep credentials out of URLs and logs. Client keys and the management key have distinct roles.
- Show an unavailable operation as unavailable rather than hiding it, and keep every control reachable by keyboard.
- Use the server's terms for accounts, usage and configuration.
