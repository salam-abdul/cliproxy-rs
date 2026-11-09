import { test } from "node:test";
import assert from "node:assert/strict";
import { facets, keep, parseLine } from "./logs.ts";

const access = (status: number, method: string, path: string, id = "0a1b2c3d") =>
  `[2030-01-01 10:00:00] [${id}] [info ] ${status} |          12ms |       127.0.0.1 | ${method.padEnd(7)} "${path}"`;

test("access lines yield status, method and path without the query", () => {
  const l = parseLine(access(429, "POST", "/v1/messages?beta=true"));
  assert.deepEqual([l.status, l.method, l.path, l.level, l.id], [429, "POST", "/v1/messages", "info", "0a1b2c3d"]);
  const caller = parseLine(`[2030-01-01 10:00:00] [--------] [warn ] [observability.rs:240] 404 |            0s |                 | GET     "/x"`);
  assert.deepEqual([caller.status, caller.method, caller.path], [404, "GET", "/x"]);
});

test("other lines keep their text and have no access fields", () => {
  const l = parseLine("[2030-01-01 10:00:00] [--------] [info ] server listening on 127.0.0.1:8317");
  assert.equal(l.text, "server listening on 127.0.0.1:8317");
  assert.equal(l.status, undefined);
  assert.deepEqual(parseLine("continuation without a prefix"), { raw: "continuation without a prefix", text: "continuation without a prefix" });
});

test("filters combine, and a set filter hides lines without that field", () => {
  const lines = [access(200, "POST", "/v1/messages"), access(503, "POST", "/v1/messages"), access(200, "GET", "/v1/models"), "plain"].map(parseLine);
  const shown = (method = "", status = "", path = "") => lines.filter((l) => keep(l, { method, status, path })).map((l) => l.raw);
  assert.equal(shown().length, 4);
  assert.deepEqual(shown("POST", "5xx"), [lines[1].raw]);
  assert.deepEqual(shown("", "", "/v1/models"), [lines[2].raw]);
  assert.deepEqual(shown("", "2xx"), [lines[0].raw, lines[2].raw]);
  assert.deepEqual(shown("DELETE"), []);
});

test("facets count each value, statuses in class order", () => {
  const f = facets([access(503, "POST", "/a"), access(200, "POST", "/a"), access(200, "GET", "/b"), "plain"].map(parseLine));
  assert.deepEqual(f.method, [["POST", 2], ["GET", 1]]);
  assert.deepEqual(f.status, [["2xx", 2], ["5xx", 1]]);
  assert.deepEqual(f.path, [["/a", 2], ["/b", 1]]);
});
