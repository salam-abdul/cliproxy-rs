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
      { id: "paid", token: "paid", enabled: true, quota: { ...q, seven_day: { utilization: 1, resets_at: "2098-01-01T00:00:00Z" }, extra_usage: { is_enabled: true, monthly_limit: 100, used_credits: 1 } } },
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

test("lost response before headers never silently repeats accepted request", async () => {
  const n = seen.length;
  const r = await curl("?lost");
  assert.equal(seen.length, n + 1);
  assert.ok(r.body.toString().includes("not replayed"));
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
