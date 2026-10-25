<script lang="ts">
  import { store } from "./store.svelte";
  import { limits, check, canCheck, hasSource, quotaKey } from "./quota";
  import { ago, type Data } from "./core";
  import Meter from "./Meter.svelte";
  let { a }: { a: Data } = $props();
  const q = $derived(limits(a));
  const pending = $derived(!!store.quotaPending[quotaKey(a)]);
</script>

<div class="stack" aria-busy={pending}>
  <div class="row">
    <h3 class="grow">Quota</h3>
    <button
      class="key small"
      disabled={pending || !canCheck(a)}
      onclick={() => check(a)}>{pending ? "Checking…" : "Check quota"}</button
    >
  </div>
  {#if q && "windows" in q}
    {#each q.windows as w}<Meter {w} now={store.now} />{:else}<p class="muted">
        The provider reported no usage windows.
      </p>{/each}
    <p class="legend">
      Reading received {ago(q.at, store.now)}{store.now - q.at > 90_000
        ? " · may be out of date"
        : ""}.
    </p>
  {:else}<p class="muted">
      {pending
        ? "Checking quota…"
        : hasSource(a)
          ? "Quota not checked yet."
          : "This provider does not report quota."}
    </p>{/if}
  {#if q?.error}<p class="error" role="status">
      {q.error} Any displayed reading is from the last successful check.
    </p>{/if}
</div>
