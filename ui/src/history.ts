import type { Data } from "./core.ts";

/** How often Overview reloads history. A longer range has wider bars. */
export const historyPollMs = (range: string) =>
  range === "30d" ? 300_000 : range === "7d" ? 120_000 : 30_000;

/**
 * Request buckets over the history period. An account card passes its credential, which
 * keys `accountBuckets`, and its native id, which keys `statuses`. A Claude status reading
 * marks a bucket observed only when `statusesCount` is true.
 */
export function requestBuckets(
  history: Data,
  account?: { credential: string; id?: string },
  statusesCount = true,
) {
  const from = Number(history.from);
  const to = Number(history.to);
  const width = Number(history.bucketMs);
  if (
    !Number.isSafeInteger(from) ||
    !Number.isSafeInteger(to) ||
    !Number.isSafeInteger(width) ||
    width <= 0 ||
    to <= from ||
    (to - from) / width > 64
  )
    return [];
  const rows = account ? history.accountBuckets : history.buckets;
  const byStart = new Map<number, Data>();
  for (const row of Array.isArray(rows) ? rows : []) {
    if (account && row.credential !== account.credential) continue;
    byStart.set(Number(row.at), row);
  }
  const observed = new Set<number>();
  // An account without a native id has no status readings.
  const statuses =
    statusesCount && (!account || account.id !== undefined) && Array.isArray(history.statuses)
      ? history.statuses
      : [];
  for (const row of statuses) {
    if (
      row.model !== "claude" ||
      (account && row.account !== account.id) ||
      typeof row.at !== "number" ||
      !Number.isFinite(row.at) ||
      row.at < from ||
      row.at > to
    )
      continue;
    observed.add(Math.floor(row.at / width) * width);
  }
  const first = Math.floor(from / width) * width;
  const result = [];
  for (let at = first; at < to; at += width) {
    const row = byStart.get(at);
    result.push({
      at,
      requests: Number(row?.requests || 0),
      failures: Number(row?.failures || 0),
      known: byStart.has(at) || observed.has(at),
    });
  }
  return result;
}

export function accountHealth(
  statuses: Data[],
  account: string,
  from: number,
  to: number,
  cells = 24,
) {
  if (
    !account ||
    !Number.isFinite(from) ||
    !Number.isFinite(to) ||
    to <= from ||
    cells < 1 ||
    cells > 64
  )
    return [];
  const severity: Record<string, number> = {
    included: 1,
    extra: 2,
    unknown: 3,
    qualification: 4,
    limited: 5,
    cooling: 6,
    signin: 7,
    disabled: 8,
  };
  const result = Array<string>(cells).fill("unknown");
  const seen = Array<boolean>(cells).fill(false);
  for (const row of statuses) {
    if (
      row.account !== account ||
      row.model !== "claude" ||
      !Number.isFinite(row.at) ||
      row.at < from ||
      row.at > to
    )
      continue;
    const index = Math.min(
      cells - 1,
      Math.floor(((row.at - from) * cells) / (to - from)),
    );
    const value = String(row.availability || "unknown");
    if (!seen[index] || (severity[value] || 0) > (severity[result[index]] || 0))
      result[index] = value;
    seen[index] = true;
  }
  return result;
}
