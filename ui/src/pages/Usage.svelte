<script lang="ts">
  import { store, every } from "../store.svelte";
  import { credName } from "../core";
  $effect(() => every(15000, () => store.creds.load(true), true));
</script>

<p class="muted">
  Account request totals reported by the server. Use OpenUsage for usage
  analysis.
</p>
<ul class="list">
  {#each store.creds.data || [] as a}<li class="item account-summary">
      <strong class="grow">{credName(a)}</strong><span
        >{a.success === undefined
          ? "Not reported"
          : `${a.success} succeeded · ${a.failed || 0} failed`}</span
      >
    </li>{/each}
</ul>
