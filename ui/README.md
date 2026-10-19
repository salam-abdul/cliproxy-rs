# Embedded dashboard

Svelte and TypeScript source lives in `src/`; `dist/` is checked in and embedded by `cpa-server/build.rs`.

```sh
npm ci
npm run check
npm test
npm run build
```

`npm run build` bundles assets and enforces the gzip JavaScript/CSS budgets in `scripts/size.mjs`. Commit updated `dist/` assets with source changes. The dashboard is not published separately.

Keep credentials out of URLs and logs. Client and management keys have distinct roles. Show unavailable operations explicitly, retain keyboard access, and keep account selection, usage and configuration terminology consistent with the server.
