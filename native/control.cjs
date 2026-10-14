"use strict";
const fs = require("node:fs"),
  path = require("node:path"),
  https = require("node:https");
const {
  management,
  readManagedAccounts,
  localManagement,
} = require("./credentials.cjs");
function settings(config) {
  if (!fs.existsSync(config.settingsFile))
    return { preferredAccount: null, names: {} };
  const s = JSON.parse(fs.readFileSync(config.settingsFile, "utf8"));
  if (
    (s.preferredAccount !== null &&
      s.preferredAccount !== undefined &&
      typeof s.preferredAccount !== "string") ||
    (s.names !== undefined &&
      (typeof s.names !== "object" ||
        s.names === null ||
        Array.isArray(s.names)))
  )
    throw Error("Invalid native settings");
  return { preferredAccount: s.preferredAccount || null, names: s.names || {} };
}
function saveSettings(config, s) {
  const t = config.settingsFile + "." + process.pid + ".tmp";
  fs.writeFileSync(t, JSON.stringify(s, null, 2) + "\n", { mode: 0o600 });
  fs.renameSync(t, config.settingsFile);
}
async function accounts(config) {
  const all = await readManagedAccounts(config),
    s = settings(config);
  return all
    .map((a) => ({ ...a, name: s.names[a.id] || a.name }))
    .sort(
      (a, b) => (b.id === s.preferredAccount) - (a.id === s.preferredAccount),
    );
}
async function resolveAccount(config, name) {
  const all = await accounts(config);
  const matches = all.filter((a) =>
    [a.id, a.fileId, a.name].some(
      (v) => v?.toLowerCase() === name?.toLowerCase(),
    ),
  );
  if (matches.length !== 1)
    throw Error(
      matches.length
        ? "Account name is ambiguous; use its ID"
        : "Unknown account; run accounts",
    );
  return matches[0];
}
async function toggle(config, name, enabled) {
  const a = await resolveAccount(config, name);
  return management(config, "/v8/management/credentials/status", {
    method: "PATCH",
    body: JSON.stringify({ name: a.fileId, disabled: !enabled }),
  });
}
async function refresh(config, name) {
  const a = await resolveAccount(config, name);
  return management(config, "/v8/management/credentials/refresh", {
    method: "POST",
    body: JSON.stringify({ name: a.fileId }),
  });
}
async function setPreference(config, name) {
  const s = settings(config);
  s.preferredAccount =
    name === "none" ? null : (await resolveAccount(config, name)).id;
  saveSettings(config, s);
  return s;
}
async function rename(config, name, label) {
  if (
    typeof label !== "string" ||
    !label.trim() ||
    label.length > 80 ||
    /[\x00-\x1f]/.test(label)
  )
    throw Error("Name must be 1–80 printable characters");
  const a = await resolveAccount(config, name),
    s = settings(config);
  s.names[a.id] = label.trim();
  saveSettings(config, s);
  return s;
}
function status(config) {
  return new Promise((resolve, reject) => {
    const req = https.get(
      {
        hostname: "localhost",
        port: config.port,
        path: "/status",
        ca: fs.readFileSync(config.ca),
        headers: {
          authorization:
            "Bearer " + fs.readFileSync(config.proxySecret, "utf8").trim(),
        },
        agent: false,
      },
      (res) => {
        let b = "";
        res.on("data", (c) => {
          b += c;
          if (b.length > 1048576) req.destroy(Error("Status too large"));
        });
        res.on("end", () => {
          if (res.statusCode !== 200)
            return reject(Error("Router status returned " + res.statusCode));
          try {
            resolve(JSON.parse(b));
          } catch {
            reject(Error("Invalid router status"));
          }
        });
      },
    );
    req.setTimeout(45000, () => req.destroy(Error("Routing health timed out")));
    req.on("error", (error) =>
      reject(
        Error(
          "Native proxy unavailable (" +
            (error.code || "connection error") +
            ")",
        ),
      ),
    );
  });
}
async function login(config) {
  return management(
    config,
    "/v8/management/oauth/auth-url?provider=claude&is_webui=true",
  );
}
async function loginStatus(config, state) {
  return management(
    config,
    "/v8/management/oauth/status?state=" + encodeURIComponent(state),
  );
}
module.exports = {
  settings,
  accounts,
  resolveAccount,
  toggle,
  refresh,
  setPreference,
  rename,
  status,
  login,
  loginStatus,
  localManagement,
};
