const test = require("node:test"),
  assert = require("node:assert/strict"),
  fs = require("fs"),
  https = require("https"),
  path = require("path"),
  cp = require("child_process"),
  z = require("zlib"),
  os = require("os");
const root = __dirname,
  certDir = process.env.TRANSPARENT_TEST_CERT_DIR || root,
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "transparent-wire-"));
const ca = process.env.TRANSPARENT_TEST_CA;
const response = Buffer.from(
  'data: {"type":"message_start","message":{"usage":{"input_tokens":2,"cache_read_input_tokens":42}}}\n\ndata: {"type":"message_stop"}\n\n',
);
let child,
  backend,
  seen = [],
  cancelClosed = false;
const secret = "fixture-secret-fixture-secret-fixture-secret";
function curl(
  suffix = "",
  auth = true,
  limit = "4",
  plain = false,
  challenge = false,
) {
  return new Promise((resolve) => {
    const conf =
      'url = "https://api.anthropic.com/v1/messages' +
      suffix +
      '"\nproxy = "' +
      (plain ? "http://localhost:20153" : "https://localhost:20145") +
      '"\n' +
      (auth ? 'proxy-user = "claude:' + secret + '"\n' : "") +
      (challenge ? "proxy-anyauth\n" : "") +
      'proxy-cacert = "' +
      ca +
      '"\ncacert = "' +
      ca +
      '"\nnoproxy = ""\nmax-time = ' +
      limit +
      '\nsilent\nshow-error\nheader = "anthropic-beta: inline-tools-2026-09-15,unknown-beta"\nheader = "Authorization: Bearer original"\nheader = "content-type: application/json"\ndata-binary = "@' +
      dir +
      '/body.json"\n';
    const p = cp.spawn("curl", ["--config", "-"]);
    let out = [],
      err = [];
    p.stdout.on("data", (c) => out.push(c));
    p.stderr.on("data", (c) => err.push(c));
    p.on("exit", (code) =>
      resolve({
        code,
        body: Buffer.concat(out),
        error: Buffer.concat(err).toString(),
      }),
    );
    p.stdin.end(conf);
  });
}
test.before(async () => {
  fs.writeFileSync(dir + "/secret", secret, { mode: 0o600 });
  fs.writeFileSync(
    dir + "/body.json",
    '{ "model":"claude-haiku-5-5", "metadata":{"session_id":"wire"}, "messages":[], "max_tokens":1 }',
  );
  backend = https.createServer(
    {
      key: fs.readFileSync(certDir + "/leaf.key"),
      cert: fs.readFileSync(certDir + "/leaf.crt"),
    },
    async (req, res) => {
      let parts = [];
      for await (const c of req) parts.push(c);
      seen.push({
        path: req.url,
        body: Buffer.concat(parts),
        headers: req.headers,
      });
      if (req.url.includes("includedlimit")) {
        res.setHeader("anthropic-ratelimit-unified-5h-status", "rejected");
        res.setHeader("anthropic-ratelimit-unified-5h-reset", String(Math.floor(Date.now() / 1000) + 3600));
      }
      if (req.url.includes("notfound")) {
        res.writeHead(404, {"content-type":"application/json"});
        return res.end(JSON.stringify({error:{type:"not_found_error",message:"private fixture message"}}));
      }
      if (req.url.includes("retryable") && req.headers.authorization === "Bearer b") {
        res.writeHead(503, {"retry-after":"2"}); return res.end("unavailable");
      }
      if (req.url.includes("slowheaders")) {
        await new Promise(r => setTimeout(r, 31000));
        if (res.destroyed) return;
      }
      if(req.url.includes("jsonlarge")){
        res.writeHead(200,{"content-type":"application/json"});
        return res.end(JSON.stringify({content:[{type:"text",text:"x".repeat(1100*1024)}],usage:{input_tokens:2,cache_read_input_tokens:84,output_tokens:7}}));
      }
      if (req.url.includes("?json")) {
        res.writeHead(200, { "content-type": "application/json" });
        return res.end(
          JSON.stringify({
            usage: { input_tokens: 2, cache_read_input_tokens: 42 },
            content: [],
          }),
        );
      }
      res.writeHead(200, {
        "content-type": "text/event-stream",
        ...(req.url.includes("gzip") ? { "content-encoding": "gzip" } : {}),
      });
      if(req.url.includes("sseerror")) return res.end('event: error\ndata: {"type":"error","error":{"type":"overloaded_error","message":"private fixture message"}}\n\n');
      if(req.url.includes("nostop")) return res.end('data: {"type":"message_start","message":{"usage":{"input_tokens":2}}}\n\n');
      if(req.url.includes("large")) {
        res.write(response.subarray(0,response.indexOf('data: {"type":"message_stop"}')));
        for(let i=0;i<1100;i++) res.write('data: {"type":"content_block_delta","delta":{"text":"'+ 'x'.repeat(1024)+'"}}\n\n');
        return res.end('data: {"type":"message_delta","usage":{"output_tokens":7,"cache_read_input_tokens":84}}\n\ndata: {"type":"message_stop"}\n\n');
      }
      if (req.url.includes("lost")) {
        return req.socket.destroy();
      }
      if (req.url.includes("partial")) {
        res.write(response.subarray(0, 30));
        setTimeout(() => res.destroy(), 20);
      } else if (req.url.includes("cancel")) {
        res.write(response.subarray(0,response.indexOf('data: {"type":"message_stop"}')));res.write("data: ");
        const timer = setInterval(() => res.write("x"), 20);
        res.on("close", () => {
          clearInterval(timer);
          cancelClosed = true;
        });
      } else
        res.end(req.url.includes("gzip") ? z.gzipSync(response) : response);
    },
  );
  await new Promise((r) => backend.listen(20146, "127.0.0.1", r));
  const q = { five_hour: { utilization: 1 }, seven_day: { utilization: 1 }, extra_usage: { is_enabled: false } };
  const config = {
    port: 20145,
    httpProxyPort: 20153,
    key: certDir + "/leaf.key",
    cert: certDir + "/leaf.crt",
    database: dir + "/pins.json",
    events: dir + "/events.jsonl",
    proxySecret: dir + "/secret",
    fixtureAccounts: [
      { id: "a", token: "a", enabled: true, quota: { ...q, seven_day: { utilization: 1, resets_at: "2099-02-01T00:00:00Z" } } },
      { id: "b", token: "b", enabled: true, quota: { ...q, seven_day: { utilization: 1, resets_at: "2099-01-01T00:00:00Z" } } },
      { id: "paid", token: "paid", enabled: true, quota: { ...q, five_hour: { utilization: 100 }, seven_day: { utilization: 1, resets_at: "2098-01-01T00:00:00Z" }, extra_usage: { is_enabled: true, monthly_limit: 100, used_credits: 1 } } },
      { id: "exhausted", token: "exhausted", enabled: true, quota: { ...q, five_hour: { utilization: 100 }, seven_day: { utilization: 1, resets_at: "2098-01-01T00:00:00Z" } } },
    ],
    upstream: { hostname: "localhost", port: 20146, ca },
  };
  const wire = require("node:crypto").createHash("sha256").update("wire").digest("hex");
  fs.writeFileSync(dir + "/pins.json", JSON.stringify({pins:{[wire]:"a"},cooldowns:{}}), {mode:0o600});
  fs.writeFileSync(dir + "/config.json", JSON.stringify(config));
  child = cp.spawn(
    process.env.NATIVE_ROUTER_TEST_BINARY || process.execPath,
    process.env.NATIVE_ROUTER_TEST_BINARY ? [dir + "/config.json"] : [root + "/server.cjs", dir + "/config.json"],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  await new Promise((resolve, reject) => {
    child.stdout.once("data", resolve);
    child.once("exit", () => reject(Error("proxy failed")));
    setTimeout(() => reject(Error("startup timeout")), 3000).unref();
  });
});
test.after(async () => {
  child?.kill("SIGTERM");
  backend?.closeAllConnections();
  await new Promise((r) => backend?.close(r));
});
test("new sessions choose soonest included weekly reset; existing sessions retain their account", async () => {
  const original = fs.readFileSync(dir + "/body.json");
  try {
    await curl();
    assert.equal(seen.at(-1).headers.authorization, "Bearer a");
    const fresh = original.toString().replace('"wire"', '"new-reset-session"');
    fs.writeFileSync(dir + "/body.json", fresh);
    await curl();
    assert.equal(seen.at(-1).headers.authorization, "Bearer b");
    assert.deepEqual(seen.at(-1).body, Buffer.from(fresh));
    await curl();
    assert.equal(seen.at(-1).headers.authorization, "Bearer b");
    const sid = require("node:crypto").createHash("sha256").update("new-reset-session").digest("hex");
    assert.equal(JSON.parse(fs.readFileSync(dir + "/pins.json")).pins[sid], "b");
    assert.ok(!seen.some(r => ["Bearer paid", "Bearer exhausted"].includes(r.headers.authorization)));
  } finally { fs.writeFileSync(dir + "/body.json", original); }
});

test("native inference tolerates more than thirty seconds before response headers without replay", async () => {
  const before = seen.length;
  const r = await curl("?slowheaders", true, "40");
  assert.equal(r.code, 0, r.error);
  assert.deepEqual(r.body, response);
  assert.equal(seen.length, before + 1);
});

test("local proxy rejects requests without secret before contacting upstream", async () => {
  const n = seen.length;
  const r = await curl("", false);
  assert.notEqual(r.code, 0);
  assert.equal(seen.length, n);
});
test("gzip response and native request remain byte-identical; counters decoded separately", async () => {
  const r = await curl("?gzip");
  assert.equal(r.code, 0, r.error);
  assert.deepEqual(r.body, z.gzipSync(response));
  const last = seen.at(-1);
  assert.deepEqual(last.body, fs.readFileSync(dir + "/body.json"));
  assert.equal(
    last.headers["anthropic-beta"],
    "inline-tools-2026-09-15,unknown-beta",
  );
  assert.equal(last.headers.authorization, "Bearer a");
  assert.ok(!last.headers["proxy-authorization"]);
  await new Promise((r) => setTimeout(r, 50));
  const events = fs
    .readFileSync(dir + "/events.jsonl", "utf8")
    .trim()
    .split("\n")
    .map(JSON.parse);
  assert.equal(events.at(-1).usage.cache_read_input_tokens, 42);
});
test("partial stream failure is never replayed to another account", async () => {
  const n = seen.length;
  const r = await curl("?partial");
  assert.notEqual(r.code, 0);
  assert.equal(seen.length, n + 1);
});
test("client cancellation closes upstream and makes no replay request", async () => {
  const n = seen.length;
  const r = await curl("?cancel", true, ".15");
  assert.notEqual(r.code, 0);
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(seen.length, n + 1);
  assert.equal(cancelClosed, true);
  if(process.env.NATIVE_ROUTER_TEST_BINARY){
    const e=fs.readFileSync(dir+'/events.jsonl','utf8').trim().split('\n').map(JSON.parse).at(-1);
    assert.equal(e.complete,false);assert.equal(e.status,499);assert.equal(e.usage.cache_read_input_tokens,42);
  }
});

async function lastInference(outcome, session) {
  for (let i = 0; i < 100; i++) {
    const rows = fs.readFileSync(dir + "/events.jsonl", "utf8").trim().split("\n").map(JSON.parse);
    const e = rows.filter(e => e.type === "inference").at(-1);
    if (e?.outcome === outcome && (!session || e.session === require("node:crypto").createHash("sha256").update(session).digest("hex"))) return e;
    await new Promise(r => setTimeout(r, 10));
  }
  assert.fail("Expected inference outcome: " + outcome);
}
test("stream errors and missing terminal events count as failures without rewriting or replay", async () => {
  for (const [query, outcome] of [["sseerror", "overloaded_error"], ["nostop", "incomplete_stream"], ["notfound", "not_found"]]) {
    const n = seen.length;
    const r = await curl("?" + query);
    assert.equal(r.code, 0);
    assert.equal(seen.length, n + 1);
    assert.ok(r.body.includes(query === "nostop" ? "message_start" : "private fixture message"));
    const e = await lastInference(outcome);
    assert.notEqual(e.status, 200);
    assert.equal(e.upstreamStatus, query === "notfound" ? 404 : 200);
    assert.ok(!JSON.stringify(e).includes("private fixture message"));
    assert.ok(e.timing.totalMs >= e.timing.firstByteMs);
  }
});
test("explicit overload fails over once with model-scoped Retry-After", async () => {
  const original = fs.readFileSync(dir + "/body.json");
  try {
    fs.writeFileSync(dir + "/body.json", original.toString().replace('"wire"','"failover-fixture"').replace('claude-haiku-5-5','claude-test-overload'));
    const n = seen.length;
    const r = await curl("?retryable");
    assert.equal(r.code, 0); assert.deepEqual(r.body, response);
    assert.equal(seen.length, n + 2);
    assert.deepEqual(seen.slice(-2).map(r => r.headers.authorization), ["Bearer b","Bearer a"]);
    const e = await lastInference("success");
    assert.equal(e.recovered, true); assert.equal(e.attempts, 2);
    const pins = JSON.parse(fs.readFileSync(dir + "/pins.json"));
    assert.ok(pins.model_cooldowns.b["claude-test-overload"] > Date.now());
    assert.ok(!pins.cooldowns.b);
  } finally { fs.writeFileSync(dir + "/body.json", original); }
});

test("lost response before headers never silently repeats accepted request", async () => {
  const n = seen.length;
  const r = await curl("?lost");
  assert.equal(seen.length, n + 1);
  assert.ok(r.body.toString().includes("not replayed"));
  const e = fs.readFileSync(dir + "/events.jsonl", "utf8").trim().split("\n").map(JSON.parse).at(-1);
  assert.equal(e.type, "transport-failure");
  assert.ok(["timeout", "connect", "connection-reset", "transport"].includes(e.failureKind));
  assert.ok(Number.isFinite(e.elapsedMs));
  assert.equal(e.replayed, false);
  assert.ok(!JSON.stringify(e).includes(secret));
});

test("non-streaming native response is unchanged and reports provider counters", async () => {
  const r = await curl("?json");
  assert.equal(r.code, 0);
  assert.deepEqual(JSON.parse(r.body), {
    usage: { input_tokens: 2, cache_read_input_tokens: 42 },
    content: [],
  });
  await new Promise((r) => setTimeout(r, 30));
  const e = fs
    .readFileSync(dir + "/events.jsonl", "utf8")
    .trim()
    .split("\n")
    .map(JSON.parse)
    .at(-1);
  assert.equal(e.usage.cache_read_input_tokens, 42);
});

test("plain loopback CONNECT keeps native TLS and request bytes intact", async () => {
  const r = await curl("", true, "4", true);
  assert.equal(r.code, 0, r.error);
  assert.deepEqual(r.body, response);
  assert.deepEqual(seen.at(-1).body, fs.readFileSync(dir + "/body.json"));
});

test("CONNECT authentication negotiation supports clients that challenge first", async () => {
  const r = await curl("", true, "4", true, true);
  assert.equal(r.code, 0, r.error);
  assert.deepEqual(r.body, response);
});

test("large SSE preserves bytes and records final usage beyond one MiB",async()=>{
 const r=await curl('?large',true,'8');assert.equal(r.code,0,r.error);assert.ok(r.body.length>1048576);
 await new Promise(r=>setTimeout(r,50));const events=fs.readFileSync(dir+'/events.jsonl','utf8').trim().split('\n').map(JSON.parse);
 assert.equal(events.at(-1).usage.cache_read_input_tokens,84);assert.equal(events.at(-1).usage.output_tokens,7);
});

test("large non-streaming JSON retains provider counters at the end",async()=>{
 const r=await curl('?jsonlarge',true,'8');assert.equal(r.code,0,r.error);assert.ok(r.body.length>1048576);
 assert.equal(JSON.parse(r.body).content[0].text.length,1100*1024);
 await new Promise(r=>setTimeout(r,50));const e=fs.readFileSync(dir+'/events.jsonl','utf8').trim().split('\n').map(JSON.parse).at(-1);
 assert.equal(e.usage.cache_read_input_tokens,84);assert.equal(e.usage.output_tokens,7);
});

test("headroom shadow records its alternative before active selection changes", async () => {
  const original = fs.readFileSync(dir + "/body.json");
  const config = JSON.parse(fs.readFileSync(dir + "/config.json"));
  config.fixtureAccounts.find(a => a.id === "b").quota.five_hour.utilization = 98;
  const restart = async shadow => {
    const exiting = new Promise(resolve => child.once("exit", resolve));
    child.kill("SIGTERM"); await exiting;
    const pins = JSON.parse(fs.readFileSync(dir + "/pins.json"));
    pins.cooldowns = {}; pins.model_cooldowns = {};
    fs.writeFileSync(dir + "/pins.json", JSON.stringify(pins));
    config.selectionShadow = shadow;
    fs.writeFileSync(dir + "/config.json", JSON.stringify(config));
    child = cp.spawn(process.env.NATIVE_ROUTER_TEST_BINARY, [dir + "/config.json"], {stdio:["ignore","pipe","pipe"]});
    await new Promise((resolve, reject) => {
      child.stdout.once("data", resolve);
      child.once("exit", () => reject(Error("proxy failed")));
      setTimeout(() => reject(Error("startup timeout")), 3000).unref();
    });
  };
  try {
    for (const shadow of [true, false]) {
      await restart(shadow);
      fs.writeFileSync(dir + "/body.json", original.toString().replace('"wire"', JSON.stringify("headroom-" + shadow)));
      const r = await curl(); assert.equal(r.code, 0, r.error);
      assert.equal(seen.at(-1).headers.authorization, shadow ? "Bearer b" : "Bearer a");
      await lastInference("success", "headroom-" + shadow);
      const rows = fs.readFileSync(dir + "/events.jsonl", "utf8").trim().split("\n").map(JSON.parse);
      const selection = rows.filter(e => e.type === "selection").at(-1);
      assert.equal(selection.proposedAccount, "a");
      assert.equal(selection.shadow, shadow);
    }
    config.maxAttempts = 1;
    config.fixtureAccounts.find(a => a.id === "b").quota.five_hour.utilization = 1;
    await restart(false);
    fs.writeFileSync(dir + "/body.json", original.toString().replace('"wire"', '"attempt-budget"').replace('claude-haiku-5-5','claude-test-budget'));
    const n = seen.length;
    const r = await curl("?retryable");
    assert.equal(seen.length, n + 1);
    assert.ok(r.body.toString().includes("attempt limit"));
  } finally { fs.writeFileSync(dir + "/body.json", original); }
});


test("included-window exhaustion overrides affinity before the quota cache expires", async () => {
  const config = JSON.parse(fs.readFileSync(dir + "/config.json"));
  const exiting = new Promise(resolve => child.once("exit", resolve));
  child.kill("SIGTERM"); await exiting;
  fs.writeFileSync(dir + "/pins.json", JSON.stringify({pins:{},cooldowns:{}}));
  config.maxAttempts = 3;
  fs.writeFileSync(dir + "/config.json", JSON.stringify(config));
  child = cp.spawn(process.env.NATIVE_ROUTER_TEST_BINARY, [dir + "/config.json"], {stdio:["ignore","pipe","pipe"]});
  await new Promise((resolve, reject) => {
    child.stdout.once("data", resolve);
    child.once("exit", () => reject(Error("proxy failed")));
    setTimeout(() => reject(Error("startup timeout")), 3000).unref();
  });
  fs.writeFileSync(dir + "/body.json", '{"model":"claude-haiku-5-5","metadata":{"session_id":"included-limit"},"messages":[],"max_tokens":1}');
  const first = await curl("?includedlimit");
  assert.equal(first.code, 0, first.error);
  assert.deepEqual(first.body, response, "finish the accepted response without replay");
  assert.equal(seen.at(-1).headers.authorization, "Bearer b");
  const second = await curl();
  assert.equal(second.code, 0, second.error);
  assert.equal(seen.at(-1).headers.authorization, "Bearer a", "move the same session to included quota on another account");
  await curl("?includedlimit");
  const count = seen.length;
  const blocked = await curl();
  assert.equal(seen.length, count, "no fallback to any exhausted or paid account");
  assert.ok(blocked.body.toString().includes("No account has confirmed available quota"));
});
