const test = require("node:test"),
  assert = require("node:assert/strict"),
  fs = require("node:fs"),
  os = require("node:os"),
  path = require("node:path"),
  http = require("node:http");
const { readManagedAccounts } = require("./credentials.cjs");
let manager,
  dir,
  config,
  rows,
  seen = [];
test.before(async () => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "native-managed-"));
  fs.chmodSync(dir, 0o700);
  fs.writeFileSync(dir + "/key", "management-fixture", { mode: 0o600 });
  config = { authDir: dir, managementKeyFile: dir + "/key" };
  rows = [
    {
      id: "claude-a.json",
      provider: "claude",
      label: "fixture",
      disabled: false,
    },
  ];
  fs.writeFileSync(
    dir + "/claude-a.json",
    JSON.stringify({
      type: "claude",
      access_token: "access-one",
      refresh_token: "never-used",
      expired: new Date(Date.now() + 3600000).toISOString(),
      organization_uuid: "org-a",
      native_route_id: "existing-stable-id",
    }),
    { mode: 0o600 },
  );
  manager = http.createServer((req, res) => {
    seen.push({ url: req.url, authorization: req.headers.authorization });
    if (req.headers.authorization !== "Bearer management-fixture") {
      res.writeHead(401);
      return res.end("{}");
    }
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify({ files: rows }));
  });
  await new Promise((r) => manager.listen(0, "127.0.0.1", r));
  config.managementUrl = "http://127.0.0.1:" + manager.address().port;
});
test.after(async () => {
  await new Promise((r) => manager.close(r));
  fs.rmSync(dir, { recursive: true });
});
test("read-only current credentials retain native affinity ID and never refresh", async () => {
  let a = (await readManagedAccounts(config))[0];
  assert.equal(a.id, "existing-stable-id");
  assert.equal(a.token, "access-one");
  let b = JSON.parse(fs.readFileSync(dir + "/claude-a.json"));
  b.access_token = "access-rotated";
  fs.writeFileSync(dir + "/claude-a.json", JSON.stringify(b));
  a = (await readManagedAccounts(config))[0];
  assert.equal(a.token, "access-rotated");
  assert.equal(a.id, "existing-stable-id");
  assert.ok(seen.every((r) => r.url === "/v8/management/credentials"));
  assert.equal(
    JSON.parse(fs.readFileSync(dir + "/claude-a.json")).refresh_token,
    "never-used",
  );
});
test("manager disable is immediate and survives stale file enabled flag", async () => {
  rows[0].disabled = true;
  assert.equal((await readManagedAccounts(config))[0].enabled, false);
  rows[0].disabled = false;
  assert.equal((await readManagedAccounts(config))[0].enabled, true);
});
test("untrusted paths and public credential permissions fail closed", async () => {
  rows[0].id = "../escape.json";
  await assert.rejects(() => readManagedAccounts(config), /path/);
  rows[0].id = "claude-a.json";
  fs.chmodSync(dir + "/claude-a.json", 0o644);
  await assert.rejects(() => readManagedAccounts(config), /private/);
  fs.chmodSync(dir + "/claude-a.json", 0o600);
});
test("expired token never qualifies and owner authentication must succeed", async () => {
  let b = JSON.parse(fs.readFileSync(dir + "/claude-a.json"));
  b.expired = "2000-01-01T00:00:00Z";
  fs.writeFileSync(dir + "/claude-a.json", JSON.stringify(b));
  assert.equal((await readManagedAccounts(config))[0].enabled, false);
  const bad = { ...config, managementKeyFile: dir + "/wrong" };
  fs.writeFileSync(bad.managementKeyFile, "wrong", { mode: 0o600 });
  await assert.rejects(() => readManagedAccounts(bad), /401/);
});
