# Embedded dashboard

Svelte and TypeScript source lives in `src/`; `dist/` is checked in and embedded by `cpa-server/build.rs`.

```sh
npm ci
npm run check
npm test
npm run build
```

`npm run build` bundles assets and enforces the gzip JavaScript/CSS budgets in `scripts/size.mjs`. Commit updated `dist/` assets with source changes. The dashboard is not published separately.

Overview shows current accounts, quota windows, default new-session order, and 24-hour, 7-day or 30-day native Claude history. Account details open from its list. Activity groups recent request attempts into outcomes and errors. Settings contains account controls, paid-overage policy, provider keys and client setup. Legacy `quotas` and `usage` links redirect to Overview and Activity.

Keep credentials out of URLs and logs. Client and management keys have distinct roles. Show unavailable operations explicitly, retain keyboard access, and keep account selection, usage and configuration terminology consistent with the server.
