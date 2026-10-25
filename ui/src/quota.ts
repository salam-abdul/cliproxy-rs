import { api, requests } from "./api";
import type { Lifetime } from "./lifetime";
import { store } from "./store.svelte";
import {
  provider,
  quotaWindows,
  signalWindows,
  type Data,
  type Window,
} from "./core";

// Usage endpoints the server calls with the credential's own token ($TOKEN$ is substituted server-side).
const usageURL: Record<string, string> = {
  claude: "https://api.anthropic.com/api/oauth/usage",
  codex: "https://chatgpt.com/backend-api/wham/usage",
  kimi: "https://api.kimi.com/coding/v1/usages",
  "kimi-ai": "https://api.kimi.ai/coding/v1/usages",
};
export const quotaKey = (a: Data) => a.auth_index || a.name;
/** The provider has a usage endpoint this server can call. */
export const hasSource = (a: Data) =>
  !!usageURL[provider(a)] &&
  (provider(a) === "claude" && store.kind === "rust"
    ? store.can("GET", "/observability/claude/usage")
    : store.can("POST", "/requests/api-call"));
export const canCheck = (a: Data) => !a.disabled && hasSource(a);

/** What is known about one credential's limits: a live check in this tab, else passive signals. */
export function limits(
  a: Data,
):
  { windows: Window[]; at: number; error?: string } | { error: string } | null {
  const live = store.quota[quotaKey(a)];
  const observed = Date.parse(a.quota?.observed_at) || 0;
  const windows = signalWindows(provider(a), a.quota);
  if (windows.length && (!live || !("at" in live) || observed > live.at)) {
    const previous = live && "windows" in live ? live.windows : [];
    const merged = new Map(previous.map((w) => [w.label, w]));
    for (const window of windows) merged.set(window.label, window);
    return {
      windows: [...merged.values()],
      at: observed,
      error: live?.error,
    };
  }
  return live || null;
}

export function check(a: Data, automatic = false): Promise<void> {
  if (!canCheck(a)) return Promise.resolve();
  const lifetime = requests;
  return lifetime.once(
    `quota:${quotaKey(a)}`,
    () => read(a, lifetime),
    automatic ? 60_000 : 0,
  );
}

async function read(a: Data, lifetime: Lifetime) {
  const p = provider(a);
  store.quotaPending[quotaKey(a)] = true;
  try {
    let raw: Data;
    const header: Data = {
      Authorization: "Bearer $TOKEN$",
      "Content-Type": "application/json",
    };
    if (p === "claude")
      Object.assign(header, {
        "anthropic-beta": "oauth-2025-04-20",
        "User-Agent": "claude-cli/2.1.280 (external, cli)",
      });
    if (p === "codex") {
      header["User-Agent"] = "codex-tui/0.149.1";
      const account = a.account_id || a.id_token?.chatgpt_account_id;
      if (account) header["Chatgpt-Account-Id"] = account;
    }
    if (p === "claude" && store.kind === "rust") {
      raw = await api(
        `/observability/claude/usage?auth_index=${encodeURIComponent(a.auth_index)}`,
      );
    } else {
      const r = await api("/requests/api-call", "POST", {
        authIndex: a.auth_index,
        method: "GET",
        url: usageURL[p],
        header,
      });
      if (r.status_code < 200 || r.status_code >= 300)
        throw new Error(
          r.status_code === 401 || r.status_code === 403
            ? `The provider rejected the account's token (HTTP ${r.status_code}). Refresh the token, or connect the account again.`
            : `The provider answered HTTP ${r.status_code}. Try again later.`,
        );
      raw = typeof r.body === "string" ? JSON.parse(r.body) : r.body;
    }
    if (!lifetime.active) return;
    store.quota[quotaKey(a)] = {
      at: Date.now(),
      windows: quotaWindows(p, raw),
    };
  } catch (e) {
    if (!lifetime.active) return;
    store.quota[quotaKey(a)] = {
      ...store.quota[quotaKey(a)],
      error: e instanceof Error ? e.message : String(e),
    };
  } finally {
    if (lifetime.active) store.quotaPending[quotaKey(a)] = false;
  }
}

/** One check per supported credential, one after another so the provider is not hammered. */
export async function checkAll(
  list: Data[],
  automatic = false,
  active = () => true,
) {
  const lifetime = requests;
  for (const a of list) {
    if (!active() || document.hidden || !lifetime.active) break;
    if (canCheck(a)) await check(a, automatic);
  }
}
