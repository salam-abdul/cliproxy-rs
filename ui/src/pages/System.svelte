<script lang="ts">
  import { store } from "../store.svelte";
  import { endpoint } from "../api";
  import { readPath } from "../core";

  const c = $derived(store.config.data || {});
  const facts = $derived(
    [
      ["Management API", new URL(endpoint()).pathname],
      ["Server", store.meta.version || "Not reported"],
      ["Commit", store.meta.commit !== "none" && store.meta.commit],
      ["Built", !isNaN(Date.parse(store.meta.built)) && new Date(store.meta.built).toLocaleString()],
      ["Config layout", c["config-version"] ? `v${c["config-version"]}` : "Legacy (migrated on save)"],
      ["Routing strategy", readPath(c, "routing/strategy", "Server default")],
      ["Auth directory", readPath(c, "oauth/auth-dir", "Server default")],
      // The config cannot show the effective policy: MANAGEMENT_PASSWORD allows remote access too.
      ["Remote management", readPath(c, "management/allow-remote", false) ? "Allowed" : "Local only, unless MANAGEMENT_PASSWORD is set"],
      ["Dashboard", "Built into the server"],
    ].filter((f) => f[1]),
  );
</script>

<div class="head"><h1>System</h1></div>

<section class="section">
  <dl class="facts">
    {#each facts as [k, v]}<div><dt>{k}</dt><dd class:mono={k === "Management API" || k === "Commit"}>{v}</dd></div>{/each}
  </dl>
  <p class="note">
    The management API does not expose process uptime, CPU or memory, so they are not shown here.
  </p>
</section>

<section class="section"><h2>Updates</h2><p>Build this personal fork from source. Updates are installed manually after validation.</p></section>
