import test from "node:test";
import assert from "node:assert/strict";

Object.defineProperty(globalThis, "location", {
  value: new URL("http://127.0.0.1:8318/management.html"),
});
const { api, connect, disconnect } = await import("./api.ts");
const hooks = { unauthorized() {}, missing() {}, meta() {} };

test("local dashboard requests use a dedicated header without a credential", async () => {
  const original = globalThis.fetch;
  try {
    globalThis.fetch = async (url, init) => {
      assert.equal(url, "http://127.0.0.1:8318/v8/management/config");
      const headers = new Headers(init?.headers);
      assert.equal(headers.get("x-cpa-local-dashboard"), "1");
      assert.equal(headers.get("authorization"), null);
      assert.equal(init?.redirect, "error");
      return Response.json({});
    };
    connect("", hooks, true);
    await api("/config");
    disconnect();
    await assert.rejects(api("/config"), /Sign in/);
  } finally {
    globalThis.fetch = original;
    disconnect();
  }
});

test("key login clears local access and sends only the selected management key", async () => {
  const original = globalThis.fetch;
  try {
    globalThis.fetch = async (_url, init) => {
      const headers = new Headers(init?.headers);
      assert.equal(headers.get("x-cpa-local-dashboard"), null);
      assert.equal(
        headers.get("authorization"),
        "Bearer synthetic-management-key",
      );
      return Response.json({});
    };
    connect("", hooks, true);
    connect("synthetic-management-key", hooks);
    await api("/config");
  } finally {
    globalThis.fetch = original;
    disconnect();
  }
});
