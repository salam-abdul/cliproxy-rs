import type { Data } from "./core.ts";

export type Group = { name: string; server: Data | null; tools: Data[] };

const list = (v: unknown): Data[] => (Array.isArray(v) ? v : []);
const labels: Record<string, string> = { dead: "Dead weight", heavy: "Heavy", failing: "Failing", bloating: "Bloating" };

/** Tools without a server under "Built in" first, then each MCP server by carried, tools in the server's order. */
export function toolGroups(data: Data): Group[] {
  const tools = list(data.tools);
  const servers = new Map(list(data.servers).map((s) => [String(s.name), s]));
  const groups = new Map<string | null, Data[]>();
  for (const t of tools) {
    const key = t.server == null ? null : String(t.server);
    groups.set(key, [...(groups.get(key) || []), t]);
  }
  const carried = (key: string | null) => Number(servers.get(key ?? "")?.carried) || 0;
  return [...groups.entries()]
    .sort(([a], [b]) =>
      a === null ? -1 : b === null ? 1 : carried(b) - carried(a) || a.localeCompare(b),
    )
    .map(([key, tools]) => ({ name: key ?? "Built in", server: key === null ? null : servers.get(key) || null, tools }));
}

export const labelText = (label: string) => labels[label] ?? label;

/** "—" for no value, whole bytes under a kilobyte, else KB or MB to one decimal. */
export function bytes(n: number | null): string {
  if (n === null) return "—";
  if (n < 1024) return `${Math.round(n)} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export const share = (n: number | null) => (n === null ? "—" : `${Math.round(n * 100)}%`);
