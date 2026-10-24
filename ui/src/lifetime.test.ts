import test from "node:test";
import assert from "node:assert/strict";
import { Lifetime } from "./lifetime.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

test("latest reads cannot publish success or failure after disposal", async () => {
  const lifetime = new Lifetime();
  const resource = {};
  const first = lifetime.latest(resource),
    second = lifetime.latest(resource);
  assert.equal(first(), false);
  assert.equal(second(), true);
  const done = deferred<string>();
  let value = "cleared";
  const task = done.promise.then(
    (v) => {
      if (second()) value = v;
    },
    () => {
      if (second()) value = "error";
    },
  );
  lifetime.dispose();
  done.reject(new Error("old session"));
  await task;
  assert.equal(value, "cleared");
  assert.equal(lifetime.signal.aborted, true);
});

test("pending work coalesces per lifetime and cannot suppress reconnect work", async () => {
  const old = new Lifetime(),
    fresh = new Lifetime();
  const done = deferred<void>();
  let calls = 0;
  const first = old.once("quota:a", () => {
    calls++;
    return done.promise;
  });
  assert.equal(
    old.once("quota:a", async () => {
      calls++;
    }),
    first,
  );
  await Promise.resolve();
  old.dispose();
  await fresh.once("quota:a", async () => {
    calls++;
  });
  done.resolve();
  await first;
  assert.equal(calls, 2);
  await fresh.once(
    "quota:a",
    async () => {
      calls++;
    },
    60_000,
  );
  assert.equal(calls, 2);
  await fresh.once("quota:a", async () => {
    calls++;
  });
  assert.equal(calls, 3);
  await old.once("quota:b", async () => {
    calls++;
  });
  assert.equal(calls, 3);
});

test("failed work releases its slot and automatic retries observe the interval", async () => {
  const lifetime = new Lifetime();
  let calls = 0;
  await assert.rejects(
    lifetime.once("a", async () => {
      calls++;
      throw new Error("failed");
    }),
  );
  await lifetime.once(
    "a",
    async () => {
      calls++;
    },
    60_000,
  );
  assert.equal(calls, 1);
  await lifetime.once("a", async () => {
    calls++;
  });
  assert.equal(calls, 2);
});

test("visible polling never overlaps and disposal removes timers and listeners", async (t) => {
  t.mock.timers.enable({ apis: ["setInterval"] });
  const visibility = new EventTarget();
  let visible = true,
    calls = 0;
  const lifetime = new Lifetime();
  const done = deferred<void>();
  lifetime.every(
    100,
    async () => {
      calls++;
      await done.promise;
    },
    false,
    visibility,
    () => visible,
  );
  t.mock.timers.tick(100);
  t.mock.timers.tick(100);
  assert.equal(calls, 1);
  done.resolve();
  await Promise.resolve();
  await Promise.resolve();
  visible = false;
  t.mock.timers.tick(100);
  assert.equal(calls, 1);
  visible = true;
  visibility.dispatchEvent(new Event("visibilitychange"));
  assert.equal(calls, 2);
  lifetime.dispose();
  await Promise.resolve();
  t.mock.timers.tick(100);
  visibility.dispatchEvent(new Event("visibilitychange"));
  assert.equal(calls, 2);
});
