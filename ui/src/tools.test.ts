import test from "node:test";
import assert from "node:assert/strict";
import { bytes, labelText, share, toolGroups } from "./tools.ts";

const tool = (name: string, server: string | null, carried: number) => ({ name, server, carried, calls: 0, labels: [] });

test("tools group by server, servers by carried, and serverless tools under Built in", () => {
  const groups = toolGroups({
    tools: [
      tool("Read", null, 9),
      tool("mcp__docs__search", "docs", 5),
      tool("mcp__mail__send", "mail", 3),
      tool("mcp__docs__fetch", "docs", 2),
      tool("Bash", null, 1),
    ],
    servers: [
      { name: "mail", tools: 1, carried: 7, calls: 0, errors: 0 },
      { name: "docs", tools: 2, carried: 5, calls: 0, errors: 0 },
    ],
  });
  assert.deepEqual(
    groups.map((g) => [g.name, g.server?.name ?? null, g.tools.map((t) => t.name)]),
    [
      ["Built in", null, ["Read", "Bash"]],
      ["mail", "mail", ["mcp__mail__send"]],
      ["docs", "docs", ["mcp__docs__search", "mcp__docs__fetch"]],
    ],
  );
});

test("no tools give no groups, and a server group needs a tool", () => {
  assert.deepEqual(toolGroups({ tools: [], servers: [] }), []);
  assert.deepEqual(toolGroups({}), []);
  const groups = toolGroups({ tools: [tool("mcp__a__x", "a", 1)], servers: [] });
  assert.deepEqual(groups.map((g) => g.name), ["a"]);
});

test("labels read as words", () => {
  assert.equal(labelText("dead"), "Dead weight");
  assert.equal(labelText("heavy"), "Heavy");
  assert.equal(labelText("failing"), "Failing");
  assert.equal(labelText("bloating"), "Bloating");
  assert.equal(labelText("other"), "other");
});

test("bytes read in B under a kilobyte, then KB and MB to one decimal", () => {
  assert.equal(bytes(null), "—");
  assert.equal(bytes(0), "0 B");
  assert.equal(bytes(1023.4), "1023 B");
  assert.equal(bytes(1024), "1.0 KB");
  assert.equal(bytes(102_400), "100.0 KB");
  assert.equal(bytes(1_048_575), "1024.0 KB");
  assert.equal(bytes(1_048_576), "1.0 MB");
  assert.equal(bytes(5_400_000), "5.1 MB");
});

test("shares read as whole percents", () => {
  assert.equal(share(null), "—");
  assert.equal(share(0), "0%");
  assert.equal(share(0.204), "20%");
  assert.equal(share(1), "100%");
});
