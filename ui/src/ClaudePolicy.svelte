<script lang="ts">
  import { api, text } from "./api";
  import { store } from "./store.svelte";

  let saving = $state(false);
  const known = $derived(
    typeof store.native.data?.allowExtraUsage === "boolean" &&
      !store.native.error &&
      !(store.native.data?.validUntil <= store.now),
  );

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
    <h3>Claude spending</h3>
    <div class="row">
      {#if known}<button
          class="key"
          role="switch"
          aria-checked={!!store.native.data.allowExtraUsage}
          disabled={saving ||
            !!store.native.error ||
            store.native.data.validUntil <= store.now}
          onclick={toggle}
          ><span class="switch-track" aria-hidden="true"><span></span></span
          >Allow paid overage: {saving
            ? "Saving…"
            : store.native.data.allowExtraUsage
              ? "On"
              : "Off"}</button
        >{:else}<span class="muted"
          >Paid overage policy unavailable · waiting for status</span
        >{/if}
      <span class="note"
        >Applies to all enabled Claude accounts. Included quota is used first.
        When off, wait for a reset rather than use paid extra usage. This does
        not change provider billing settings or set a spending cap.</span
      >
    </div>
    {#if store.native.error}<p class="note error">
        Could not refresh the Claude routing policy.
      </p>{/if}
  </section>
{/if}
