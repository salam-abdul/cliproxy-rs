<script lang="ts">
  import { api, text } from "./api";
  import { every, store } from "./store.svelte";

  let saving = $state(false);
  $effect(() => every(15_000, async () => {
    if (!saving) await store.native.load(true);
  }));

  async function toggle() {
    if (!store.native.data || saving) return;
    const allowExtraUsage = !store.native.data.allowExtraUsage;
    saving = true;
    try {
      await api("/routing/native-policy", "PATCH", { allowExtraUsage });
      await store.native.load(true);
    } catch (e) {
      store.notify(text(e), true);
    } finally {
      saving = false;
    }
  }
</script>

{#if store.native.data?.enabled}
  <section class="section" aria-label="Claude routing">
    <h2>Claude routing</h2>
    <div class="row">
      <button class="key" role="switch" aria-checked={!!store.native.data.allowExtraUsage} disabled={saving || !!store.native.error} onclick={toggle}>Allow paid overage: {store.native.data.allowExtraUsage ? "On" : "Off"}</button>
      <span class="note">Use included quota across enabled Claude accounts first. When off, wait for a reset once all included quota is exhausted.</span>
    </div>
    {#if store.native.error}<p class="note error">Could not refresh the Claude routing policy.</p>{/if}
  </section>
{/if}
