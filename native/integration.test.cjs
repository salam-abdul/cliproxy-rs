const test = require("node:test"),
  assert = require("node:assert/strict"),
  fs = require("node:fs"),
  path = require("node:path"),
  os = require("node:os"),
  http = require("node:http"),
  https = require("node:https"),
  cp = require("node:child_process");
const ctl = require("./control.cjs");
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "native-integrated-")),
  certDir = process.env.TRANSPARENT_TEST_CERT_DIR,
  ca = process.env.TRANSPARENT_TEST_CA;
let manager,
  backend,
  child,
  config,
  rows = [],
  seen = [],
  exhaustUntil = 0,
  upstreamStatus = 200;
const secret = "test-native-proxy-secret-test-native-proxy-secret";
async function start() {
  child = cp.spawn(
    process.env.NATIVE_MANAGER_TEST_BINARY || process.execPath,
    process.env.NATIVE_MANAGER_TEST_BINARY ? ["--config",dir+"/manager.yaml","--local-model"] : [path.join(__dirname, "server.cjs"), dir + "/config.json"],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  await new Promise((resolve, reject) => {
    child.stdout.once("data", resolve);
    child.once("exit", () => reject(Error("Proxy exited")));
    setTimeout(() => reject(Error("Proxy startup timed out")), 3000).unref();
  });
  for(let i=0;i<40;i++){try{await ctl.status(config);return;}catch{await new Promise(r=>setTimeout(r,100));}}
  throw Error("Manager never became healthy");
}
async function stop() {
  if (!child || child.exitCode !== null) return;
  await new Promise((resolve) => {
    child.once("exit", resolve);
    child.kill("SIGTERM");
  });
}
function send(include = false) {
  return new Promise((resolve) => {
    const p = cp.spawn("curl", ["--config", "-"]);
    let output = "";
    p.stdout.on("data", (x) => (output += x));
    p.on("exit", (code) => resolve({ code, output }));
    p.stdin.end(
      (include ? 'include\n' : '') + 'url = "https://api.anthropic.com/v1/messages?beta=true"\nproxy = "https://localhost:20150"\nproxy-user = "claude:' +
        secret +
        '"\nproxy-cacert = "' +
        ca +
        '"\ncacert = "' +
        ca +
        '"\nnoproxy = ""\nmax-time = 5\nsilent\nheader = "Authorization: Bearer NATIVE_LOGIN"\nheader = "anthropic-beta: inline-tools-2026-09-15,FUTURE-BETA"\ndata-binary = "@' +
        dir +
        '/body.json"\n',
    );
  });
}
test.before(async () => {
  fs.chmodSync(dir, 0o700);
  for (const [name, value] of [
    ["manager-key", "manager-secret"],
    ["proxy-secret", secret],
  ])
    fs.writeFileSync(dir + "/" + name, value, { mode: 0o600 });
  for (const n of ["a", "b"]) {
    const id = n + ".json";
    rows.push({ id, provider: "claude", disabled: false, label: n });
    fs.writeFileSync(
      dir + "/" + id,
      JSON.stringify({
        type: "claude",
        access_token: "fixture-" + n,
        expired: new Date(Date.now() + 86400000).toISOString(),
        native_route_id: n,
      }),
      { mode: 0o600 },
    );
  }
  manager = http.createServer(async (req, res) => {
    if (req.headers.authorization !== "Bearer manager-secret") {
      res.writeHead(401);
      return res.end("{}");
    }
    let body = "";
    for await (const c of req) body += c;
    if (req.method === "PATCH") {
      const p = JSON.parse(body);
      rows.find((r) => r.id === p.name).disabled = p.disabled;
    }
    res.setHeader("content-type", "application/json");
    res.end(
      JSON.stringify(
        req.method === "PATCH" ? { status: "ok" } : { files: rows },
      ),
    );
  });
  await new Promise((r) => manager.listen(0, "127.0.0.1", r));
  backend = https.createServer(
    {
      key: fs.readFileSync(certDir + "/leaf.key"),
      cert: fs.readFileSync(certDir + "/leaf.crt"),
    },
    async (req, res) => {
      let parts = [];
      for await (const c of req) parts.push(c);
      seen.push({ body: Buffer.concat(parts), headers: req.headers });
      if (exhaustUntil) {
        res.setHeader("anthropic-ratelimit-unified-5h-status", "rejected");
        res.setHeader("anthropic-ratelimit-unified-5h-reset", String(exhaustUntil));
      }
      if (upstreamStatus === 429) {
        res.writeHead(429, {"retry-after":"120"});
        return res.end("quota exhausted");
      }
      res.setHeader("content-type", "text/event-stream");
      res.end(
        'data: {"type":"message_start","message":{"usage":{"input_tokens":2,"cache_read_input_tokens":1234}}}\n\ndata: {"type":"message_stop"}\n\n',
      );
    },
  );
  await new Promise((r) => backend.listen(20151, "127.0.0.1", r));
  config = {
    port: 20150,
    maxAttempts: 2,
    database: dir + "/routing.json",
    events: dir + "/events.jsonl",
    proxySecret: dir + "/proxy-secret",
    key: certDir + "/leaf.key",
    cert: certDir + "/leaf.crt",
    ca,
    authDir: dir,
    managementKeyFile: dir + "/manager-key",
    settingsFile: dir + "/settings.json",
    managementUrl: "http://127.0.0.1:" + manager.address().port,
    fixtureQuota: {
      five_hour: { utilization: 1 },
      seven_day: { utilization: 1 }, extra_usage: { is_enabled: true, monthly_limit: 100, used_credits: 1 },
    },
    upstream: { hostname: "localhost", port: 20151, ca },
  };
  fs.writeFileSync(dir + "/config.json", JSON.stringify(config));
  fs.writeFileSync(
    dir + "/body.json",
    '{ "model":"claude-opus-5-5", "metadata":{"session_id":"existing-mcp"}, "messages":[{"role":"user","content":[{"type":"tool_reference","tool_name":"mcp__plugin_linear_linear__list_issues"}]}], "tools":[{"name":"mcp__plugin_linear_linear__list_issues","input_schema":{"type":"object"}}], "max_tokens":1 }',
  );
  if (process.env.NATIVE_MANAGER_TEST_BINARY) {
    const port=manager.address().port;
    await new Promise(r=>manager.close(r));
    fs.writeFileSync(dir+'/manager.yaml',`server:
  host: "127.0.0.1"
  port: ${port}
management:
  secret-key: manager-secret
oauth:
  auth-dir: ${dir}
observability:
  usage:
    usage-statistics-enabled: true
native-claude:
  config-file: ${dir}/config.json
`);
  }
  await start();
});
test.after(async () => {
  await stop();
  manager.closeAllConnections();
  await new Promise((r) => manager.close(r));
  backend.closeAllConnections();
  await new Promise((r) => backend.close(r));
  fs.rmSync(dir, { recursive: true });
});
test("native transport honors manager toggles, committed rotation and durable affinity", async () => {
  assert.equal((await send()).code, 0);
  assert.equal(seen.at(-1).headers.authorization, "Bearer fixture-a");
  await ctl.toggle(config, "a", false);
  if (process.env.NATIVE_MANAGER_TEST_BINARY) {
    const status = await require("./credentials.cjs").management(config, "/v8/management/observability/native/status?model=claude-sonnet-4-6");
    assert.equal(status.model, "claude-sonnet-4-6");
    assert.ok(status.validUntil > status.observedAt);
    assert.equal(status.accounts.find(a => a.id === "a").availability, "disabled");
    assert.equal(status.accounts.find(a => a.id === "b").availability, "included");
    assert.equal(status.enabled, true);
    assert.equal(status.accounts.find(a => a.id === "a").disabled, true);
    assert.equal(status.accounts.find(a => a.id === "a").includedQuota, null);
    assert.ok(!JSON.stringify(status).includes("fixture-"), "status never exposes tokens");
  }
  await send();
  assert.equal(seen.at(-1).headers.authorization, "Bearer fixture-b");
  await ctl.toggle(config, "a", true);
  await send();
  assert.equal(seen.at(-1).headers.authorization, "Bearer fixture-b");
  const b = JSON.parse(fs.readFileSync(dir + "/b.json"));
  b.access_token = "fixture-b-rotated";
  fs.writeFileSync(dir + "/b.json", JSON.stringify(b));
  await stop();
  await start();
  await send();
  assert.equal(seen.at(-1).headers.authorization, "Bearer fixture-b-rotated");
  for (const r of seen) {
    assert.deepEqual(r.body, fs.readFileSync(dir + "/body.json"));
    assert.equal(
      r.headers["anthropic-beta"],
      "inline-tools-2026-09-15,FUTURE-BETA",
    );
    assert.ok(!r.headers["proxy-authorization"]);
  }
  assert.equal((await ctl.status(config)).ready, true);
});
test("proxy overage toggle persists, yields to included quota, honors disabled accounts, and advertises reset", async () => {
  if (!process.env.NATIVE_MANAGER_TEST_BINARY) return;
  const management = require("./credentials.cjs").management;
  const policy = value => management(config, "/v8/management/routing/native-policy", {method:"PATCH",body:JSON.stringify({allowExtraUsage:value})});
  assert.equal((await ctl.status(config)).allowExtraUsage, false);
  await assert.rejects(() => policy("true"));
  exhaustUntil = Math.floor(Date.now()/1000) + 120;
  await send();
  assert.equal(seen.at(-1).headers.authorization, "Bearer fixture-b-rotated");
  await policy(true);
  await send();
  assert.equal(seen.at(-1).headers.authorization, "Bearer fixture-a", "included quota beats the paid session pin");
  await policy(false);
  const count = seen.length;
  const blocked = await send(true);
  assert.equal(seen.length, count);
  assert.match(blocked.output, /HTTP\/1.1 429/);
  assert.match(blocked.output, /anthropic-ratelimit-unified-representative-claim: five_hour/i);
  assert.match(blocked.output, new RegExp("anthropic-ratelimit-unified-reset: " + exhaustUntil, "i"));
  assert.match(blocked.output, /retry-after: 1[012][0-9]/i);
  await policy(true);
  await ctl.toggle(config, "a", false);
  await send();
  assert.equal(seen.at(-1).headers.authorization, "Bearer fixture-b-rotated", "disabled beats paid affinity");
  await stop(); await start();
  assert.equal((await ctl.status(config)).allowExtraUsage, true, "policy survives restart");
  await ctl.toggle(config, "a", true);
  await policy(false);
  exhaustUntil = Math.floor(Date.now()/1000) + 2;
  await send(); await send();
  const before = seen.length;
  await send();
  assert.equal(seen.length, before, "both accounts are gated until reset");
  await new Promise(r => setTimeout(r, Math.max(0, exhaustUntil*1000-Date.now())+100));
  exhaustUntil = 0;
  await send();
  assert.equal(seen.length, before+1, "fresh included quota permits the next request after reset");
});
test("upstream quota rejection retains reset hints after failover and attempt exhaustion", async () => {
  if (!process.env.NATIVE_MANAGER_TEST_BINARY) return;
  await ctl.toggle(config, "b", false);
  upstreamStatus = 429;
  exhaustUntil = Math.floor(Date.now()/1000) + 30;
  let before = seen.length;
  let result = await send(true);
  assert.equal(seen.length, before + 1, "only the enabled account is attempted");
  assert.match(result.output, /HTTP\/1.1 429/);
  assert.match(result.output, /retry-after: 1[12][0-9]/i);
  assert.ok(Number(result.output.match(/anthropic-ratelimit-unified-reset: (\d+)/i)?.[1]) >= exhaustUntil + 90, "retry hint includes the longer cooldown");
  await stop();
  const pins = JSON.parse(fs.readFileSync(config.database));
  pins.cooldowns = {}; pins.model_cooldowns = {};
  fs.writeFileSync(config.database, JSON.stringify(pins));
  await start();
  await ctl.toggle(config, "b", true);
  before = seen.length;
  result = await send(true);
  assert.equal(seen.length, before + 2, "each eligible account is tried once");
  assert.match(result.output, /HTTP\/1.1 429/);
  assert.match(result.output, /retry-after: 1[12][0-9]/i);
  assert.ok(Number(result.output.match(/anthropic-ratelimit-unified-reset: (\d+)/i)?.[1]) >= exhaustUntil + 90, "retry hint includes the longer cooldown");
});
test("health fails and inference stops when credential owner is unreachable", async () => {
  if(process.env.NATIVE_MANAGER_TEST_BINARY) { await stop(); const r=await send(); assert.notEqual(r.code,0); await assert.rejects(()=>ctl.status(config)); return; }
  manager.closeAllConnections();
  await new Promise((r) => manager.close(r));
  const count = seen.length;
  const r = await send();
  assert.ok(r.output.includes("could not complete"));
  assert.equal(seen.length, count);
  await assert.rejects(() => ctl.status(config), /503/);
});
