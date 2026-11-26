// Synthetic browser checks: all requests are intercepted; no server or provider is contacted.
// CHROME_PATH=... node scripts/dashboard-check.mjs
import { chromium } from "playwright-core";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import assert from "node:assert/strict";

const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const page = await browser.newPage();
const errors = [];
page.on("pageerror", e => errors.push(e.message));
let checks = 0, historyChecks = 0, fail = false, hold, release, native = false, overage = false, policyFailure = false;
let requireKey = false, quotaStarted, expired = false, availability = "included", saveHold, saveRelease, saveStarted;
let historyHold, historyRelease, historyFailure = false, credsFailure = false, acceptDialogs = false, credsObservedAt;
let activityChecks = 0, activityConflict = false;
let speedChecks = 0, speedConflict = false, speedEmpty = false, speedUntimed = false;
let toolsChecks = 0, toolsConflict = false, toolsFailure = false;
const statusCalls = [], deleteBodies = [], apiCalls = [];
let config = {};
let resetReads = 0;
const resetClaims = [];
const resetFresh = [];
let resetRefused = false;
const creds = [{ name: "account-a.json", auth_index: "a", provider: "claude", status: "active" },
  { name: "disabled.json", auth_index: "b", provider: "claude", disabled: true }];
const access = (status, method, path, id = "0a1b2c3d") => `[2030-01-01 10:00:00] [${id}] [info ] ${status} |          12ms |       127.0.0.1 | ${method.padEnd(7)} "${path}"`;
const logLines = [access(200, "POST", "/v1/messages?beta=true"), access(503, "POST", "/v1/messages", "1a2b3c4d"), access(200, "GET", "/v1/models"),
  "[2030-01-01 10:00:01] [--------] [info ] server listening", access(200, "GET", "/v8/management/config", "--------")];
