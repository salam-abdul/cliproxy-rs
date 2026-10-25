<script lang="ts">
  import { store } from "./store.svelte";
  import { limits } from "./quota";
  import {
    provider,
    span,
    claudeWindows,
    type Data,
    type Window,
  } from "./core";
  let { a }: { a: Data } = $props();
  const q = $derived(limits(a));
  const native = $derived(
    store.native.data?.accounts?.find(
      (n: Data) => n.fileId === (a.id || a.name),
    )?.includedQuota,
  );
  const windows: Window[] = $derived(
    provider(a) === "claude" && native?.limits
      ? claudeWindows(native.limits)
      : q && "windows" in q
        ? q.windows
        : [],
  );
  const stale = $derived(
    provider(a) === "claude" && native
      ? store.native.data?.validUntil <= store.now
      : q && "at" in q && store.now - q.at > 90000,
  );
</script>

{#if windows.length}<span class="quota-brief">
    {#each windows as w}<span
        >{w.label}: {typeof w.used === "number"
          ? `${w.used}% used`
          : "unknown"}{#if w.reset}
          · {Date.parse(w.reset) > store.now
            ? `resets in ${span(Date.parse(w.reset) - store.now)}`
            : "reset time passed"}<time datetime={w.reset}>
            · {new Date(w.reset).toLocaleString()}</time
          >{/if}</span
      >{/each}
    {#if store.native.error || q?.error}<span
        >Last reading · refresh failed</span
      >{:else if stale}<span>Last reading · may be out of date</span>{/if}
  </span>{/if}
