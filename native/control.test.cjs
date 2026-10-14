const test = require("node:test"),
  assert = require("node:assert/strict"),
  fs = require("node:fs"),
  os = require("node:os"),
  path = require("node:path"),
  http = require("node:http"),
  cp = require("node:child_process");
const ctl = require("./control.cjs");
let dir,
  server,
  c,
  disabled = false,
  seen = [];
test.before(async () => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "native-control-"));
  fs.chmodSync(dir, 0o700);
  fs.writeFileSync(dir + "/key", "key", { mode: 0o600 });
  fs.writeFileSync(
    dir + "/claude.json",
    JSON.stringify({
      type: "claude",
      access_token: "SECRET_ACCESS",
      refresh_token: "SECRET_REFRESH",
      expired: new Date(Date.now() + 3600000).toISOString(),
      native_route_id: "stable",
      native_route_name: "Account-A",
    }),
    { mode: 0o600 },
  );
  server = http.createServer(async (req, res) => {
    let b = "";
    for await (const x of req) b += x;
    seen.push({ url: req.url, method: req.method, body: b });
    if (req.url.endsWith("/status") && req.method === "PATCH")
      disabled = JSON.parse(b).disabled;
    res.setHeader("content-type", "application/json");
    res.end(
      JSON.stringify(
        req.url.includes("auth-url")
          ? {
              url: "https://claude.ai/oauth/authorize?state=test",
              state: "test",
            }
          : req.url.includes("oauth/status")
            ? { status: "ok" }
            : req.method === "PATCH"
              ? { status: "ok", disabled }
              : {
                  files: [{ id: "claude.json", provider: "claude", disabled }],
                },
      ),
    );
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  c = {
    authDir: dir,
    managementKeyFile: dir + "/key",
    settingsFile: dir + "/settings.json",
    managementUrl: "http://127.0.0.1:" + server.address().port,
  };
});
test.after(async () => {
  await new Promise((r) => server.close(r));
  fs.rmSync(dir, { recursive: true });
});
test("account control uses upstream manager and preference cannot rebind sessions", async () => {
  await ctl.toggle(c, "Account-A", false);
  assert.equal((await ctl.accounts(c))[0].enabled, false);
  await ctl.toggle(c, "Account-A", true);
  assert.equal((await ctl.accounts(c))[0].enabled, true);
  await ctl.setPreference(c, "Account-A");
  assert.equal(ctl.settings(c).preferredAccount, "stable");
  assert.equal(seen.filter((x) => x.method === "PATCH").length, 2);
  assert.ok(!fs.existsSync(dir + "/routing.sqlite"));
  await assert.rejects(
    () => ctl.setPreference(c, "missing"),
    /Unknown account/,
  );
});
test("rename and settings persist without changing credential contents", async () => {
  const before = fs.readFileSync(dir + "/claude.json");
  await ctl.rename(c, "Account-A", "Work");
  assert.equal((await ctl.accounts(c))[0].name, "Work");
  assert.deepEqual(fs.readFileSync(dir + "/claude.json"), before);
  assert.equal(fs.statSync(c.settingsFile).mode & 0o077, 0);
  await assert.rejects(() => ctl.rename(c, "Work", "bad\nname"), /printable/);
});
test("sign-in uses existing PKCE manager flow; no token exchange in companion", async () => {
  assert.equal((await ctl.login(c)).state, "test");
  assert.equal((await ctl.loginStatus(c, "test")).status, "ok");
  assert.ok(
    seen.some(
      (x) =>
        x.url === "/v8/management/oauth/auth-url?provider=claude&is_webui=true",
    ),
  );
  assert.ok(seen.every((x) => !x.url.includes("/token")));
});
test("CLI never prints account credentials and invalid commands fail", async () => {
  fs.writeFileSync(dir + "/config.json", JSON.stringify(c));
  const run = (args) =>
    new Promise((resolve) => {
      const p = cp.spawn(process.execPath, [
        path.join(__dirname, "cliproxy-native"),
        "--config",
        dir + "/config.json",
        ...args,
      ]);
      let output = "";
      p.stdout.on("data", (x) => (output += x));
      p.stderr.on("data", (x) => (output += x));
      p.on("exit", (code) => resolve({ code, output }));
    });
  const a = await run(["accounts"]);
  assert.equal(a.code, 0);
  assert.ok(!/SECRET_ACCESS|SECRET_REFRESH/.test(a.output));
  assert.equal((await run(["nonsense"])).code, 1);
  assert.equal((await run(["settings", "set", "compression", "on"])).code, 1);
});
