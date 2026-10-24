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
let checks = 0, fail = false, hold, release, native = false, overage = false, policyFailure = false;
const creds = [{ name: "account-a.json", auth_index: "a", provider: "claude", status: "active" },
  { name: "disabled.json", auth_index: "b", provider: "claude", disabled: true }];
await page.route("**/*", async route => {
  const request = route.request(), path = new URL(request.url()).pathname;
  const send = body => route.fulfill({ json: body, headers: { "x-cpa-version": "8.0.0", "x-cpa-commit": "synthetic" } });
  if (path === "/v8/management/observability/native/status") return native === null ? route.fulfill({ status: 501, json: {} }) : send({ enabled: native, allowExtraUsage: overage });
  if (path === "/v8/management/routing/native-policy") {
    assert.equal(request.method(), "PATCH");
    if (policyFailure) return route.fulfill({status:500,json:{error:"Could not save policy"}});
    overage = request.postDataJSON().allowExtraUsage;
    return send({allowExtraUsage:overage});
  }
  if (path === "/v8/management/config") return send({});
  if (path === "/v8/management/credentials") return send({ files: creds });
  if (path === "/v8/management/requests/api-call") {
    const body = request.postDataJSON();
    assert.equal(body.authIndex, "a");
    assert.equal(body.method, "GET");
    assert.equal(body.url, "https://api.anthropic.com/api/oauth/usage");
    checks++;
    if (hold) await hold;
    return send({ status_code: fail ? 503 : 200, body: { five_hour: { utilization: 23, resets_at: "2030-01-01T00:00:00Z" }, seven_day_opus: {utilization:42}, extra_usage:{is_enabled:true,used_credits:100,monthly_limit:1000} } });
  }
  if (path.startsWith("/v8/")) throw new Error(`Unexpected API: ${path}`);
  const asset = path === "/management.html" ? "index.html" : path.slice(1);
  assert.match(asset, /^(index\.html|(assets|fonts)\/[\w.-]+\.(js|css|woff2))$/);
  return route.fulfill({ body: readFileSync(resolve("dist", asset)), contentType: asset.endsWith(".js") ? "text/javascript" : asset.endsWith(".css") ? "text/css" : "text/html" });
});
const settle = async () => { await page.evaluate(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))); };
try {
  await page.clock.install();
  await page.goto("http://dashboard.test/management.html");
  await page.getByText("Checked just now", { exact: true }).waitFor({ timeout: 5000 }).catch(async e => { console.log(await page.locator("body").innerText(), errors, checks); throw e; });
  assert.equal(await page.getByRole("switch", {name:/Allow paid overage/}).count(), 0);
  assert.equal(checks, 1, "automatically checks only the enabled account");
  assert.equal(await page.locator(".side .nav a").count(), 6);
  await page.locator('.side a[href="#credentials"]').click();
  await page.locator('.section-tabs a[href="#quotas"]').click();
  await settle();
  assert.equal(checks, 1, "navigation shares the freshness cache");
  await page.clock.fastForward(61_000);
  await settle();
  assert.equal(checks, 2, "visible quota view refreshes automatically");
  await page.evaluate(() => { Object.defineProperty(document, "hidden", { configurable: true, value: true }); document.dispatchEvent(new Event("visibilitychange")); });
  await page.clock.fastForward(120_000);
  await settle();
  assert.equal(checks, 2, "hidden tabs do not check providers");
  fail = true;
  await page.evaluate(() => { Object.defineProperty(document, "hidden", { configurable: true, value: false }); document.dispatchEvent(new Event("visibilitychange")); });
  await page.getByText(/The provider answered HTTP 503/).waitFor();
  assert.equal(checks, 3, "resuming a stale visible tab checks immediately");
  assert.ok(await page.getByText(/23%/).count(), "a failed refresh retains the last quota");
  fail = false;
  hold = new Promise(r => release = r);
  await page.getByRole("button", { name: "Check quota", exact: true }).first().click();
  await page.getByRole("button", { name: "Check quota", exact: true }).first().click();
  assert.equal(checks, 4, "manual requests coalesce while pending");
  release(); hold = null;
  await page.getByText(/The provider answered HTTP 503/).waitFor({ state: "hidden" });
  await page.locator('.side a[href="#config"]').click();
  await settle();
  assert.equal(await page.locator(".section-tabs a").count(), 5);
  await page.clock.fastForward(120_000);
  await settle();
  assert.equal(checks, 4, "leaving the quota view stops provider polling");
  await page.setViewportSize({ width: 390, height: 844 });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
  native = true;
  await page.reload();
  await page.locator(".side .nav").waitFor({state:"attached"});
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
  creds[0].quota = {observed_at:new Date(await page.evaluate(() => Date.now()) + 1000).toISOString(), signals:{
    "Anthropic-Ratelimit-Unified-5h-Utilization":"0.36",
    "Anthropic-Ratelimit-Unified-5h-Reset":"2000000000",
    "Anthropic-Ratelimit-Unified-Overage-Status":"rejected",
  }};
  await page.goto("http://dashboard.test/management.html#overview");
  assert.equal(await page.getByRole("switch", {name:/Allow paid overage/}).count(), 0, "policy is absent from Overview");
  assert.equal(await page.getByText(/Native Claude routing · Account switches/).count(), 0);
  const meter = page.getByRole("meter", {name:"Current session used"});
  await meter.waitFor();
  assert.equal(await meter.getAttribute("aria-valuenow"), "36");
  assert.equal(await page.getByText("Extra usage", {exact:true}).count(), 1, "partial response headers retain REST-only spending");
  assert.equal(await page.getByRole("meter", {name:"Current week (Opus only) used"}).getAttribute("aria-valuenow"), "42");
  assert.equal(await page.getByText("Anthropic-Ratelimit-Unified-Overage-Status: rejected", {exact:true}).isVisible(), false);
  native = null;
  await page.reload();
  await page.locator(".side .nav").waitFor({state:"attached"});
  assert.equal(await page.getByRole("switch", {name:/Allow paid overage/}).count(), 0);
  if (process.env.DASHBOARD_CAPTURE) await page.screenshot({ path: process.env.DASHBOARD_CAPTURE, fullPage: true });
  assert.deepEqual(errors, []);
  console.log("PASS grouped navigation, automatic quota refresh, hidden/resume, disabled accounts, coalescing, retained errors, cleanup, mobile layout, native scope and unsupported status");
} finally { await browser.close(); }
