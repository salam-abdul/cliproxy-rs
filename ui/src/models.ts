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

const norm = (s: string) => s.trim().toLowerCase();

/**
 * Whether `id` is excluded, and by which entries. `locked` lists the wildcard entries:
 * removing the exact entries cannot enable the model while one of them matches.
 */
export function exclusion(id: string, entries: string[]): { excluded: boolean; locked: string[] } {
  const model = norm(id);
  const matching = entries.filter((e) => wildcard(norm(e), model));
  return { excluded: matching.length > 0, locked: matching.filter((e) => e.includes("*")) };
}

/**
 * The entries after switching `id` on or off. Off appends the model ID; on removes the
 * entries naming exactly that ID, and leaves wildcard entries to the caller.
 */
export function switchModel(id: string, entries: string[], enabled: boolean): string[] {
  const model = norm(id);
  if (enabled) return entries.filter((e) => e.includes("*") || norm(e) !== model);
  return exclusion(id, entries).excluded ? entries : [...entries, model];
}
