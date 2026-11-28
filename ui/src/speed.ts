import { credName, type Data } from "./core.ts";

export type Part = "server" | "earlier" | "first" | "generation";

export type Bar = {
  provider: string;
  source: string;
  total: number;
  width: number;
  segments: { part: Part; ms: number; share: number }[];
};

export type Row = {
  provider: string;
  source: string;
  model: string | null;
  ttftKind: string;
  requests: number;
  firstP50: number | null;
  firstP95: number | null;
  totalP50: number | null;
  totalP95: number | null;
  tokensPerSecond: number | null;
  decodeSpeed: number | null;
  firstEvent: boolean;
};

const parts: Part[] = ["server", "earlier", "first", "generation"];
const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : 0);
const orNull = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : null);
const list = (v: unknown): Data[] => (Array.isArray(v) ? v : []);

/** "—" for no value, whole milliseconds under a second, else seconds to one decimal. */
export function duration(ms: number | null): string {
  if (ms === null) return "—";
  const whole = Math.round(ms);
  return whole < 1000 ? `${whole} ms` : `${(ms / 1000).toFixed(1)} s`;
}

/** One bar per provider and source, scaled to the largest total, longest first. */
export function partBars(data: Data): Bar[] {
  const entries = list(data.parts);
  const widest = entries.reduce((n, p) => Math.max(n, num(p.total)), 0);
  const bars = entries.map((p): Bar => {
    const total = num(p.total);
    return {
      provider: String(p.provider),
      source: String(p.source),
      total,
      width: widest > 0 ? total / widest : 0,
      segments: parts.map((part) => ({ part, ms: num(p[part]), share: total > 0 ? num(p[part]) / total : 0 })),
    };
  });
  return bars.sort(
    (a, b) => b.total - a.total || a.provider.localeCompare(b.provider) || a.source.localeCompare(b.source),
  );
}

const sliceNames: Record<string, string> = {
  model: "Model",
  credential: "Account",
  size: "Request size",
  cache: "Cache",
  stream: "Streamed",
  hour: "Hour",
};

/** One sentence per slow slice, in the server's order (largest ratio first). */
export function slowLines(data: Data, providerName: (id: string) => string, creds: Data[] = []): string[] {
  const accounts = new Map(creds.map((a) => [String(a.id || a.name), credName(a)]));
  return list(data.slow).map((s) => {
    const slice = String(s.slice);
    const value =
      slice === "stream"
        ? s.value === true
          ? "yes"
          : "no"
        : slice === "hour"
          ? `${String(num(s.value)).padStart(2, "0")}:00`
          : slice === "credential"
            ? (accounts.get(String(s.value)) ?? String(s.value))
            : String(s.value);
    const p95 = num(s.p95);
    const baseline = num(s.baseline);
    const against =
      baseline > 0 ? `${(p95 / baseline).toFixed(1)} times the rest` : `against ${duration(baseline)} for the rest`;
    return `${sliceNames[slice] ?? slice} ${value} on ${providerName(String(s.provider))} (${s.source}): first response p95 ${duration(p95)}, ${against} (${num(s.requests)} requests)`;
  });
}

/** The table rows; `firstEvent` marks rows whose first response is the first translated event or packet. */
export function speedRows(data: Data): Row[] {
  return list(data.rows).map((r) => ({
    provider: String(r.provider),
    source: String(r.source),
    model: typeof r.model === "string" ? r.model : null,
    ttftKind: String(r.ttftKind),
    requests: num(r.requests),
    firstP50: orNull(r.firstP50),
    firstP95: orNull(r.firstP95),
    totalP50: orNull(r.totalP50),
    totalP95: orNull(r.totalP95),
    tokensPerSecond: orNull(r.tokensPerSecond),
    decodeSpeed: orNull(r.decodeSpeed),
    firstEvent: r.ttftKind === "event",
  }));
}
