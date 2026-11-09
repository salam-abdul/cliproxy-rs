// Application log lines as the server writes them. Pure, covered by logs.test.ts.

export type Line = {
  raw: string;
  time?: string;
  id?: string;
  level?: string;
  text: string;
  /** Set only on access lines: `200 |  12ms |  127.0.0.1 | POST    "/v1/messages"`. */
  status?: number;
  method?: string;
  path?: string;
};
/** Empty strings mean "any". `status` is a class such as "4xx". */
export type Filter = { method: string; status: string; path: string };

export function parseLine(raw: string): Line {
  const m = raw.match(/^\[([^\]]*)\] \[([^\]]*)\] \[(\w+)\s*\] (?:\[[^\]]*\] )?(.*)$/);
  if (!m) return { raw, text: raw };
  const line: Line = { raw, time: m[1], id: m[2], level: m[3].toLowerCase(), text: m[4] };
  // Greedy: the logged path is percent-decoded and can hold quotes. A decoded "?" cannot be
  // told from the query separator, so it ends the path.
  const a = m[4].match(/^(\d{3}) \|\s*\S+ \|\s*\S* \| (\S+)\s+"(.*)"/);
  if (a) Object.assign(line, { status: Number(a[1]), method: a[2], path: a[3].split("?")[0] });
  return line;
}

export const statusClass = (s: number) => `${Math.floor(s / 100)}xx`;

/** A line without the filtered field fails that filter, so other lines hide while one is set. */
export const keep = (l: Line, f: Filter) =>
  (!f.method || l.method === f.method) &&
  (!f.status || (l.status !== undefined && statusClass(l.status) === f.status)) &&
  (!f.path || l.path === f.path);

/**
 * The values each filter can take in `lines`, most frequent first. A value set in `f` stays
 * listed with 0 when no line has it, so the control that clears it stays on screen.
 */
export function facets(lines: Line[], f: Filter) {
  const count = (k: keyof Filter, pick: (l: Line) => string | undefined) => {
    const n = new Map<string, number>();
    for (const l of lines) {
      const v = pick(l);
      if (v) n.set(v, (n.get(v) || 0) + 1);
    }
    if (f[k] && !n.has(f[k])) n.set(f[k], 0);
    return [...n].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
  };
  return {
    method: count("method", (l) => l.method),
    status: count("status", (l) => (l.status === undefined ? undefined : statusClass(l.status))).sort((a, b) => a[0].localeCompare(b[0])),
    path: count("path", (l) => l.path),
  };
}
