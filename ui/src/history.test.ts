import test from "node:test";
import assert from "node:assert/strict";
import {
  accountHealth,
  historyFor,
  historyPollMs,
  requestBuckets,
} from "./history.ts";

test("history polls as often as its bars are wide allow", () => {
  assert.equal(historyPollMs("24h"), 30_000);
  assert.equal(historyPollMs("7d"), 120_000);
  assert.equal(historyPollMs("30d"), 300_000);
});

test("filtered panels use history only for the selected provider and range", () => {
  const codex = { range: "24h", provider: "codex" };
  const every = { range: "24h", provider: "" };
  assert.equal(historyFor(codex, "24h", "codex"), codex);
  assert.equal(historyFor(every, "24h", ""), every);
  assert.equal(historyFor(codex, "24h", ""), undefined);
  assert.equal(historyFor(codex, "24h", "claude"), undefined);
  assert.equal(historyFor(codex, "7d", "codex"), undefined);
  assert.equal(historyFor(undefined, "24h", ""), undefined);
});

test("request buckets distinguish observed quiet intervals from gaps", () => {
  const data = {
    from: 0,
    to: 400,
    bucketMs: 100,
    oldest: 120,
    buckets: [{ at: 100, requests: 3, failures: 1 }],
    accountBuckets: [{ credential: "cred-a", at: 100, requests: 2, failures: 0 }],
    statuses: [
      { account: "a", model: "claude", at: 50, availability: "included" },
      { account: "b", model: "claude", at: 250, availability: "included" },
    ],
  };
  assert.deepEqual(
    requestBuckets(data).map((b) => [b.known, b.requests, b.failures]),
    [
      [true, 0, 0],
      [true, 3, 1],
      [true, 0, 0],
      [false, 0, 0],
    ],
  );
  assert.deepEqual(
    requestBuckets(data, { credential: "cred-a", id: "a" }).map((b) => [b.known, b.requests]),
    [
      [true, 0],
      [true, 2],
      [false, 0],
      [false, 0],
    ],
  );
  assert.ok(
    requestBuckets({
      ...data,
      buckets: [],
      accountBuckets: [],
      statuses: [],
    }).every((b) => !b.known),
  );
});

test("a card reads buckets by credential and status readings by native id", () => {
  const data = {
    from: 0,
    to: 300,
    bucketMs: 100,
    buckets: [{ at: 0, requests: 9, failures: 0 }],
    accountBuckets: [
      { credential: "cred-a", at: 0, requests: 2, failures: 1 },
      { credential: "native-a", at: 100, requests: 5, failures: 0 },
      { credential: "cred-b", at: 100, requests: 4, failures: 0 },
    ],
    statuses: [
      { account: "native-a", model: "claude", at: 250, availability: "included" },
      { account: "cred-a", model: "claude", at: 150, availability: "included" },
    ],
  };
  assert.deepEqual(
    requestBuckets(data, { credential: "cred-a", id: "native-a" }).map((b) => [
      b.known,
      b.requests,
      b.failures,
    ]),
    [
      [true, 2, 1],
      [false, 0, 0],
      [true, 0, 0],
    ],
  );
});

test("the panels count a status reading as an observation only when asked", () => {
  const data = {
    from: 0,
    to: 200,
    bucketMs: 100,
    buckets: [{ at: 0, requests: 1, failures: 0 }],
    statuses: [
      { account: "native-a", model: "claude", at: 150, availability: "included" },
    ],
  };
  assert.deepEqual(
    requestBuckets(data, undefined, false).map((b) => b.known),
    [true, false],
  );
  assert.deepEqual(
    requestBuckets(data, undefined, true).map((b) => b.known),
    [true, true],
  );
});

test("account health keeps observation gaps unknown", () => {
  const statuses = [
    { account: "a", model: "claude", at: 5_000, availability: "included" },
    { account: "a", model: "claude", at: 20_000, availability: "cooling" },
    { account: "a", model: "claude", at: 21_000, availability: "included" },
    { account: "b", model: "claude", at: 30_000, availability: "disabled" },
  ];
  assert.deepEqual(accountHealth(statuses, "a", 0, 40_000, 4), [
    "included",
    "unknown",
    "cooling",
    "unknown",
  ]);
});
