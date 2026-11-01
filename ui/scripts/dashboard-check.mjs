// Synthetic browser checks: all requests are intercepted; no server or provider is contacted.
// CHROME_PATH=... node scripts/dashboard-check.mjs
import { chromium } from "playwright-core";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";

const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const page = await browser.newPage();
const errors = [];
page.on("pageerror", e => errors.push(e.message));
let checks = 0, historyChecks = 0, fail = false, hold, release, native = false, overage = false, policyFailure = false;
let requireKey = false, quotaStarted, expired = false, availability = "included", saveHold, saveRelease, saveStarted;
const rust = process.env.DASHBOARD_KIND === "rust";
const creds = [{ name: "account-a.json", auth_index: "a", provider: "claude", status: "active" },
  { name: "disabled.json", auth_index: "b", provider: "claude", disabled: true }];
await page.route("**/*", async route => {
  const request = route.request(), path = new URL(request.url()).pathname;
  const send = body => route.fulfill({ json: body, headers: { "x-cpa-version": rust ? "cliproxy-rs/0.2.6" : "8.0.0", "x-cpa-commit": "synthetic" } });
  if (path === "/v8/management/observability/native/status") return native === null ? route.fulfill({ status: 501, json: {} }) : send({ enabled: native, allowExtraUsage: overage, model: new URL(request.url()).searchParams.get("model") || "claude", validUntil: expired ? 1 : 9e15, ready: availability === "included", selectionStrategy:"soonest-reset", selectionOrder:availability === "included" ? ["a"] : [], newSessionMinRemaining:5, accounts: creds.map(a => ({id:a.auth_index,fileId:a.id || a.name,name:a.name,availability:a.disabled ? "disabled" : availability})) });
  if (path === "/v8/management/routing/native-policy") {
    assert.equal(request.method(), "PATCH");
    if (policyFailure) return route.fulfill({status:500,json:{error:"Could not save policy"}});
    overage = request.postDataJSON().allowExtraUsage;
    return send({allowExtraUsage:overage});
  }
  if (path === "/v8/management/credentials/fields" && request.postData() !== "{}") {
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
  if (path === "/v8/management/config") return requireKey && !request.headers().authorization ? route.fulfill({status:401,json:{error:"management key required"}}) : send({});
  if (path === "/v8/management/credentials") return send({ files: creds });
  if (path === (rust ? "/v8/management/observability/claude/usage" : "/v8/management/requests/api-call")) {
    if (rust) {
      assert.equal(request.method(), "GET");
      assert.equal(new URL(request.url()).searchParams.get("auth_index"), "a");
    } else {
    const body = request.postDataJSON();
    assert.equal(body.authIndex, "a");
    assert.equal(body.method, "GET");
    assert.equal(body.url, "https://api.anthropic.com/api/oauth/usage");
    }
    checks++;
    quotaStarted?.();
    if (hold) await hold;
    const body = { five_hour: { utilization: 23, resets_at: "2030-01-01T00:00:00Z" }, seven_day_opus: {utilization:42}, extra_usage:{is_enabled:true,used_credits:100,monthly_limit:1000} };
    if (rust) return fail ? route.fulfill({status:502,json:{error:"Claude usage provider answered HTTP 503"}}) : send(body);
    return send({status_code: fail ? 503 : 200, body});
  }
  if (["POST", "PATCH", "PUT", "DELETE"].includes(request.method()) && request.postData() === "{}") return route.fulfill({status:400,json:{error:"invalid request"}});
  if (path.startsWith("/v8/")) throw new Error(`Unexpected API: ${path}`);
  const asset = path === "/management.html" ? "index.html" : path.slice(1);
  assert.match(asset, /^(index\.html|(assets|fonts)\/[\w.-]+\.(js|css|woff2))$/);
  return route.fulfill({ body: readFileSync(resolve("dist", asset)), contentType: asset.endsWith(".js") ? "text/javascript" : asset.endsWith(".css") ? "text/css" : "text/html" });
});
const settle = async () => { await page.evaluate(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))); };
try {
  await page.clock.install();
  await page.goto("http://dashboard.test/management.html#overview");
  await page.getByRole("meter", {name:"5-hour limit remaining"}).waitFor({timeout:5000});
  assert.equal(await page.getByRole("switch", {name:/Allow paid overage/}).count(), 0);
  assert.equal(checks, 1, "automatically checks only the enabled account");
  assert.equal(await page.locator(".top .nav a").count(), 3);
  await page.goto("http://dashboard.test/management.html#quotas/account-a.json");
  await settle();
  assert.equal(checks, 1, "legacy quota route shares Overview and freshness cache");
  await page.clock.fastForward(61_000);
  await settle();
  assert.equal(checks, 2, "visible quota view refreshes automatically");
  await page.evaluate(() => { Object.defineProperty(document, "hidden", { configurable: true, value: true }); document.dispatchEvent(new Event("visibilitychange")); });
  await page.clock.fastForward(120_000);
  await settle();
  assert.equal(checks, 2, "hidden tabs do not check providers");
  fail = true;
  await page.evaluate(() => { Object.defineProperty(document, "hidden", { configurable: true, value: false }); document.dispatchEvent(new Event("visibilitychange")); });
  await page.getByText("Last reading · refresh failed", {exact:true}).waitFor();
  assert.equal(checks, 3, "resuming a stale visible tab checks immediately");
  assert.ok(await page.getByText(/77%/).count(), "a failed refresh retains the last quota");
  fail = false;
  hold = new Promise(r => release = r);
  await page.getByRole("button", { name: "Refresh", exact: true }).first().click();
  await page.getByRole("button", { name: "Refreshing…", exact: true }).waitFor();
  assert.equal(await page.getByRole("button", { name: "Refreshing…", exact: true }).isDisabled(), true);
  assert.equal(checks, 4, "manual requests coalesce while pending");
  release(); hold = null;
  await page.getByText("Last reading · refresh failed", {exact:true}).waitFor({ state: "hidden" });
  await page.locator('.top a[href="#config"]').click();
  await settle();
  assert.equal(await page.locator(".section-tabs a").count(), 7);
  await page.clock.fastForward(120_000);
  await settle();
  assert.equal(checks, 4, "leaving the quota view stops provider polling");
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
  assert.equal(await page.getByText(/Native Claude routing · Account switches/).count(), 0);
  const meter = page.getByRole("meter", {name:"5-hour limit remaining"});
  await meter.waitFor();
  assert.equal(await meter.getAttribute("aria-valuenow"), "64");
  assert.equal(await page.getByText("Extra usage", {exact:true}).count(), 1, "partial response headers retain REST-only spending");
  assert.equal(await page.getByRole("meter", {name:"7-day limit (Opus only) remaining"}).getAttribute("aria-valuenow"), "58");

  native = null;
  await page.reload();
  await page.locator(".top .nav").waitFor({state:"attached"});
  assert.equal(await page.getByRole("switch", {name:/Allow paid overage/}).count(), 0);
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
  await pendingRequest;
  quotaStarted = undefined;
  assert.equal(checks, before + 1);
  await page.getByRole("button", {name:"Sign out", exact:true}).click();
  hold = null;
  await page.getByLabel("Management key").fill("synthetic-management");
  await page.getByRole("button", {name:"Connect", exact:true}).click();
  await page.getByRole("meter", {name:"5-hour limit remaining"}).waitFor({timeout:5000}).catch(async error => {console.log({checks,before,errors}, await page.locator("body").innerText()); throw error;});
  assert.equal(checks, before + 2, "reconnect starts fresh work despite an old pending quota read");
  fail = true;
  release();
  await settle();
  assert.equal(await page.getByText("Last reading · refresh failed", {exact:true}).count(), 0, "old failure cannot repopulate the new connection");
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
  assert.equal(await page.getByRole("meter").count(),0,"account settings do not repeat Overview quota meters");
  await page.getByText("Advanced account settings", {exact:true}).click();
  await page.getByLabel("Note",{exact:true}).fill("Synthetic draft");
  let dialogs = 0;
  page.on("dialog", async dialog => {dialogs++; await dialog.dismiss();});
  await page.locator('.top .nav a[href="#overview"]').click();
  await settle();
  assert.equal(dialogs, 1, "unsaved account fields guard navigation");
  assert.equal(await page.getByLabel("Note",{exact:true}).inputValue(), "Synthetic draft");
  await page.getByLabel("Note",{exact:true}).fill("Submitted note");
  saveHold = new Promise(r => saveRelease = r);
  const savePending = new Promise(r => saveStarted = r);
  await page.getByRole("button",{name:"Save",exact:true}).click();
  await savePending;
  await page.getByLabel("Note",{exact:true}).fill("New unsaved note");
  saveRelease(); saveHold = null;
  await page.getByText("Saved.",{exact:true}).waitFor();
  await page.locator('.top .nav a[href="#overview"]').click();
  await settle();
  assert.equal(dialogs,2,"typing during save remains dirty after the submitted version is saved");
  await page.getByLabel("Note",{exact:true}).fill("Submitted note");
  await page.locator('.top a[href="#activity"]').click();
  await page.getByText("Recovered after account switch",{exact:false}).waitFor();
  assert.equal(await page.locator(".native-event").count(),2,"two outcomes, with recovered attempts grouped");
  await page.getByRole("button",{name:"Failed",exact:true}).click();
  assert.equal(await page.locator(".native-event").count(),0,"successful failover and client cancellation are not failures");
  await page.goto("http://dashboard.test/management.html#credentials/account-a.json");
  await page.getByRole("button",{name:"Disable account",exact:true}).waitFor();
  await page.setViewportSize({width:390,height:844});
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),false);
  await page.screenshot({path:"/private/tmp/cliproxy-accounts-mobile.png",fullPage:true});
  await page.setViewportSize({width:1280,height:900});
  if (process.env.DASHBOARD_CAPTURE) await page.screenshot({ path: process.env.DASHBOARD_CAPTURE, fullPage: true });
  await page.goto("http://dashboard.test/management.html#overview");
  await page.getByRole("heading", {name:"Top models",exact:true}).waitFor();
  await page.locator(".history-model").getByText("claude-opus-4-6",{exact:true}).waitFor();
  assert.ok(historyChecks > 0,"history loads with Overview");
  await page.getByRole("button",{name:"7 days"}).click();
  await page.getByText("By requests · selected period",{exact:true}).waitFor();
  assert.ok(historyChecks > 1,"range selection loads durable history");
  await page.getByRole("button",{name:"account-a.json"}).first().click();
  await page.getByRole("dialog",{name:/account-a.json details/}).waitFor();
  await page.getByText("Availability history",{exact:true}).waitFor();
  await page.getByRole("button",{name:"Close",exact:true}).click();
  assert.equal(await page.getByText(/Claude routing:/).count(),0);
  await page.locator(".rank-badge").waitFor();
  assert.equal(await page.locator(".rank-badge").first().innerText(),"1");
  assert.equal(await page.getByLabel("Capacity model").count(),0);
  assert.equal(await page.getByText("Check Claude capacity",{exact:true}).count(),0);
  assert.equal(await page.getByText("Errors & account switches",{exact:true}).count(),0,"request details belong to Activity");
  assert.equal(await page.getByRole("heading",{name:"Tokens by category"}).count(),1);
  assert.equal(await page.getByText(/API equivalent/).count(),0);
  assert.ok(await page.getByRole("button",{name:"Refresh",exact:true}).evaluate(el => el.getBoundingClientRect().height) <= 32,"desktop controls stay compact");
  assert.equal(await page.locator(".account-columns span").count(),4,"quota and recent request columns stay together");
  assert.equal(await page.getByText(/cost/i).count(),0,"Overview does not imply token pricing");
  await page.setViewportSize({width:390,height:844});
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),false);
  assert.deepEqual(errors, []);
  console.log("PASS accounts, history, truthful status, distinct pages, draft guard, outcomes, quota polling, policy errors, mobile and three destinations");
} catch(error) { console.log(await page.locator("body").innerText(), errors); await page.screenshot({path:"/private/tmp/cliproxy-dashboard-failure.png",fullPage:true}); throw error; } finally { await browser.close(); }
