import { strict as assert } from "node:assert";
import { test } from "node:test";
import { exclusion, switchModel, wildcard } from "./models.ts";

test("wildcard matches the server's cases", () => {
  // The cases of `wildcard_matches_go` in crates/cpa-core/src/registry/dynamic.rs.
  for (const [p, v, ok] of [
    ["a*c", "abc", true],
    ["a*c", "ab", false],
    ["*", "x", true],
    ["a**b", "ab", true],
    ["x", "x", true],
    ["*mid*", "a-mid-b", true],
    ["a*b*c", "acb", false],
  ] as const)
    assert.equal(wildcard(p, v), ok, `${p} ${v}`);
  assert.equal(wildcard("", "x"), false);
  assert.equal(wildcard("a*a", "a"), false, "the prefix and suffix cannot overlap");
});

test("an entry excludes a model trimmed and case-insensitively", () => {
  assert.deepEqual(exclusion("Gemini-2.5-Pro", [" gemini-2.5-pro "]), { excluded: true, locked: [] });
  assert.deepEqual(exclusion("gemini-2.5-pro", ["GEMINI-2.5-*", "gemini-2.5-pro"]), {
    excluded: true,
    locked: ["GEMINI-2.5-*"],
  });
  assert.deepEqual(exclusion("gemini-3-flash", ["gemini-2.5-*"]), { excluded: false, locked: [] });
});

test("switching a model edits only its exact entries", () => {
  assert.deepEqual(switchModel("Claude-X", ["a-*"], false), ["a-*", "claude-x"], "off appends the lowercased ID");
  assert.deepEqual(switchModel("a-1", ["a-*"], false), ["a-*"], "an excluded model is not listed twice");
  assert.deepEqual(switchModel("claude-x", ["a-*", " Claude-X ", "b"], true), ["a-*", "b"]);
  assert.deepEqual(switchModel("a-1", ["a-*", "a-1"], true), ["a-*"], "a wildcard stays for the caller");
});
