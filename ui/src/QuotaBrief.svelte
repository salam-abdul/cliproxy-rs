<script lang="ts">
  import { store } from "./store.svelte";
  import { quotaSummary } from "./quota";
  import { ago, span, type Data } from "./core";
  let { a, compact = false }: { a: Data; compact?: boolean } = $props();
  const quota = $derived(quotaSummary(a));
  const windows = $derived(compact ? quota.windows.slice(0, 2) : quota.windows);
</script>

<span class="quota-meters" class:quota-compact={compact} class:quota-stale={quota.failed || quota.stale}>
  {#each windows as w}
    {@const remaining = w.used === null ? null : Math.max(0, 100 - w.used)}
    {@const resetsAt = Date.parse(w.reset)}
    <span class="quota-meter">
      <span class="quota-caption"
        ><span>{w.label}</span><strong class="num"
          >{remaining === null
            ? "Unknown"
            : `${Number(remaining.toFixed(1))}% left`}</strong
        ></span
      >
      {#if remaining !== null}<span
          class="meter-track"
          role="meter"
          aria-label={`${w.label} remaining`}
          aria-valuemin="0"
          aria-valuemax="100"
          aria-valuenow={remaining}
        >
          <span
            style:width={`${remaining}%`}
            class:low={remaining <= 20}
            class:empty={remaining === 0}
          ></span>
        </span>{/if}
      {#if w.reset && Number.isFinite(resetsAt)}<time
          class="muted"
          datetime={w.reset}
          title={new Date(w.reset).toLocaleString()}
        >
          {resetsAt > store.now
            ? `Resets in ${span(resetsAt - store.now)}`
            : "Reset passed · awaiting update"}
        </time>{/if}
      {#if w.detail}<span class="muted">{w.detail}</span>{/if}
    </span>
  {:else}<span class="muted quota-unreported">Quota not reported</span>{/each}
  {#if quota.failed}<span class="warning-text">Last reading · check failed</span>
  {:else if quota.at}<span class:warning-text={quota.stale} class:muted={!quota.stale}
      >Updated {ago(quota.at, store.now)}</span
    >
  {:else if quota.stale}<span class="warning-text">Last reading · needs refresh</span>{/if}
</span>
