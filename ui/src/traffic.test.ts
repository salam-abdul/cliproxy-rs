import test from "node:test";
import assert from "node:assert/strict";
import {
  fallbackNote,
  isFailure,
  outcomeLabel,
  summarizeHistory,
} from "./traffic.ts";

test("a served fallback is labelled with its model and is not a failure", () => {
  const served = { outcome: "fallback_served", fallbackModel: "local-test" };
  assert.equal(outcomeLabel(served), "Fell back to local-test");
  assert.equal(isFailure(served), false);
  assert.equal(fallbackNote(served), undefined);
});

test("a failed fallback is noted on the rejected request", () => {
  const rejected = {
    outcome: "quota_unavailable",
    fallbackModel: "local-test",
    fallbackFailure: "unknown_model",
  };
  assert.equal(isFailure(rejected), true);
  assert.equal(outcomeLabel(rejected), "No account has available quota");
  assert.equal(
    fallbackNote(rejected),
    "Fallback to local-test failed: no provider serves it",
  );
  assert.equal(fallbackNote({ outcome: "quota_unavailable" }), undefined);
});

test("proxy send outcomes are labelled by status", () => {
  assert.equal(outcomeLabel({ outcome: "failed", status: 500 }), "HTTP 500");
  assert.equal(outcomeLabel({ outcome: "failover", status: 503 }), "HTTP 503");
  assert.equal(
    outcomeLabel({ outcome: "stream_error", status: null }),
    "Response stream error",
  );
  assert.equal(
    outcomeLabel({ outcome: "quota_unavailable", status: null }),
    "No account has available quota",
  );
});

const tokens = (sends: number, exactSends: number) => ({
  uncachedInputTokens: 100,
  cacheReadTokens: 300,
  cacheWriteTokens: 50,
  outputTokens: 20,
  sends,
  exactSends,
});

test("token categories come from the exact send totals", () => {
  const s = summarizeHistory({
    buckets: [{ requests: 2, failures: 1 }],
    tokens: tokens(3, 3),
  });
  assert.deepEqual(
    s.tokens.map((t) => [t.label, t.tokens]),
    [
      ["Uncached input", 100],
      ["Output", 20],
      ["Cache read", 300],
      ["Cache write", 50],
    ],
  );
  assert.equal(s.requests, 2);
  assert.equal(s.failures, 1);
  assert.equal(s.partialTokens, false);
});

test("inexact sends mark the token totals as lower bounds", () => {
  const s = summarizeHistory({ buckets: [], tokens: tokens(3, 2) });
  assert.equal(s.tokens[0].tokens, 100);
  assert.equal(s.partialTokens, true);
});

test("empty history has no measured token totals", () => {
  const s = summarizeHistory({ buckets: [], models: [] });
  assert.ok(s.tokens.every((row) => row.tokens === null));
  assert.equal(s.requests, 0);
  assert.equal(s.partialTokens, false);
});
