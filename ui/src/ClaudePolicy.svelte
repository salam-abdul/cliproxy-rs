<script lang="ts">
  import { store } from "./store.svelte";

  const known = $derived(
    typeof store.native.data?.allowExtraUsage === "boolean" &&
      !store.native.error &&
      !(store.native.data?.validUntil <= store.now),
  );
  const on = $derived(!!store.native.data?.allowExtraUsage);
  const toggle = () =>
    store.call(
      "PATCH",
      "/routing/native-policy",
      { allowExtraUsage: !on },
      on ? "Paid overage off." : "Paid overage on.",
      on ? "" : "Allow paid overage? Requests may be billed after included quota runs out.",
      () => store.native.load(true),
    );
</script>

{#if store.native.data?.enabled}
  <section class="section" aria-label="Claude routing">
    <h3>Claude spending</h3>
    <div class="row">
      {#if known}<button
          class="key"
          role="switch"
          aria-checked={on}
          disabled={store.busy}
          onclick={toggle}
          ><span class="switch-track" aria-hidden="true"><span></span></span
          >Allow paid overage: {on ? "On" : "Off"}</button
        >{:else}<span class="muted"
          >Paid overage policy unavailable · waiting for status</span
        >{/if}
      <span class="note"
        >Use paid extra usage after included quota runs out. Account billing
        settings stay unchanged.</span
      >
    </div>
    {#if store.native.error}<p class="note error">
        Could not refresh the Claude routing policy.
      </p>{/if}
  </section>
{/if}
