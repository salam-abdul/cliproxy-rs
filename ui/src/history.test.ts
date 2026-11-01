import test from "node:test";
import assert from "node:assert/strict";
import { accountHealth, requestBuckets } from "./history.ts";

test("request buckets distinguish observed quiet intervals from gaps", () => {
  const data = {
    from: 0,
    to: 400,
    bucketMs: 100,
    oldestRequest: 120,
    buckets: [{ at: 100, requests: 3, failures: 1 }],
    accountBuckets: [{ account: "a", at: 100, requests: 2, failures: 0 }],
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
    requestBuckets(data, "a").map((b) => [b.known, b.requests]),
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
