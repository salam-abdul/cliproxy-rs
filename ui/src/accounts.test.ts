import test from "node:test";
import assert from "node:assert/strict";
import { accountStatus, requestOutcomes } from "./accounts.ts";
const now = 2_000_000_000_000;
const a = {
  id: "file-a",
  name: "a.json",
  provider: "claude",
  status: "active",
};
const native = (state: string) => ({
  enabled: true,
  model: "sonnet",
  validUntil: now + 15000,
  accounts: [{ fileId: "file-a", id: "native-a", availability: state }],
});
test("native status is model scoped, freshness bounded, and never inferred from enabled", () => {
  for (const state of ["limited", "unknown", "cooling", "qualification"])
    assert.notEqual(
      accountStatus(a, native(state), null, "sonnet", now).lamp,
      "ok",
    );
  assert.equal(
    accountStatus(a, native("included"), null, "sonnet", now).label,
    "Included quota available",
  );
  assert.equal(
    accountStatus(a, native("extra"), null, "sonnet", now).label,
    "Paid extra usage available",
  );
  assert.equal(
    accountStatus(a, native("extra"), null, "sonnet", now).available,
    true,
  );
  assert.equal(
    accountStatus(a, native("included"), null, "opus", now).label,
    "Checking availability",
  );
  assert.equal(
    accountStatus(a, native("included"), null, "sonnet", now + 16000).label,
    "Availability unknown",
  );
  assert.equal(
    accountStatus(
      { ...a, disabled: true },
      native("included"),
      null,
      "sonnet",
      now,
    ).label,
    "Disabled",
  );
  assert.equal(
    accountStatus(a, native("included"), new Error("offline"), "sonnet", now)
      .label,
    "Availability unknown",
  );
  assert.equal(
    accountStatus(a, { enabled: false }, null, "sonnet", now).label,
    "Enabled · availability not reported",
  );
});
test("request outcomes keep recovered attempts together and pending distinct from success", () => {
  const events = [
    {
      type: "inference",
      requestId: "r",
      at: "2033-05-18T03:33:21Z",
      account: "b",
      outcome: "success",
      recovered: true,
    },
    {
      type: "failover",
      requestId: "r",
      at: "2033-05-18T03:33:20Z",
      account: "a",
      status: 429,
    },
    { type: "selection", account: "a" },
  ];
  const out = requestOutcomes(events);
  assert.equal(out.length, 1);
  assert.equal(out[0].events.length, 2);
  assert.equal(out[0].outcome, "success");
  assert.equal(requestOutcomes([events[1]])[0].outcome, "pending");
  assert.equal(
    requestOutcomes([
      { type: "inference", requestId: "x", outcome: "client_cancelled" },
    ])[0].outcome,
    "client_cancelled",
  );
});
