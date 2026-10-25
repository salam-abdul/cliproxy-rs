import { credState, provider, type Data, type CredState } from "./core.ts";

export function accountStatus(
  a: Data,
  native: Data | undefined,
  error: unknown,
  model: string,
  now: number,
): CredState & { available?: boolean } {
  const fallback = credState(a, now);
  if (a.disabled) return fallback;
  if (provider(a) === "claude" && native?.enabled) {
    if (error || !(native.validUntil > now))
      return {
        lamp: "warn",
        label: "Availability unknown",
        detail: "Status needs a successful refresh.",
      };
    if (native.model !== model)
      return { lamp: "off", label: "Checking availability", detail: model };
    const account = native.accounts?.find(
      (n: Data) => n.fileId === (a.id || a.name),
    );
    const states: Record<string, [CredState["lamp"], string]> = {
      disabled: ["off", "Disabled"],
      signin: ["bad", "Sign in again"],
      qualification: ["warn", "Qualification mode"],
      cooling: ["warn", "Cooling down"],
      included: ["ok", "Included quota available"],
      extra: ["warn", "Paid extra usage available"],
      limited: ["warn", "Included quota exhausted"],
      unknown: ["warn", "Availability unknown"],
    };
    const [lamp, label] = states[account?.availability] || states.unknown;
    return {
      lamp,
      label,
      available: ["included", "extra"].includes(account?.availability),
      detail:
        account?.retryAt > now
          ? `Retry after ${new Date(account.retryAt).toLocaleString()}`
          : "",
    };
  }
  return fallback.lamp === "ok"
    ? {
        lamp: "off",
        label: "Enabled · availability not reported",
        detail: fallback.detail,
      }
    : fallback;
}

export function requestOutcomes(events: Data[]): Data[] {
  const groups = new Map<string, Data[]>();
  for (const event of events) {
    if (!event.requestId) continue;
    const group = groups.get(event.requestId) || [];
    group.push(event);
    groups.set(event.requestId, group);
  }
  return [...groups.values()]
    .map((group): Data => {
      group.sort((a, b) => Date.parse(b.at) - Date.parse(a.at));
      const final = group.find((e) =>
        ["inference", "rejected", "transport-failure"].includes(e.type),
      );
      return {
        ...(final || group[0]),
        outcome: final?.outcome || (final ? final.type : "pending"),
        events: group,
      };
    })
    .sort((a, b) => Date.parse(b.at) - Date.parse(a.at));
}
