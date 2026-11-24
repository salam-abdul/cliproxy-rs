import test from "node:test";
import assert from "node:assert/strict";
import { duration, partBars, slowLines, speedRows } from "./speed.ts";

const names: Record<string, string> = { codex: "Codex", claude: "Claude" };
const providerName = (id: string) => names[id] ?? id;

const row = (over: Record<string, unknown> = {}) => ({
  provider: "codex",
  source: "proxy",
  model: "gpt-5",
  ttftKind: "byte",
  requests: 3,
  firstP50: 20,
  firstP95: 30,
  totalP50: 900,
  totalP95: 1200,
  tokensPerSecond: 500,
  decodeSpeed: 1000,
  ...over,
});

const slow = (over: Record<string, unknown> = {}) => ({
  slice: "model",
  value: "b",
  provider: "codex",
  source: "proxy",
  ttftKind: "byte",
  requests: 30,
  p95: 400,
  baseline: 100,
  ...over,
});

test("durations read in milliseconds under a second and in seconds above", () => {
  assert.equal(duration(null), "—");
  assert.equal(duration(0), "0 ms");
  assert.equal(duration(850), "850 ms");
  assert.equal(duration(912.5), "913 ms");
  assert.equal(duration(999.6), "1.0 s");
  assert.equal(duration(1000), "1.0 s");
  assert.equal(duration(12_345), "12.3 s");
});

test("each provider and source gets a bar of its four parts, widest total first", () => {
  const bars = partBars({
    parts: [
      { provider: "claude", source: "native", requests: 1, server: 50, earlier: 400, first: 50, generation: 550, total: 1050 },
      { provider: "codex", source: "proxy", requests: 2, server: 12.5, earlier: 150, first: 150, generation: 600, total: 912.5 },
    ],
  });
  assert.deepEqual(
    bars.map((b) => [b.provider, b.source, b.total]),
    [
      ["claude", "native", 1050],
      ["codex", "proxy", 912.5],
    ],
  );
  assert.equal(bars[0].width, 1);
  assert.equal(bars[1].width, 912.5 / 1050);
  assert.deepEqual(bars[1].segments, [
    { part: "server", ms: 12.5, share: 12.5 / 912.5 },
    { part: "earlier", ms: 150, share: 150 / 912.5 },
    { part: "first", ms: 150, share: 150 / 912.5 },
    { part: "generation", ms: 600, share: 600 / 912.5 },
  ]);
  const shares = bars[0].segments.reduce((n, s) => n + s.share, 0);
  assert.ok(Math.abs(shares - 1) < 1e-12, "a bar's shares add up to the whole");
});

test("a bar whose total is zero has no width and no shares", () => {
  const bars = partBars({
    parts: [{ provider: "codex", source: "proxy", requests: 1, server: 0, earlier: 0, first: 0, generation: 0, total: 0 }],
  });
  assert.equal(bars.length, 1);
  assert.equal(bars[0].width, 0);
  assert.deepEqual(
    bars[0].segments.map((s) => s.share),
    [0, 0, 0, 0],
  );
});

test("a slow model reads as a sentence against the rest", () => {
  assert.deepEqual(slowLines({ slow: [slow()] }, providerName), [
    "Model b on Codex (proxy): first response p95 400 ms, 4.0 times the rest (30 requests)",
  ]);
});

test("every slice kind has a readable name and value", () => {
  const lines = slowLines(
    {
      slow: [
        slow({ slice: "credential", value: "codex-a.json", p95: 1500, baseline: 1000 }),
        slow({ slice: "credential", value: "unknown.json" }),
        slow({ slice: "size", value: "50k-200k" }),
        slow({ slice: "cache", value: "miss" }),
        slow({ slice: "stream", value: false }),
        slow({ slice: "stream", value: true }),
        slow({ slice: "hour", value: 14 }),
        slow({ slice: "hour", value: 0, provider: "claude", source: "native" }),
      ],
    },
    providerName,
    [{ id: "codex-a.json", name: "codex-a.json", email: "a@example.test" }],
  );
  assert.deepEqual(lines, [
    "Account a@example.test on Codex (proxy): first response p95 1.5 s, 1.5 times the rest (30 requests)",
    "Account unknown.json on Codex (proxy): first response p95 400 ms, 4.0 times the rest (30 requests)",
    "Request size 50k-200k on Codex (proxy): first response p95 400 ms, 4.0 times the rest (30 requests)",
    "Cache miss on Codex (proxy): first response p95 400 ms, 4.0 times the rest (30 requests)",
    "Streamed no on Codex (proxy): first response p95 400 ms, 4.0 times the rest (30 requests)",
    "Streamed yes on Codex (proxy): first response p95 400 ms, 4.0 times the rest (30 requests)",
    "Hour 14:00 on Codex (proxy): first response p95 400 ms, 4.0 times the rest (30 requests)",
    "Hour 00:00 on Claude (native): first response p95 400 ms, 4.0 times the rest (30 requests)",
  ]);
});

test("a slice slow against a rest of zero gives no ratio", () => {
  assert.deepEqual(slowLines({ slow: [slow({ p95: 0, baseline: 0 })] }, providerName), [
    "Model b on Codex (proxy): first response p95 0 ms, against 0 ms for the rest (30 requests)",
  ]);
});

test("table rows keep their figures and mark first-event timing", () => {
  const rows = speedRows({
    rows: [
      row({ ttftKind: "event", firstP50: 100, firstP95: 100, tokensPerSecond: null, decodeSpeed: null }),
      row({ firstP50: 300, firstP95: 300, model: null }),
    ],
  });
  assert.deepEqual(rows, [
    {
      provider: "codex",
      source: "proxy",
      model: "gpt-5",
      ttftKind: "event",
      requests: 3,
      firstP50: 100,
      firstP95: 100,
      totalP50: 900,
      totalP95: 1200,
      tokensPerSecond: null,
      decodeSpeed: null,
      firstEvent: true,
    },
    {
      provider: "codex",
      source: "proxy",
      model: null,
      ttftKind: "byte",
      requests: 3,
      firstP50: 300,
      firstP95: 300,
      totalP50: 900,
      totalP95: 1200,
      tokensPerSecond: 500,
      decodeSpeed: 1000,
      firstEvent: false,
    },
  ]);
});

test("an empty response gives no bars, rows or slow lines", () => {
  const empty = {
    range: "24h",
    from: 0,
    to: 1,
    offset: 0,
    requests: 0,
    untimed: 0,
    rows: [],
    parts: [],
    slow: [],
    oldestSend: null,
    lostEvents: 0,
    coverageUncertain: false,
  };
  assert.deepEqual(partBars(empty), []);
  assert.deepEqual(speedRows(empty), []);
  assert.deepEqual(slowLines(empty, providerName), []);
  assert.deepEqual(partBars({}), []);
});
