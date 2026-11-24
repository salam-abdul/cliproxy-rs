/**
 * `oauth.excluded-models` as the server applies it: each entry is a model ID or a pattern
 * where `*` matches any run of characters, compared trimmed and lowercased
 * (`apply_excluded` and `wildcard` in cpa-core's registry).
 */
export function wildcard(pattern: string, value: string): boolean {
  if (!pattern) return false;
  if (!pattern.includes("*")) return pattern === value;
  const parts = pattern.split("*");
  const first = parts[0],
    last = parts[parts.length - 1];
  if (!value.startsWith(first)) return false;
  value = value.slice(first.length);
  if (!value.endsWith(last)) return false;
  value = value.slice(0, value.length - last.length);
  for (const segment of parts.slice(1, -1)) {
    if (!segment) continue;
    const i = value.indexOf(segment);
    if (i < 0) return false;
    value = value.slice(i + segment.length);
  }
  return true;
}

const norm = (s: unknown) => String(s).trim().toLowerCase();

/** The configured list as strings; anything but a list reads as empty. */
export function entries(value: unknown): string[] {
  return Array.isArray(value) ? value.map(String) : [];
}

/**
 * Whether `id` is served, and the wildcard entries excluding it: removing its exact
 * entries cannot switch it on while one of them matches.
 */
export function modelState(id: string, list: string[]): { on: boolean; lockedBy: string[] } {
  const model = norm(id);
  const matching = list.filter((e) => wildcard(norm(e), model));
  return { on: matching.length === 0, lockedBy: matching.filter((e) => e.includes("*")) };
}

/** The list with `entry` added as the server stores it: trimmed, lowercased, once. */
export function addEntry(list: string[], entry: string): string[] {
  const e = norm(entry);
  return !e || list.some((x) => norm(x) === e) ? list : [...list, e];
}

/**
 * The list after switching `id` on or off. Off adds the model ID; on removes the entries
 * naming exactly that ID, and leaves wildcard entries to the caller.
 */
export function switchModel(id: string, list: string[], on: boolean): string[] {
  const model = norm(id);
  return on ? list.filter((e) => e.includes("*") || norm(e) !== model) : addEntry(list, model);
}