await page.route("**/*", async route => {
  const request = route.request(), path = new URL(request.url()).pathname;
  const send = body => route.fulfill({ json: body, headers: { "x-cpa-version": "cliproxy-rs-0.2.6" } });
  if (path === "/v8/management/observability/native/status") return send({ enabled: native, allowExtraUsage: overage, model: new URL(request.url()).searchParams.get("model") || "claude", validUntil: expired ? 1 : 9e15, ready: availability === "included", selectionStrategy:"soonest-reset", selectionOrder:availability === "included" ? ["a"] : [], newSessionMinRemaining:5, accounts: creds.map(a => ({id:a.auth_index,fileId:a.id || a.name,name:a.name,availability:a.disabled ? "disabled" : availability})) });
  if (path === "/v8/management/routing/native-policy") {
    assert.equal(request.method(), "PATCH");
    if (policyFailure) return route.fulfill({status:500,json:{error:"Could not save policy"}});
    overage = request.postDataJSON().allowExtraUsage;
    return send({allowExtraUsage:overage});
  }
  if (path === "/v8/management/credentials/fields") {
    const body = request.postDataJSON();
    saveStarted?.();
    if (saveHold) await saveHold;
    creds.find(a => (a.id || a.name) === body.name).note = body.note;
    return send({});
  }
  if (path === "/v8/management/credentials/models") return send({models:[]});
  if (path === "/v8/management/observability/native/events") return send({events:[{type:"inference",requestId:"r",at:new Date().toISOString(),account:"a",model:"claude-opus-4-6",outcome:"success",recovered:true,usage:{input_tokens:1000,output_tokens:100,cache_read_input_tokens:10000,cache_creation_input_tokens:200,cache_creation:{ephemeral_5m_input_tokens:200,ephemeral_1h_input_tokens:0}}},{type:"failover",requestId:"r",at:new Date().toISOString(),account:"b",status:429},{type:"inference",requestId:"cancel",at:new Date().toISOString(),account:"a",outcome:"client_cancelled"}]});
  if (path === "/v8/management/observability/native/history") {
    historyChecks++;
    if (historyHold) await historyHold;
    if (historyFailure) return route.fulfill({status:500,json:{error:"History unavailable"}});
    const range = new URL(request.url()).searchParams.get("range") || "24h";
    const now = Date.now(), duration = {"24h":86400000,"7d":604800000,"30d":2592000000}[range];
    const width = {"24h":3600000,"7d":21600000,"30d":86400000}[range];
    return send({range,from:now-duration,to:now,bucketMs:width,oldestRequest:now-86400000,
      buckets:[{at:Math.floor(now/width)*width,requests:2,failures:1}],
      accountBuckets:[{account:"a",at:Math.floor(now/width)*width,requests:2,failures:1}],
      models:[{model:"claude-opus-4-6",requests:2,inputTokens:1000,
        outputTokens:100,cacheReadTokens:10000,cacheWriteTokens:200,
        reportedInput:1,reportedOutput:1,reportedCacheRead:1,reportedCacheWrite:1,
      }],
      statuses:[{account:"a",model:"claude",at:now-600000,availability:"included",fiveHourUsed:23}],
      errors:[{outcome:"quota_unavailable",requests:1}],lostEvents:0});
  }
  if (path === "/v8/management/observability/activity") {
    activityChecks++;
    if (activityConflict) return route.fulfill({status:409,json:{error:"activity store is not running"}});
    const range = new URL(request.url()).searchParams.get("range") || "24h";
    const now = Date.now(), duration = {"24h":86400000,"7d":604800000,"30d":2592000000}[range];
    const width = {"24h":3600000,"7d":21600000,"30d":86400000}[range], last = Math.floor(now/width)*width;
    const totals = (sends, failed, rateLimited, outputTokens) => ({requests:sends,sends,failed,rateLimited,failedOver:0,
      uncachedInputTokens:100,cacheReadTokens:300,cacheWriteTokens:100,outputTokens,cancelled:0,exactSends:sends});
    const cycle = (credential, over) => ({provider:"claude",credential,start:now-duration/2,end:now-3600000,reset:null,first:0,peak:30,last:30,
      ended:false,unused:null,partial:false,inRange:true,tokens:60000,tokensPerPercent:2000,...over});
    return send({range,from:now-duration,to:now,bucketMs:width,
      providers:[{provider:"claude",...totals(8,1,2,800)},{provider:"codex",...totals(4,0,0,200)}],
      credentials:[{provider:"claude",credential:"account-a.json",...totals(5,1,0,600)},{provider:"claude",credential:"disabled.json",...totals(3,0,2,200)},
        {provider:"codex",credential:"codex-synthetic.json",...totals(4,0,0,200)}],
      buckets:[{at:last-width,provider:"claude",sends:2,failed:0,rateLimited:2,cancelled:0},{at:last,provider:"claude",sends:6,failed:1,rateLimited:0,cancelled:0},
        {at:last,provider:"codex",sends:4,failed:0,rateLimited:0,cancelled:0}],
      cycles:[cycle("account-a.json",{ended:true,last:60,peak:60,unused:40}),cycle("disabled.json",{})],
      oldestSend:now-duration/2,lostEvents:0,coverageUncertain:false});
  }
  if (path === "/v8/management/observability/speed") {
    speedChecks++;
    const params = new URL(request.url()).searchParams;
    assert.match(params.get("offset") ?? "", /^-?\d+$/, "speed sends the viewer's offset in whole minutes");
    if (speedConflict) return route.fulfill({status:409,json:{error:"activity store is not running"}});
    const range = params.get("range") || "24h";
    const now = Date.now(), duration = {"24h":86400000,"7d":604800000,"30d":2592000000}[range];
    const base = {range,from:now-duration,to:now,offset:Number(params.get("offset")),oldestSend:now-duration/2,lostEvents:0,coverageUncertain:false};
    if (speedEmpty) return send({...base,requests:0,untimed:0,rows:[],parts:[],slow:[]});
    if (speedUntimed) return send({...base,requests:3,untimed:3,rows:[],parts:[],slow:[]});
    return send({...base,requests:93,untimed:1,
      rows:[{provider:"claude",source:"native",model:"claude-opus-4-6",ttftKind:"byte",requests:2,firstP50:800,firstP95:1200,totalP50:4000,totalP95:9000,tokensPerSecond:40.5,decodeSpeed:60.2},
        {provider:"codex",source:"proxy",model:"gpt-5",ttftKind:"event",requests:90,firstP50:300,firstP95:400,totalP50:2500,totalP95:12345,tokensPerSecond:null,decodeSpeed:null}],
      parts:[{provider:"codex",source:"proxy",requests:90,server:12.5,earlier:150,first:150,generation:600,total:912.5},
        {provider:"claude",source:"native",requests:2,server:50,earlier:400,first:50,generation:550,total:1050}],
      slow:[{slice:"model",value:"b",provider:"codex",source:"proxy",ttftKind:"byte",requests:30,p95:400,baseline:100}]});
  }
  if (path === "/v8/management/observability/tools") {
    toolsChecks++;
    if (toolsConflict) return route.fulfill({status:409,json:{error:"Body history is off"}});
    if (toolsFailure) return route.fulfill({status:500,json:{error:"Tools could not be read"}});
    const range = new URL(request.url()).searchParams.get("range") || "24h";
    const measures = {used:0,weight:2048,calls:0,callsPerConversation:0,results:0,errors:0,failureRate:null,resultMean:null,resultP95:null,resultTotal:0};
    return send({range,covered:{fromMs:Date.UTC(2030,0,1),toMs:Date.UTC(2030,0,2)},conversations:10,sends:40,skippedFormat:3,skippedCut:2,unreadableFiles:1,
      tools:[{name:"Read",server:null,carried:10,carriedShare:1,...measures,used:9,calls:30,callsPerConversation:3,results:30,errors:1,failureRate:1/30,resultMean:3000,resultP95:150000,resultTotal:90000,labels:["heavy","bloating"]},
        {name:"mcp__docs__search",server:"docs",carried:4,carriedShare:0.4,...measures,labels:["dead"]}],
      servers:[{name:"docs",tools:1,carried:4,calls:0,errors:0}],
      skills:[{name:"pdf",calls:2,conversations:1,errors:0,loadedMean:4096,loadedP95:5000}]});
  }
  if (path === "/v8/management/config") return requireKey && !request.headers().authorization ? route.fulfill({status:401,json:{error:"management key required"}}) : send(config);
  if (path === "/v8/management/config/api-keys/openai-compatibility") {
    if (request.method() === "PUT") config["api-keys"]["openai-compatibility"] = request.postDataJSON();
    return send(request.method() === "PUT" ? {} : config["api-keys"]["openai-compatibility"]);
  }
  if (path === "/v8/management/routing/model-definitions/claude")
    return send({channel:"claude",models:[{id:"claude-new-1",display_name:"New"},{id:"claude-old-1",display_name:"Old"}]});
  if (path === "/v8/management/config/oauth/excluded-models/claude") {
    const oauth = config.oauth ??= {}, lists = oauth["excluded-models"] ??= {};
    if (request.method() === "PUT") { lists.claude = request.postDataJSON(); return send({}); }
    return lists.claude ? send(lists.claude) : route.fulfill({status:404,json:{error:"not found",code:"not_found"}});
  }
  if (path === "/v8/management/observability/usage/api-keys") return send({});
  if (path === "/v8/management/requests/api-call") {
    apiCalls.push(request.postDataJSON());
    return send({status_code:200, header:{}, body:JSON.stringify({object:"list", data:[{id:"m-b"},{id:"m-a"},{id:"m-a"}]})});
  }
  if (path === "/v8/management/credentials/status") {
    const body = request.postDataJSON();
    statusCalls.push(body);
    if (!body.disabled && body.name === "disabled.json") return route.fulfill({status:409,json:{error:"Locked"}});
    creds.find(a => a.name === body.name).disabled = body.disabled;
    return send({});
  }
  if (path === "/v8/management/credentials" && request.method() === "DELETE") { deleteBodies.push(request.postDataJSON()); return send({status:"ok"}); }
  if (path === "/v8/management/credentials") return credsFailure ? route.fulfill({status:500,json:{error:"Credentials unavailable"}}) : send({ files: creds, observed_at: credsObservedAt });
  if (path === "/v8/management/observability/claude/usage") {
    assert.equal(request.method(), "GET");
    assert.equal(new URL(request.url()).searchParams.get("auth_index"), "a");
    checks++;
    quotaStarted?.();
    if (hold) await hold;
    const body = { five_hour: { utilization: 23, resets_at: "2030-01-01T00:00:00Z" }, seven_day_opus: {utilization:42}, extra_usage:{is_enabled:true,used_credits:100,monthly_limit:1000} };
    return fail ? route.fulfill({status:502,json:{error:"Claude usage provider answered HTTP 503"}}) : send(body);
  }
  if (path === "/v8/management/quota/resets") {
    if (request.method() === "POST") {
      resetClaims.push(request.postDataJSON());
      // The first claim's outcome is unknown; the retry lands.
      return send(resetClaims.length === 1 ? {outcome:"unconfirmed",message:"The provider did not confirm the reset."} : {outcome:"applied",message:"Reset applied."});
    }
    assert.equal(new URL(request.url()).searchParams.get("auth_index"), "a", "disabled accounts are never asked for resets");
    resetReads++;
    resetFresh.push(new URL(request.url()).searchParams.get("fresh"));
    if (resetRefused) return route.fulfill({status:400,json:{error:"Setup tokens cannot use resets. Connect Claude through the dashboard sign-in.",code:"no_resets"}});
    return send({eligible:true,resets:resetClaims.length > 1 ? [] : [{key:"g:0",id:"g",kind:"session",expires_at:"2099-01-01T00:00:00Z",usable:true,detail:"Refills your 5-hour limit now. This cannot be undone."}]});
  }
  if (path === "/v8/management/observability/logs") return send({lines: logLines, "next-cursor": ""});
  if (path === "/v8/management/observability/logs/errors") return send({files: [{name: "error-small.log", modified: 1893456000}, {name: "error-large.log", modified: 1893456000}]});
  if (path === "/v8/management/observability/logs/errors/error-small.log") return route.fulfill({body: "upstream status 502"});
  if (path === "/v8/management/observability/logs/errors/error-large.log") return route.fulfill({body: "x".repeat(256 * 1024) + "TAIL"});
  if (path.startsWith("/v8/")) throw new Error(`Unexpected API: ${path}`);
  const asset = path === "/management.html" ? "index.html" : path.slice(1);
  assert.match(asset, /^(index\.html|favicon\.svg|(assets|fonts)\/[\w.-]+\.(js|css|woff2))$/);
  return route.fulfill({ body: readFileSync(resolve("dist", asset)), contentType: {js:"text/javascript",css:"text/css",svg:"image/svg+xml"}[asset.split(".").pop()] || "text/html" });
});
const settle = async () => { await page.evaluate(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))); };
try {
  await page.clock.install();
  await page.goto("http://dashboard.test/management.html#overview");
  await page.getByText("Quota not reported", {exact:true}).first().waitFor({timeout:5000});
  await page.clock.fastForward(120_000);
  await settle();
  assert.equal(checks, 0, "Overview never checks provider quotas on its own");
  assert.equal(resetReads, 0, "Overview never reads quota resets on its own");
  const theme = () => page.evaluate(() => [document.documentElement.dataset.theme ?? "auto", localStorage.getItem("cliproxy-theme")]);
  assert.deepEqual(await theme(), ["auto", null], "the theme follows the system until chosen");
  for (const [label, expected] of [["Theme: auto. Use light", ["light", "light"]], ["Theme: light. Use dark", ["dark", "dark"]], ["Theme: dark. Use auto", ["auto", null]]]) {
    await page.getByRole("button", {name: label, exact: true}).first().click();
    assert.deepEqual(await theme(), expected, label);
  }
  assert.equal(await page.getByRole("switch", {name:/Allow paid overage/}).count(), 0);
  assert.equal(await page.locator(".toast").isVisible(), false, "an empty toast region is hidden");
  await page.getByRole("button", { name: "Check quotas", exact: true }).click();
  await page.getByRole("meter", {name:"5-hour limit remaining"}).waitFor({timeout:5000});
  assert.equal(checks, 1, "a manual check reaches only the enabled account");
  await page.getByText(/^1 reset · soonest expires in /).waitFor();
  assert.equal(resetReads, 1, "a manual check reads resets for the enabled account only");
  assert.deepEqual(resetFresh, ["1"], "a manual check asks for a fresh reset read");
  await page.getByText("Updated just now", {exact:true}).waitFor();
  assert.deepEqual(await page.locator(".top .nav a").allInnerTexts(), ["Overview", "Accounts", "Activity", "Settings"]);
  await page.goto("http://dashboard.test/management.html#quotas/account-a.json");
  await settle();
  assert.equal(checks, 1, "legacy quota route shares Overview readings without a new check");
  await page.clock.fastForward(61_000);
  await settle();
  assert.equal(checks, 1, "a visible Overview does not recheck on a timer");
  await page.getByText("Updated 1m ago", {exact:true}).waitFor();
  fail = true;
  await page.getByRole("button", { name: "Check quotas", exact: true }).click();
  await page.getByText("Last reading · check failed", {exact:true}).waitFor();
  assert.equal(checks, 2);
  assert.ok(await page.getByText(/77%/).count(), "a failed check retains the last quota");
  fail = false;
  hold = new Promise(r => release = r);
  await page.getByRole("button", { name: "Check quotas", exact: true }).click();
  await page.getByRole("button", { name: "Checking…", exact: true }).waitFor();
  assert.equal(await page.getByRole("button", { name: "Checking…", exact: true }).isDisabled(), true);
  assert.equal(checks, 3, "manual requests coalesce while pending");
  release(); hold = null;
  await page.getByText("Last reading · check failed", {exact:true}).waitFor({ state: "hidden" });
  await page.locator(".top .nav a", {hasText:"Settings"}).click();
  await settle();
  assert.equal(await page.locator(".section-tabs a").count(), 7, "Settings holds seven configuration pages");
  assert.equal(await page.locator('.section-tabs a[aria-current="page"]').innerText(), "Use with tools", "Settings opens its first tab");
  await page.clock.fastForward(120_000);
  await settle();
  assert.equal(checks, 3, "no page checks provider quotas on its own");
  await page.setViewportSize({ width: 390, height: 844 });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
  native = true;
  await page.reload();
  await page.locator(".top .nav").waitFor({state:"attached"});
  assert.equal(await page.getByRole("switch", {name:/Allow paid overage/}).count(), 0, "policy is absent from Settings");
  await page.goto("http://dashboard.test/management.html#credentials");
  await page.getByRole("region", {name:"Claude routing"}).waitFor();
  const toggle = page.getByRole("switch", {name:/Allow paid overage/});
  assert.equal(await toggle.getAttribute("aria-checked"), "false");
  await page.getByPlaceholder("Filter by name, provider or note").fill("no-such-account");
  await page.getByText("No accounts match.",{exact:true}).waitFor();
  assert.equal(await toggle.isVisible(), true, "account filters cannot hide the overage policy");
  let asked = "";
  page.once("dialog", dialog => { asked = dialog.message(); dialog.dismiss(); });
  await toggle.click();
  await settle();
  assert.equal(asked, "Allow paid overage? Requests may be billed after included quota runs out.");
  assert.equal(overage, false, "dismissing the confirmation leaves paid overage off");
  page.once("dialog", dialog => dialog.accept());
  await toggle.click();
  await page.getByRole("switch", {name:"Allow paid overage: On"}).waitFor();
  await page.reload();
  await page.getByRole("switch", {name:"Allow paid overage: On"}).waitFor();
  policyFailure = true;
  await toggle.click();
  await page.getByText("Could not save policy", {exact:true}).waitFor();
  assert.equal(await toggle.getAttribute("aria-checked"), "true");
  policyFailure = false;
  await toggle.click();
  await page.getByRole("switch", {name:"Allow paid overage: Off"}).waitFor();
  native = false;
  await page.reload();
  creds[0].quota = {observed_at:new Date(await page.evaluate(() => Date.now()) + 1000).toISOString(), signals:{
    "Anthropic-Ratelimit-Unified-5h-Utilization":"0.36",
    "Anthropic-Ratelimit-Unified-5h-Reset":"2000000000",
    "Anthropic-Ratelimit-Unified-Overage-Status":"rejected",
  }};
  await page.goto("http://dashboard.test/management.html#overview");
  assert.equal(await page.getByRole("switch", {name:/Allow paid overage/}).count(), 0, "policy is absent from Overview");
  assert.equal(await page.getByText("Loading history…").count(), 0, "servers without native routing do not promise history");
  assert.equal(await page.getByText(/Native Claude routing · Account switches/).count(), 0);
  const meter = page.getByRole("meter", {name:"5-hour limit remaining"});
  await meter.waitFor();
  assert.equal(await meter.getAttribute("aria-valuenow"), "64", "passive signals show without a check");
  await page.getByRole("button", { name: "Check quotas", exact: true }).click();
  await page.getByRole("meter", {name:"7-day limit (Opus only) remaining"}).waitFor();
  assert.equal(await meter.getAttribute("aria-valuenow"), "64");
  assert.equal(await page.getByText("Extra usage", {exact:true}).count(), 0, "compact Overview shows two quota windows");
  assert.equal(await page.getByRole("meter", {name:"7-day limit (Opus only) remaining"}).getAttribute("aria-valuenow"), "58");
  await page.getByRole("button", {name:"account-a.json"}).first().click();
  await page.getByRole("dialog", {name:/account-a.json details/}).waitFor();
  assert.equal(await page.getByText("Extra usage", {exact:true}).count(), 1, "account details retain additional quota windows");
  await page.getByText("5-hour reset", {exact:true}).waitFor();
  assert.equal(resetFresh.at(-1), null, "opening an account reads cached resets");
  let resetQuestion = "";
  page.once("dialog", dialog => { resetQuestion = dialog.message(); dialog.accept(); });
  await page.getByRole("button", {name:"Use", exact:true}).click();
  await page.getByText("The provider did not confirm the reset.").first().waitFor();
  assert.equal(resetQuestion, "Use this 5-hour reset? Refills your 5-hour limit now. This cannot be undone.");
  page.once("dialog", dialog => dialog.accept());
  await page.getByRole("button", {name:"Use", exact:true}).click();
  await page.getByText("No resets available.", {exact:true}).waitFor();
  assert.equal(resetClaims.length, 2);
  assert.equal(resetClaims[0].auth_index, "a");
  assert.equal(resetClaims[0].reset_id, "g");
  assert.match(resetClaims[0].request_id, /^[\w-]{1,64}$/);
  assert.equal(resetClaims[1].request_id, resetClaims[0].request_id, "a retry after an unconfirmed claim reuses its request id");
  await page.getByRole("button", {name:"Close",exact:true}).click();
  resetRefused = true;
  const refusedReads = resetReads;
  await page.getByRole("button", {name:"account-a.json"}).first().click();
  await page.getByRole("dialog", {name:/account-a.json details/}).waitFor();
  await settle();
  assert.equal(resetReads, refusedReads + 1);
  await page.getByRole("heading", {name:"Quota resets", exact:true}).waitFor({state:"detached"});
  resetRefused = false;
  await page.getByRole("button", {name:"Close",exact:true}).click();

  await page.setViewportSize({width:1280,height:900});
  delete creds[0].quota;
  requireKey = true;
  native = false;
  await page.goto("http://dashboard.test/management.html#overview");
  await page.reload();
  await page.getByLabel("Management key").fill("synthetic-management");
  hold = new Promise(r => release = r);
  const before = checks;
  const pendingRequest = new Promise(resolve => quotaStarted = resolve);
  await page.getByRole("button", {name:"Connect", exact:true}).click();
  await page.getByRole("button", { name: "Check quotas", exact: true }).click();
  await pendingRequest;
  quotaStarted = undefined;
  assert.equal(checks, before + 1);
  await page.getByRole("button", {name:"Sign out", exact:true}).click();
  hold = null;
  await page.getByLabel("Management key").fill("synthetic-management");
  await page.getByRole("button", {name:"Connect", exact:true}).click();
  await page.getByRole("button", { name: "Check quotas", exact: true }).click();
  await page.getByRole("meter", {name:"5-hour limit remaining"}).waitFor({timeout:5000}).catch(async error => {console.log({checks,before,errors}, await page.locator("body").innerText()); throw error;});
  assert.equal(checks, before + 2, "reconnect starts fresh work despite an old pending quota read");
  fail = true;
  release();
  await settle();
  assert.equal(await page.getByText("Last reading · check failed", {exact:true}).count(), 0, "old failure cannot repopulate the new connection");
  fail = false;
  native = true;
  requireKey = false;
  await page.reload();
  await page.getByText("Enabled", {exact:true}).waitFor();
  for (const [state, title] of [["limited","Quota exhausted"],["unknown","Availability unknown"],["signin","Sign in again"]]) {
    availability = state;
    await page.clock.fastForward(11000);
    await page.getByText(title, {exact:true}).waitFor();
    assert.equal(await page.getByText("Enabled", {exact:true}).count(), 0);
  }
  await page.goto("http://dashboard.test/management.html#credentials/account-a.json");
  await page.getByRole("link", {name:"Reconnect account",exact:true}).waitFor();
  availability = "included";
  expired = true;
  await page.clock.fastForward(11000);
  await page.locator(".state-label").filter({hasText:"Availability unknown"}).waitFor();
  expired = false;
  await page.clock.fastForward(11000);
  await page.locator(".state-label").filter({hasText:"Enabled"}).waitFor();
  await page.getByText("Advanced account settings", {exact:true}).click();
  await page.getByLabel("Note",{exact:true}).fill("Synthetic draft");
  let dialogs = 0;
  let lastDialog = "";
  page.on("dialog", async dialog => {dialogs++; lastDialog = dialog.message(); await (acceptDialogs ? dialog.accept() : dialog.dismiss());});
  await page.keyboard.press("Escape");
  await settle();
  assert.equal(dialogs, 1, "unsaved account fields guard closing the details");
  assert.equal(await page.getByLabel("Note",{exact:true}).inputValue(), "Synthetic draft");
  await page.getByLabel("Note",{exact:true}).fill("Submitted note");
  assert.equal(await page.locator(".toast[role=status]").count(), 1, "the toast live region is mounted before any message");
  const toast = await page.locator(".toast[role=status]").elementHandle();
  saveHold = new Promise(r => saveRelease = r);
  const savePending = new Promise(r => saveStarted = r);
  await page.getByRole("button",{name:"Save",exact:true}).click();
  await savePending;
  await page.getByLabel("Note",{exact:true}).fill("New unsaved note");
  saveRelease(); saveHold = null;
  await page.getByText("Saved.",{exact:true}).waitFor();
  assert.equal(await toast.evaluate(e => e.isConnected && e.textContent), "Saved.", "the toast live region stays mounted");
  await page.keyboard.press("Escape");
  await settle();
  assert.equal(dialogs,2,"typing during save remains dirty after the submitted version is saved");
  await page.getByLabel("Note",{exact:true}).fill("Submitted note");
  await page.keyboard.press("Escape");
  await page.getByRole("dialog").waitFor({state:"hidden"});
  assert.equal(dialogs,2,"a saved draft closes without asking");
  await page.locator('.top a[href="#activity"]').click();
  await page.getByText("Recovered after account switch",{exact:false}).waitFor();
  assert.equal(await page.locator(".native-event").count(),2,"two outcomes, with recovered attempts grouped");
  await page.getByRole("button",{name:"Failed",exact:true}).click();
  assert.equal(await page.locator(".native-event").count(),0,"successful failover and client cancellation are not failures");
  creds[1].email = "b@example.test";
  creds[0].cooldowns = [{model_key:"claude",reason:"rate_limit",retry_at:new Date(await page.evaluate(() => Date.now()) + 30000).toISOString()}];
  await page.goto("http://dashboard.test/management.html#credentials/account-a.json");
  await page.getByRole("button",{name:"Disable account",exact:true}).waitFor();
  assert.equal(await page.locator(".accounts li").filter({hasText:"b@example.test"}).locator(".name small").innerText(), "disabled.json", "the subtitle names the file, not the email again");
  await page.getByText("Advanced account settings", {exact:true}).click();
  const cooldown = page.locator(".chip").filter({hasText:"rate limit"});
  const seconds = async () => Number((await cooldown.innerText()).match(/(\d+)s$/)[1]);
  const remaining = await seconds();
  await page.clock.fastForward(5000);
  await settle();
  assert.ok(remaining - await seconds() >= 4, "cooldown countdown ticks");
  // The server's clock is five minutes behind this browser's: a two-minute cooldown is still live.
  const serverNow = await page.evaluate(() => Date.now()) + 15_000 - 300_000;
  credsObservedAt = new Date(serverNow).toISOString();
  creds[0].cooldowns = [{model_key:"claude",reason:"rate_limit",retry_at:new Date(serverNow + 120_000).toISOString()}];
  await page.clock.fastForward(15_000);
  await cooldown.getByText(/· 2m$/).waitFor();
  delete creds[1].email; delete creds[0].cooldowns; credsObservedAt = undefined;
  await page.setViewportSize({width:390,height:844});
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),false);
  await page.screenshot({path:join(tmpdir(),"cliproxy-accounts-mobile.png"),fullPage:true});
  await page.setViewportSize({width:1280,height:900});
  if (process.env.DASHBOARD_CAPTURE) await page.screenshot({ path: process.env.DASHBOARD_CAPTURE, fullPage: true });
  await page.goto("http://dashboard.test/management.html#overview");
  await page.getByRole("heading", {name:"Top models",exact:true}).waitFor();
  await page.locator(".history-model").getByText("claude-opus-4-6",{exact:true}).waitFor();
  assert.ok(historyChecks > 0,"history loads with Overview");
  const activity = page.getByRole("region",{name:"Provider activity"});
  await activity.getByText("0.4 of 1 account",{exact:true}).waitFor();
  assert.deepEqual(await activity.locator("button[aria-expanded]").allInnerTexts(),["Claude","Codex"],"providers sort by unused weekly quota");
  const claudeToggle = activity.getByRole("button",{name:"Claude",exact:true});
  assert.equal(await claudeToggle.getAttribute("aria-expanded"),"false");
  assert.equal(await activity.getByText("disabled.json",{exact:true}).count(),0,"account rows start closed");
  await claudeToggle.click();
  assert.equal(await claudeToggle.getAttribute("aria-expanded"),"true");
  await activity.getByText("account-a.json",{exact:true}).waitFor();
  await activity.getByText("disabled.json",{exact:true}).waitFor();
  assert.ok(await activity.getByText(/all rate-limited or cancelled/).count() > 0,"a bucket of only 429s is not drawn as idle");
  const historyBefore = historyChecks, activityBefore = activityChecks;
  for (let i = 0; i < 6; i++) { await page.clock.fastForward(10_000); await settle(); }
  assert.equal(historyChecks - historyBefore, 2, "history reloads every 30 seconds despite 10-second status polls");
  assert.equal(activityChecks - activityBefore, 1, "provider activity reloads every 60 seconds");
  historyHold = new Promise(r => historyRelease = r);
  const rangeBefore = historyChecks, activityRange = activityChecks;
  await page.getByRole("button",{name:"7 days"}).click();
  await page.getByText("Loading history…").first().waitFor();
  await page.getByRole("button",{name:"account-a.json"}).first().click();
  const detail = page.getByRole("dialog",{name:/account-a.json details/});
  await detail.waitFor();
  assert.ok(await detail.getByText("Loading history…").count() > 0,"old range is hidden while the new range loads");
  assert.equal(await page.evaluate(() => document.activeElement?.closest('dialog') !== null),true,"modal moves focus inside");
  await page.keyboard.press("Shift+Tab");
  assert.equal(await page.evaluate(() => { const e = document.activeElement; return !e || e === document.body || e.closest('dialog') !== null; }),true,"reverse Tab never reaches the inert page");
  await page.keyboard.press("Escape");
  await detail.waitFor({state:"hidden"});
  assert.equal(await page.evaluate(() => document.activeElement?.classList.contains('account-title')),true,"closing restores account focus");
  historyRelease(); historyHold = null;
  await page.getByText("By requests · selected period",{exact:true}).waitFor();
  assert.equal(historyChecks - rangeBefore, 1, "range selection loads durable history once");
  await activity.getByText("0.4 of 1 account",{exact:true}).waitFor();
  assert.equal(activityChecks - activityRange, 1, "range selection loads provider activity once");
  historyFailure = true;
  await page.getByRole("button",{name:"30 days"}).click();
  await page.getByText("History could not be loaded. Current account readings remain available.").waitFor();
  await page.getByRole("button",{name:"account-a.json"}).first().click();
  await detail.getByText("History could not be loaded for this period.").first().waitFor();
  assert.equal(await detail.getByText("2",{exact:true}).count(),0,"failed range does not show prior totals");
  await page.keyboard.press("Escape");
  historyFailure = false;
  await page.getByRole("button",{name:"24 hours"}).click();
  await page.locator(".history-model").getByText("claude-opus-4-6",{exact:true}).waitFor();
  activityConflict = true;
  const conflictBefore = activityChecks;
  await page.getByRole("button",{name:"7 days"}).click();
  await activity.getByText("The activity store is not running.",{exact:true}).waitFor();
  await page.locator(".history-model").getByText("claude-opus-4-6",{exact:true}).waitFor();
  assert.equal(activityChecks - conflictBefore, 1, "a refused activity read is not retried before the next poll");
  activityConflict = false;
  await page.getByRole("button",{name:"24 hours"}).click();
  await activity.getByText("0.4 of 1 account",{exact:true}).waitFor();
  activityConflict = true;
  const pollBefore = activityChecks;
  await page.clock.fastForward(60_000);
  await settle();
  assert.equal(activityChecks - pollBefore, 1, "provider activity polled while the store refuses");
  assert.equal(await activity.locator(".alert").count(), 0, "a failed poll does not replace a loaded table");
  assert.equal(await activity.getByText("0.4 of 1 account",{exact:true}).count(), 1, "the loaded table stays after a failed poll");
  activityConflict = false;
  const speed = page.getByRole("region",{name:"Speed",exact:true});
  const slowLine = "Model b on Codex (proxy): first response p95 400 ms, 4.0 times the rest (30 requests)";
  const speedRead = () => page.waitForResponse(r => new URL(r.url()).pathname === "/v8/management/observability/speed");
  await speed.getByText(slowLine,{exact:true}).waitFor();
  assert.equal(await speed.locator(".speed-bar").count(),2,"a bar per provider and source");
  assert.deepEqual(await speed.locator(".speed-bar .sr").allTextContents(),
    ["In the server 50 ms","Earlier sends 400 ms","First response 50 ms","Generation 550 ms",
      "In the server 13 ms","Earlier sends 150 ms","First response 150 ms","Generation 600 ms"],"each part of each bar has accessible text, longest bar first");
  assert.equal(await speed.getByRole("row",{name:/gpt-5/}).getByText("first event",{exact:true}).count(),1,"a row timed by the first translated event is marked");
  assert.equal(await speed.getByRole("row",{name:/claude-opus-4-6/}).getByText("first event",{exact:true}).count(),0,"a row timed by the first byte is not marked");
  const speedRange = speedChecks;
  let reread = speedRead();
  await page.getByRole("button",{name:"7 days"}).click();
  await reread;
  await speed.getByText(slowLine,{exact:true}).waitFor();
  await settle();
  assert.equal(speedChecks - speedRange,1,"range selection loads speed once");
  speedConflict = true;
  const speedPoll = speedChecks;
  await page.clock.fastForward(299_000);
  await settle();
  assert.equal(speedChecks - speedPoll,0,"speed does not poll before 300 seconds");
  reread = speedRead();
  await page.clock.fastForward(1_000);
  await reread;
  await settle();
  assert.equal(speedChecks - speedPoll,1,"speed polls every 300 seconds");
  assert.equal(await speed.locator(".alert").count(),0,"a refused speed poll shows no alert");
  assert.equal(await speed.getByText(slowLine,{exact:true}).count(),1,"the loaded speed section stays after a refused poll");
  assert.equal(await speed.locator("table").count(),1,"the speed table stays after a refused poll");
  await page.getByRole("button",{name:"24 hours"}).click();
  await speed.getByText("The activity store is not running.",{exact:true}).waitFor();
  speedConflict = false;
  speedEmpty = true;
  await page.getByRole("button",{name:"7 days"}).click();
  await speed.getByText("No completed requests recorded in this period.",{exact:true}).waitFor();
  assert.equal(await speed.locator(".speed-bar").count(),0,"an empty period draws no bars");
  speedEmpty = false;
  speedUntimed = true;
  await page.getByRole("button",{name:"30 days"}).click();
  await speed.getByText("None of the 3 completed requests in this period has timing.",{exact:true}).waitFor();
  assert.equal(await speed.getByText("No completed requests recorded in this period.",{exact:true}).count(),0,"untimed requests are not called no requests");
  assert.equal(await speed.locator(".speed-bar").count(),0,"a period with only untimed requests draws no bars");
  speedUntimed = false;
  await page.getByRole("button",{name:"24 hours"}).click();
  await speed.getByText(slowLine,{exact:true}).waitFor();
  await page.setViewportSize({width:390,height:844});
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),false,"the provider table does not widen the page");
  assert.ok(await activity.locator(".provider-scroll").evaluate(e => e.scrollWidth > e.clientWidth),"the provider table scrolls inside its card");
  assert.ok(await speed.locator(".speed-scroll").evaluate(e => e.scrollWidth > e.clientWidth),"the speed table scrolls inside its card");
  await page.setViewportSize({width:1280,height:900});
  await page.getByRole("button",{name:"account-a.json"}).first().click();
  await page.getByRole("dialog",{name:/account-a.json details/}).waitFor();
  await page.getByText("Availability history",{exact:true}).waitFor();
  await page.getByRole("button",{name:"Close",exact:true}).click();
  assert.equal(await page.getByText(/Claude routing:/).count(),0);
  await page.locator(".rank-badge").first().waitFor();
  assert.equal(await page.locator(".rank-badge").first().innerText(),"1");
  assert.equal(await page.getByLabel("Capacity model").count(),0);
  assert.equal(await page.getByText("Check Claude capacity",{exact:true}).count(),0);
  assert.equal(await page.getByText("Errors & account switches",{exact:true}).count(),0,"request details belong to Activity");
  assert.equal(await page.getByRole("heading",{name:"Tokens by category"}).count(),1);
  assert.equal(await page.getByText(/API equivalent/).count(),0);
  assert.ok(await page.getByRole("button",{name:"Check quotas",exact:true}).evaluate(el => el.getBoundingClientRect().height) <= 32,"desktop controls stay compact");
  assert.equal(await page.locator(".account-columns span").count(),4,"quota and recent request columns stay together");
  const centers = await page.locator(".account-row").first().evaluate(row => [".account-title",".quota-unreported",".account-history-wrap > :first-child"]
    .map(s => { const r = row.querySelector(s).getBoundingClientRect(); return r.top + r.height / 2; }));
  assert.ok(Math.max(...centers) - Math.min(...centers) <= 1,`account columns start on the title line: ${centers}`);
  assert.equal(await page.getByText(/cost/i).count(),0,"Overview does not imply token pricing");
  credsFailure = true;
  await page.reload();
  await page.getByText("Could not load accounts",{exact:true}).waitFor();
  assert.equal(await page.getByText("No accounts connected.").count(),0,"a failed first read is not an empty account list");
  assert.equal(await page.getByText("Refresh failed. Showing the last readings.",{exact:true}).count(),0,"a failed account read reports one error");
  credsFailure = false;
  await page.goto("http://dashboard.test/management.html#credentials");
  await page.getByRole("button",{name:"Select shown",exact:true}).click();
  await page.getByText("2 selected",{exact:true}).waitFor();
  await page.getByRole("button",{name:"Disable",exact:true}).click();
  await page.getByText("2 accounts disabled.",{exact:true}).waitFor();
  assert.deepEqual(statusCalls.splice(0), [{name:"account-a.json",auth_index:"a",disabled:true},{name:"disabled.json",auth_index:"b",disabled:true}]);
  await page.getByRole("button",{name:"Enable",exact:true}).click();
  await page.getByText("1 failed: disabled.json (Locked)",{exact:true}).waitFor();
  assert.equal(statusCalls.splice(0).length, 2, "one failure does not stop the others");
  assert.equal(creds[0].disabled, false);
  await page.getByLabel("Select account-a.json",{exact:true}).uncheck();
  await page.getByText("1 selected",{exact:true}).waitFor();
  acceptDialogs = true;
  await page.getByRole("button",{name:"Delete",exact:true}).click();
  await page.getByText("1 account file deleted.",{exact:true}).waitFor();
  assert.equal(lastDialog, "Delete 1 account file (1 account) from the server?", "the confirm counts the accounts the files hold");
  acceptDialogs = false;
  assert.deepEqual(deleteBodies, [{names:["disabled.json"]}]);
  await page.getByText(/^Select accounts to enable/).waitFor();
  delete creds[0].disabled; creds[1].disabled = true;
  config = {"api-keys":{claude:[{name:"anthropic",keys:[{"api-key":"sk-claude"}]}],
    "openai-compatibility":[{name:"router","base-url":"https://models.example.test/v1/ ",headers:{"X-Org":" org-1 ",authorization:"Bearer group",Empty:"","X-Client":"$X-Client","X-Session":"s-$CPA-SESSION-ID"},
    keys:[{"api-key":" sk-test-1 ","proxy-url":"socks5://127.0.0.1:1"}],models:[{name:"m-b"}]}]}};
  await page.goto("http://dashboard.test/management.html#providers");
  await page.reload();
  await page.getByRole("button",{name:/^OpenAI-compatible/}).click();
  await page.getByRole("button",{name:"Import models",exact:true}).click();
  await page.getByText("2 offered",{exact:true}).waitFor();
  assert.deepEqual(apiCalls, [{method:"GET",url:"https://models.example.test/v1/models",header:{"X-Org":"org-1",authorization:"Bearer group"},proxy_url:"socks5://127.0.0.1:1"}],
    "the list is read with inference's headers: a group header replaces the key's token");
  await page.getByRole("button",{name:/^Claude/}).click();
  assert.equal(await page.getByText("2 offered").count(), 0, "an offer never shows under another provider's group");
  await page.getByRole("button",{name:/^OpenAI-compatible/}).click();
  assert.equal(await page.getByText("2 offered").count(), 0, "switching provider closes the offer");
  await page.getByRole("button",{name:"Import models",exact:true}).click();
  await page.getByText("2 offered",{exact:true}).waitFor();
  assert.equal(await page.getByLabel("m-b",{exact:true}).isDisabled(), true, "a configured model stays as configured");
  await page.getByLabel("Filter offered models").fill("A");
  assert.equal(await page.getByLabel("m-b",{exact:true}).count(), 0);
  await page.getByLabel("m-a",{exact:true}).check();
  await page.getByRole("button",{name:"Add 1 selected",exact:true}).click();
  await page.getByText("1 model added.",{exact:true}).waitFor();
  assert.deepEqual(config["api-keys"]["openai-compatibility"][0].models, [{name:"m-b"},{name:"m-a"}]);
  await page.getByText("2 offered").waitFor({state:"hidden"});
  config = {oauth:{"excluded-models":{claude:["claude-old-*"]}}};
  await page.goto("http://dashboard.test/management.html#models");
  await page.reload();
  const fresh = page.getByRole("switch",{name:"claude-new-1 enabled",exact:true}), old = page.getByRole("switch",{name:"claude-old-1 enabled",exact:true});
  await fresh.waitFor();
  assert.equal(await fresh.getAttribute("aria-checked"), "true");
  assert.equal(await old.getAttribute("aria-checked"), "false", "a wildcard entry switches its matches off");
  assert.equal(await old.isDisabled(), true, "a model a wildcard excludes cannot be switched on alone");
  await page.getByText("Excluded by claude-old-*",{exact:true}).waitFor();
  await page.getByText("2 models",{exact:true}).waitFor();
  await fresh.click();
  await page.getByText("claude-new-1 excluded.",{exact:true}).waitFor();
  assert.deepEqual(config.oauth["excluded-models"].claude, ["claude-old-*", "claude-new-1"], "switching off adds the model ID");
  assert.equal(await fresh.getAttribute("aria-checked"), "false");
  await page.getByRole("button",{name:"Allow claude-new-1",exact:true}).waitFor();
  await fresh.click();
  await page.getByText("claude-new-1 enabled.",{exact:true}).waitFor();
  assert.deepEqual(config.oauth["excluded-models"].claude, ["claude-old-*"], "switching on removes the model ID");
  await page.getByLabel("Model or pattern").fill(" Claude-New-1 ");
  await page.getByRole("button",{name:"Exclude",exact:true}).click();
  await page.getByText("Model excluded.",{exact:true}).waitFor();
  assert.deepEqual(config.oauth["excluded-models"].claude, ["claude-old-*", "claude-new-1"], "the pattern field adds an entry as the switch does");
  assert.equal(await fresh.getAttribute("aria-checked"), "false", "an entry typed in any case switches its model off");
  await fresh.click();
  await page.getByText("claude-new-1 enabled.",{exact:true}).waitFor();
  await page.getByRole("button",{name:"Allow claude-old-*",exact:true}).click();
  await page.getByText("Every model is available.",{exact:true}).waitFor();
  assert.equal(await old.getAttribute("aria-checked"), "true", "removing the wildcard switches its matches on");
  assert.equal(await old.isDisabled(), false);
  await page.goto("http://dashboard.test/management.html#tools");
  await page.locator('.section-tabs a[aria-current]').filter({hasText:"Tools"}).waitFor();
  const toolsTable = page.getByRole("table",{name:"Tools",exact:true});
  const dead = toolsTable.getByRole("row",{name:/^search/});
  await dead.getByText("Dead weight",{exact:true}).waitFor();
  assert.equal(await dead.getByText("40%",{exact:true}).count(),1,"carried share reads as a percent");
  assert.deepEqual(await toolsTable.locator("tbody th[colspan]").allInnerTexts(),["Built in","docs · 1 tool · carried by 4 · 0 calls · 0 errors"],"built-in tools first, then each MCP server");
  const read = toolsTable.getByRole("row",{name:/^Read/});
  assert.deepEqual(await read.locator(".chip").allInnerTexts(),["Heavy","Bloating"]);
  assert.equal(await read.getByText("146.5 KB",{exact:true}).count(),1,"p95 result size in KB");
  await page.getByRole("table",{name:"Skills",exact:true}).getByRole("row",{name:/^pdf/}).getByText("4.0 KB",{exact:true}).waitFor();
  await page.getByText("The server cannot tell whether a tool's result helped.",{exact:true}).waitFor();
  await page.getByText(/40 sends · 10 conversations with tools/).waitFor();
  await page.getByText(/3 skipped as not Claude Messages · 2 with a cut body · 1 unreadable file/).waitFor();
  const toolsRange = toolsChecks;
  await page.getByRole("group",{name:"History range"}).getByRole("button",{name:"7 days",exact:true}).click();
  await dead.getByText("Dead weight",{exact:true}).waitFor();
  assert.equal(toolsChecks - toolsRange,1,"range selection loads tools once");
  await page.clock.fastForward(600_000);
  assert.equal(toolsChecks - toolsRange,1,"tools do not poll");
  await page.setViewportSize({width:390,height:844});
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),false,"the tools tables scroll inside their cards");
  await page.setViewportSize({width:1280,height:720});
  toolsConflict = true;
  await page.reload();
  await page.getByText("Body history is off. Turn on observability.history to see tools.",{exact:true}).waitFor();
  assert.equal(await page.getByRole("table").count(),0,"history off shows no tables");
  toolsConflict = false;
  toolsFailure = true;
  await page.reload();
  await page.getByText("Tools could not be loaded.",{exact:true}).waitFor();
  toolsFailure = false;
  config = {};
  await page.goto("http://dashboard.test/management.html#logs");
  const status = page.getByRole("group",{name:"Status"}), lines = page.locator(".log > div");
  await status.waitFor();
  assert.equal(await lines.count(), 4, "the dashboard's own management calls stay hidden");
  assert.equal(await page.locator("option").filter({hasText:"/v8/"}).count(), 0, "hidden lines offer no path");
  await status.getByRole("button",{name:/^5xx/}).click();
  assert.equal(await lines.count(), 1, "a status class keeps only matching access lines");
  await page.getByRole("group",{name:"Method"}).getByRole("button",{name:/^GET/}).click();
  await page.getByText("No lines match.",{exact:true}).waitFor();
  await page.getByRole("button",{name:"Clear filters",exact:true}).click();
  await page.getByLabel("Path",{exact:true}).selectOption("/v1/messages");
  assert.equal(await lines.count(), 2, "a path matches without its query");
  await page.getByRole("button",{name:"Clear filters",exact:true}).click();
  assert.equal(await lines.count(), 4);
  await page.getByRole("button",{name:"Copy shown",exact:true}).click();
  await page.getByText("Copying needs the dashboard on HTTPS or localhost.",{exact:true}).waitFor();
  await page.addInitScript(() => Object.defineProperty(navigator, "clipboard", {value: {writeText: async t => { window.copied = t; }}}));
  await page.reload();
  await page.getByRole("button",{name:"Copy shown",exact:true}).click();
  await page.getByText("4 lines copied.",{exact:true}).waitFor();
  assert.equal(await page.evaluate(() => window.copied), logLines.slice(0, 4).join("\n"), "copy takes the raw lines shown");
  await lines.nth(1).dblclick();
  await page.getByText("Line copied.",{exact:true}).waitFor();
  assert.equal(await page.evaluate(() => window.copied), logLines[1]);
  await page.getByLabel("Wrap lines",{exact:true}).uncheck();
  assert.equal(await lines.first().evaluate(e => getComputedStyle(e).whiteSpace), "pre", "unwrapped lines scroll sideways");
  await page.getByRole("button",{name:"Full screen",exact:true}).click();
  await page.getByRole("button",{name:"Exit full screen",exact:true}).click();
  await page.getByRole("button",{name:"Full screen",exact:true}).waitFor();
  const errorRow = name => page.locator(".list .item").filter({hasText:name});
  await errorRow("error-small.log").getByRole("button",{name:"Open",exact:true}).click();
  assert.equal(await page.getByLabel("error-small.log").innerText(), "upstream status 502");
  assert.equal(await page.getByText("Showing the first 256 KB.",{exact:false}).count(), 0);
  await errorRow("error-large.log").getByRole("button",{name:"Open",exact:true}).click();
  await page.getByText("Showing the first 256 KB. Download the file for the rest.",{exact:true}).waitFor();
  assert.equal((await page.getByLabel("error-large.log").innerText()).includes("TAIL"), false, "a large file is cut at 256 KB");
  await page.getByRole("button",{name:"Close",exact:true}).click();
  assert.equal(await page.getByLabel("error-large.log").count(), 0);
  await page.setViewportSize({width:390,height:844});
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),false);
  assert.deepEqual(errors, []);
  console.log("PASS accounts, history, truthful status, distinct pages, draft guard, outcomes, manual quota checks, policy errors, log filters and copy, error log viewer, server clock skew, account batches, model import, model switches, tools and skills, mobile and four destinations");
} catch(error) { console.log(await page.locator("body").innerText(), errors); await page.screenshot({path:join(tmpdir(),"cliproxy-dashboard-failure.png"),fullPage:true}); throw error; } finally { await browser.close(); }
