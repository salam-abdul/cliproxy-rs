import test from "node:test";
import assert from "node:assert/strict";
import { healthBlocks, providerActivity } from "./providers.ts";

const totals = (over: Record<string, number> = {}) => ({
  requests: 0,
  sends: 0,
  failed: 0,
  rateLimited: 0,
  failedOver: 0,
  uncachedInputTokens: 0,
  cacheReadTokens: 0,
  cacheWriteTokens: 0,
  outputTokens: 0,
  cancelled: 0,
  exactSends: 0,
  ...over,
});

const cycle = (credential: string, over: Record<string, unknown> = {}) => ({
  provider: "claude",
  credential,
  start: 0,
  end: 100,
  reset: null,
  first: 0,
  peak: 50,
  last: 50,
  ended: false,
  unused: null,
  partial: false,
  inRange: true,
  tokens: 0,
  tokensPerPercent: null,
  ...over,
});

test("shares, failure rate and cache read rate leave cancels and 429s out", () => {
  const a = totals({
    sends: 10,
    failed: 2,
    cancelled: 1,
    rateLimited: 1,
    outputTokens: 300,
    uncachedInputTokens: 100,
    cacheReadTokens: 600,
    cacheWriteTokens: 300,
    exactSends: 7,
  });
  const b = totals({ sends: 30, outputTokens: 900, exactSends: 30 });
  const rows = providerActivity(
    {
      providers: [
        { provider: "claude", ...a },
        { provider: "codex", ...b },
      ],
      credentials: [],
      cycles: [],
    },
    [],
  );
  const claude = rows.find((r) => r.provider === "claude")!;
  const codex = rows.find((r) => r.provider === "codex")!;
  assert.equal(claude.sendShare, 0.25);
  assert.equal(codex.sendShare, 0.75);
  assert.equal(claude.outputShare, 0.25);
  assert.equal(codex.outputShare, 0.75);
  assert.equal(claude.failureRate, 0.25);
  assert.equal(codex.failureRate, 0);
  assert.equal(claude.cacheReadRate, 0.6);
  assert.equal(codex.cacheReadRate, null);
  assert.equal(claude.inexact, 3);
  assert.equal(codex.inexact, 0);
});

test("a provider sums its accounts' unused quota in accounts", () => {
  const [claude] = providerActivity(
    {
      providers: [{ provider: "claude", ...totals({ sends: 3 }) }],
      credentials: ["a", "b", "c"].map((credential) => ({
        provider: "claude",
        credential,
        ...totals({ sends: 1 }),
      })),
      cycles: [
        cycle("a", { ended: true, last: 50, unused: 50 }),
        cycle("b", { ended: true, last: 0, unused: 100 }),
        cycle("c"),
      ],
    },
    [],
  );
  assert.equal(claude.unusedAccounts, 1.5);
  assert.equal(claude.endedAccounts, 2);
  assert.deepEqual(
    claude.accounts.map((a) => [a.credential, a.unused]),
    [
      ["b", 100],
      ["a", 50],
      ["c", null],
    ],
  );
});

test("providers sort by unused accounts with no figure last", () => {
  const rows = providerActivity(
    {
      providers: ["alpha", "beta", "gamma"].map((provider) => ({
        provider,
        ...totals({ sends: 1 }),
      })),
      credentials: [
        { provider: "alpha", credential: "a1", ...totals({ sends: 1 }) },
        { provider: "beta", credential: "b1", ...totals({ sends: 1 }) },
        { provider: "gamma", credential: "g1", ...totals({ sends: 1 }) },
      ],
      cycles: [
        cycle("b1", { provider: "beta", ended: true, last: 20, unused: 80 }),
        cycle("g1", { provider: "gamma", ended: true, last: 90, unused: 10 }),
      ],
    },
    [],
  );
  assert.deepEqual(
    rows.map((r) => [r.provider, r.unusedAccounts]),
    [
      ["beta", 0.8],
      ["gamma", 0.1],
      ["alpha", null],
    ],
  );
});

test("an unused figure from before the range is flagged", () => {
  const [claude] = providerActivity(
    {
      providers: [{ provider: "claude", ...totals({ sends: 1 }) }],
      credentials: [{ provider: "claude", credential: "a", ...totals({ sends: 1 }) }],
      cycles: [
        cycle("a", {
          start: -500,
          end: -200,
          ended: true,
          last: 70,
          unused: 30,
          inRange: false,
          tokensPerPercent: 1000,
          partial: true,
        }),
      ],
    },
    [],
  );
  const [account] = claude.accounts;
  assert.equal(account.unused, 30);
  assert.equal(account.unusedInRange, false);
  assert.equal(account.peak, null, "the weekly peak comes only from a cycle in range");
  assert.equal(account.tokensPerPercent, 1000);
  assert.equal(account.tokensPartial, true);
});

test("health blocks mark unknown, idle, rejected and each band", () => {
  const data = {
    from: 0,
    to: 600,
    bucketMs: 100,
    oldestSend: 150,
    buckets: [
      { at: 0, provider: "claude", sends: 1, failed: 0, rateLimited: 0, cancelled: 0 },
      { at: 200, provider: "claude", sends: 3, failed: 0, rateLimited: 2, cancelled: 1 },
      { at: 300, provider: "claude", sends: 12, failed: 6, rateLimited: 1, cancelled: 1 },
      { at: 300, provider: "codex", sends: 5, failed: 0, rateLimited: 0, cancelled: 0 },
      { at: 400, provider: "claude", sends: 10, failed: 3, rateLimited: 0, cancelled: 0 },
      { at: 500, provider: "claude", sends: 10, failed: 1, rateLimited: 0, cancelled: 0 },
    ],
  };
  const blocks = healthBlocks(data, "claude");
  assert.deepEqual(
    blocks.map((b) => [b.at, b.state, b.band]),
    [
      [0, "unknown", null],
      [100, "idle", null],
      [200, "rejected", null],
      [300, "rated", 0],
      [400, "rated", 2],
      [500, "rated", 3],
    ],
  );
  assert.equal(blocks[3].rate, 0.4);
  assert.equal(blocks[4].rate, 0.7);
  assert.equal(blocks[5].rate, 0.9);
  assert.ok(
    healthBlocks({ ...data, oldestSend: null }, "claude").every((b) => b.state === "unknown"),
  );
});

test("an account is named from the credentials list", () => {
  const [claude] = providerActivity(
    {
      providers: [{ provider: "claude", ...totals({ sends: 2 }) }],
      credentials: [
        { provider: "claude", credential: "x.json", ...totals({ sends: 1 }) },
        { provider: "claude", credential: "gone.json", ...totals({ sends: 1 }) },
      ],
      cycles: [],
    },
    [{ id: "x.json", name: "x.json", email: "a@example.com" }],
  );
  assert.deepEqual(
    claude.accounts.map((a) => [a.credential, a.name]),
    [
      ["x.json", "a@example.com"],
      ["gone.json", "gone.json"],
    ],
  );
});
