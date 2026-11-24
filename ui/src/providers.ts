import { credName, type Data } from "./core.ts";

export type Totals = {
  requests: number;
  sends: number;
  failed: number;
  rateLimited: number;
  failedOver: number;
  uncachedInputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  outputTokens: number;
  cancelled: number;
  exactSends: number;
};

type Rates = {
  totals: Totals;
  sendShare: number | null;
  outputShare: number | null;
  failureRate: number | null;
  cacheReadRate: number | null;
};

export type AccountRow = Rates & {
  credential: string;
  name: string;
  peak: number | null;
  unused: number | null;
  unusedInRange: boolean;
  tokensPerPercent: number | null;
  tokensPerPercentInRange: boolean;
  tokensPartial: boolean;
};

export type ProviderRow = Rates & {
  provider: string;
  unusedAccounts: number | null;
  endedAccounts: number;
  accounts: AccountRow[];
};

export type Block = {
  at: number;
  state: "unknown" | "idle" | "rejected" | "rated";
  band: 0 | 1 | 2 | 3 | null;
  rate: number | null;
};

const fields: (keyof Totals)[] = [
  "requests",
  "sends",
  "failed",
  "rateLimited",
  "failedOver",
  "uncachedInputTokens",
  "cacheReadTokens",
  "cacheWriteTokens",
  "outputTokens",
  "cancelled",
  "exactSends",
];

const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : 0);
const list = (v: unknown): Data[] => (Array.isArray(v) ? v : []);
const ratio = (n: number, d: number) => (d > 0 ? n / d : null);

function totalsOf(row: Data | undefined): Totals {
  return Object.fromEntries(fields.map((f) => [f, num(row?.[f])])) as Totals;
}

function rates(totals: Totals, sends: number, output: number): Rates {
  return {
    totals,
    sendShare: ratio(totals.sends, sends),
    outputShare: ratio(totals.outputTokens, output),
    failureRate: ratio(totals.failed, totals.sends - totals.cancelled - totals.rateLimited),
    cacheReadRate: ratio(
      totals.cacheReadTokens,
      totals.uncachedInputTokens + totals.cacheReadTokens + totals.cacheWriteTokens,
    ),
  };
}

const latest = (cycles: Data[], keep: (c: Data) => boolean) =>
  cycles.filter(keep).reduce<Data | undefined>((a, c) => (!a || num(c.start) >= num(a.start) ? c : a), undefined);

/** Provider rows with their accounts, from the activity route's totals and weekly cycles. */
export function providerActivity(data: Data, creds: Data[]): ProviderRow[] {
  const providers = list(data.providers);
  const credentials = list(data.credentials);
  const cycles = list(data.cycles);
  const sends = providers.reduce((n, p) => n + num(p.sends), 0);
  const output = providers.reduce((n, p) => n + num(p.outputTokens), 0);
  const names = new Map(creds.map((a) => [String(a.id || a.name), credName(a)]));

  const ids = new Set([...providers, ...cycles].map((p) => String(p.provider)));
  const rows = [...ids].map((provider): ProviderRow => {
    const accountIds = new Set(
      [...credentials, ...cycles].filter((c) => c.provider === provider).map((c) => String(c.credential)),
    );
    const accounts = [...accountIds].map((credential): AccountRow => {
      const own = cycles.filter((c) => c.provider === provider && c.credential === credential);
      const peak = latest(own, (c) => c.inRange === true);
      const ended = latest(own, (c) => c.ended === true && typeof c.unused === "number");
      const perPercent = latest(own, (c) => typeof c.tokensPerPercent === "number");
      const totals = totalsOf(credentials.find((c) => c.provider === provider && c.credential === credential));
      return {
        credential,
        name: names.get(credential) ?? credential,
        ...rates(totals, sends, output),
        peak: peak ? num(peak.peak) : null,
        unused: ended ? num(ended.unused) : null,
        unusedInRange: ended?.inRange !== false,
        tokensPerPercent: perPercent ? num(perPercent.tokensPerPercent) : null,
        tokensPerPercentInRange: perPercent?.inRange !== false,
        tokensPartial: perPercent?.partial === true,
      };
    });
    accounts.sort(
      (a, b) => (b.unused ?? -1) - (a.unused ?? -1) || a.name.localeCompare(b.name),
    );
    const withUnused = accounts.filter((a) => a.unused !== null);
    return {
      provider,
      ...rates(totalsOf(providers.find((p) => p.provider === provider)), sends, output),
      unusedAccounts: withUnused.length
        ? withUnused.reduce((n, a) => n + (a.unused as number) / 100, 0)
        : null,
      endedAccounts: withUnused.length,
      accounts,
    };
  });
  return rows.sort(
    (a, b) =>
      (b.unusedAccounts ?? -1) - (a.unusedAccounts ?? -1) || a.provider.localeCompare(b.provider),
  );
}

/** One block per time bucket of the range, with the provider's success rate and band. */
export function healthBlocks(data: Data, provider: string): Block[] {
  const from = Number(data.from);
  const to = Number(data.to);
  const width = Number(data.bucketMs);
  if (
    !Number.isSafeInteger(from) ||
    !Number.isSafeInteger(to) ||
    !Number.isSafeInteger(width) ||
    width <= 0 ||
    to <= from ||
    (to - from) / width > 64
  )
    return [];
  const oldest = typeof data.oldestSend === "number" ? data.oldestSend : null;
  const byStart = new Map<number, Data>();
  for (const row of list(data.buckets)) if (row.provider === provider) byStart.set(Number(row.at), row);
  const blocks: Block[] = [];
  for (let at = Math.floor(from / width) * width; at < to; at += width) {
    const row = byStart.get(at);
    const counted = num(row?.sends) - num(row?.rateLimited) - num(row?.cancelled);
    if (oldest === null || at + width <= oldest) blocks.push({ at, state: "unknown", band: null, rate: null });
    else if (!row || num(row.sends) === 0) blocks.push({ at, state: "idle", band: null, rate: null });
    else if (counted <= 0) blocks.push({ at, state: "rejected", band: null, rate: null });
    else {
      const rate = 1 - num(row.failed) / counted;
      blocks.push({ at, state: "rated", rate, band: rate < 0.5 ? 0 : rate < 0.65 ? 1 : rate < 0.8 ? 2 : 3 });
    }
  }
  return blocks;
}
