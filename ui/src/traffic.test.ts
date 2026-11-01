import test from "node:test";
import assert from "node:assert/strict";
import { summarizeHistory } from "./traffic.ts";

const model = (extra: Record<string, unknown> = {}) => ({
  model: "claude-sonnet-4-6",
  requests: 1,
  inputTokens: 0,
  outputTokens: 0,
  cacheReadTokens: 0,
  cacheWriteTokens: 0,
  reportedInput: 1,
  reportedOutput: 1,
  reportedCacheRead: 1,
  reportedCacheWrite: 1,
  ...extra,
});
const summarize = (models: object[], requests = 1, modelsTruncated = false) =>
  summarizeHistory({
    buckets: [{ requests, failures: 0 }],
    models,
    modelsTruncated,
  });

test("reported token categories use actual counters", () => {
  const s = summarize([
    model({
      inputTokens: 100,
      outputTokens: 20,
      cacheReadTokens: 300,
      cacheWriteTokens: 100,
    }),
  ]);
  assert.deepEqual(
    s.tokens.map((t) => [t.tokens, t.reported]),
    [
      [100, 1],
      [20, 1],
      [300, 1],
      [100, 1],
    ],
  );
  assert.equal(s.partialTokens, false);
});

test("missing counters are excluded and totals are marked incomplete", () => {
  const s = summarize(
    [
      model({
        requests: 2,
        inputTokens: 100,
        cacheReadTokens: 300,
        reportedInput: 1,
        reportedCacheRead: 1,
        reportedCacheWrite: 0,
      }),
    ],
    2,
  );
  assert.equal(s.tokens[0].tokens, 100);
  assert.equal(s.tokens[0].reported, 1);
  assert.equal(s.tokens[3].tokens, null);
  assert.equal(s.partialTokens, true);
});

test("zero reported counters remain zero", () => {
  const s = summarize([model()]);
  assert.deepEqual(
    s.tokens.map((t) => t.tokens),
    [0, 0, 0, 0],
  );
  assert.equal(s.partialTokens, false);
});

test("truncated models mark category totals incomplete", () => {
  const s = summarize(
    [model({ inputTokens: 100, cacheReadTokens: 300 })],
    1,
    true,
  );
  assert.equal(s.tokens[0].tokens, 100);
  assert.equal(s.partialTokens, true);
});

test("unknown model identifiers still contribute token counts", () => {
  const s = summarize([model({ model: "new-model", inputTokens: 50 })]);
  assert.equal(s.tokens[0].tokens, 50);
  assert.equal(s.models[0].model, "new-model");
});

test("history totals use request counts and reported counters", () => {
  const s = summarizeHistory({
    buckets: [{ requests: 2, failures: 1 }],
    models: [
      model({
        requests: 2,
        inputTokens: 100,
        outputTokens: 30,
        reportedInput: 1,
        reportedOutput: 1,
        reportedCacheRead: 1,
        reportedCacheWrite: 1,
      }),
    ],
  });
  assert.equal(s.requests, 2);
  assert.equal(s.failures, 1);
  assert.equal(s.partialTokens, true);
});

test("empty history has no measured token totals", () => {
  const s = summarizeHistory({ buckets: [], models: [] });
  assert.ok(s.tokens.every((row) => row.tokens === null));
  assert.equal(s.requests, 0);
});
