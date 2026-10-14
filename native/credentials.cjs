"use strict";
// cliproxy-rs alone refreshes. This command-line client reads committed access tokens only.
const fs = require("node:fs"),
  path = require("node:path"),
  crypto = require("node:crypto");
function localManagement(config) {
  const u = new URL(config.managementUrl);
  if (
    !["127.0.0.1", "localhost", "[::1]"].includes(u.hostname) ||
    !["http:", "https:"].includes(u.protocol) ||
    u.username ||
    u.password
  )
    throw Error("Management must be a private loopback URL");
  return u.origin;
}
function privateFile(file) {
  const s = fs.lstatSync(file);
  if (!s.isFile() || s.isSymbolicLink() || (s.mode & 0o077) !== 0)
    throw Error("Credential and key files must be private regular files");
  return fs.readFileSync(file, "utf8");
}
async function management(config, route, options = {}) {
  const base = localManagement(config),
    key = privateFile(config.managementKeyFile).trim();
  const r = await fetch(base + route, {
    ...options,
    headers: {
      "content-type": "application/json",
      authorization: "Bearer " + key,
    },
    redirect: "error",
    signal: AbortSignal.timeout(5000),
  });
  if (!r.ok) throw Error("Account manager returned " + r.status);
  return r.json();
}
async function readManagedAccounts(config) {
  const result = await management(config, "/v8/management/credentials");
  if (!Array.isArray(result.files))
    throw Error("Invalid account manager listing");
  const root = fs.realpathSync(config.authDir),
    accounts = [];
  for (const row of result.files) {
    if ((row.provider || row.type) !== "claude" || row.runtime_only) continue;
    const id = row.id;
    if (
      typeof id !== "string" ||
      !id ||
      id.includes("\\") ||
      path.isAbsolute(id) ||
      id.split("/").includes("..")
    )
      throw Error("Unsafe credential path");
    const file = path.resolve(root, id);
    if (!file.startsWith(root + path.sep))
      throw Error("Unsafe credential path");
    const actual = fs.realpathSync(file);
    if (actual !== file || !actual.startsWith(root + path.sep))
      throw Error("Unsafe credential path");
    const a = JSON.parse(privateFile(file));
    if (a.type !== "claude") throw Error("Credential provider mismatch");
    const expires = Date.parse(a.expired || a.expires_at || "");
    const stable =
      a.native_route_id ||
      crypto
        .createHash("sha256")
        .update("claude:" + (a.organization_uuid || a.account_uuid || id))
        .digest("hex");
    if (typeof stable !== "string" || !/^[A-Za-z0-9._:-]{1,256}$/.test(stable))
      throw Error("Invalid stable account identity");
    accounts.push({
      id: stable,
      fileId: id,
      name: a.native_route_name || row.label || id,
      token: a.access_token,
      enabled:
        !row.disabled &&
        !a.disabled &&
        typeof a.access_token === "string" &&
        a.access_token.length > 0 &&
        Number.isFinite(expires) &&
        expires > Date.now() + 30000,
      disabled: !!row.disabled || !!a.disabled,
      expired: !Number.isFinite(expires) || expires <= Date.now() + 30000,
      expiresAt: expires,
    });
  }
  if (new Set(accounts.map((a) => a.id)).size !== accounts.length)
    throw Error("Duplicate stable Claude account identity");
  return accounts;
}
module.exports = { readManagedAccounts, management, localManagement };
