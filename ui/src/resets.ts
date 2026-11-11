import { api, ApiError } from "./api";
import { store } from "./store.svelte";
import { checkAll } from "./quota";
import { newKey, provider, resetLabel, type Data, type Reset } from "./core";

/** Enabled Claude and Codex sign-ins; the server refuses the rest, such as setup tokens. */
export const hasResets = (a: Data) =>
  !a.disabled &&
  a.account_type !== "api_key" &&
  ["claude", "codex"].includes(provider(a)) &&
  !store.resets[a.auth_index]?.unsupported;

/** Reads the account's resets: from a read at most an hour old, or a new one when `fresh`. */
export async function loadResets(a: Data, fresh = false) {
  if (!hasResets(a)) return;
  try {
    const query = `auth_index=${encodeURIComponent(a.auth_index)}${fresh ? "&fresh=1" : ""}`;
    store.resets[a.auth_index] = await api(`/quota/resets?${query}`);
  } catch (e) {
    // A 400 means this account can never have resets.
    store.resets[a.auth_index] =
      e instanceof ApiError && e.status === 400
        ? { unsupported: true }
        : { error: e instanceof Error ? e.message : String(e) };
  }
}

// A claim whose outcome was never confirmed keeps its request id, so retrying it
// cannot spend a second reset: the provider answers that the first one landed.
const unsettled = new Map<string, string>();

export async function applyReset(a: Data, r: Reset) {
  if (!confirm(`Use this ${resetLabel(r).toLowerCase()}? ${r.detail}`)) return;
  const slot = `${a.auth_index}:${r.id}`;
  const request = unsettled.get(slot) || newKey();
  unsettled.set(slot, request);
  await store.act(async () => {
    const claim = await api("/quota/resets", "POST", { auth_index: a.auth_index, reset_id: r.id, request_id: request });
    if (claim.outcome !== "unconfirmed") unsettled.delete(slot);
    const until = claim.until ? ` Try again after ${new Date(claim.until).toLocaleString()}.` : "";
    store.notify(claim.message + until, !["applied", "nothing_to_reset"].includes(claim.outcome));
    await loadResets(a);
    if (claim.outcome === "applied") {
      await checkAll([a]);
      await store.native.load(true);
    }
  });
}
