<script lang="ts">
  import QuotaBrief from "../QuotaBrief.svelte";
  import { store, every } from "../store.svelte";
  import { accountStatus } from "../accounts";
  import { credName, ago, type Data } from "../core";
  $effect(() => every(10000, () => store.creds.load(true)));
  const native = $derived(store.native.data);
  const fresh = $derived(
    !store.native.error &&
      native?.model === store.model &&
      native?.validUntil > store.now,
  );
  const recent = $derived((native?.recent || []) as Data[]);
  const success = $derived(recent.find((e) => e.outcome === "success"));
</script>

<div class="head">
  <h1>Overview</h1>
  <a class="key" href="#use">Set up a client</a>
</div>
<section class="section stack">
  <h2>Connection</h2>
  <p>
    <span class="lamp {store.online ? 'ok' : 'bad'}"></span> Management {store.online
      ? "connected"
      : "unreachable"}
  </p>
  <p>
    <span class="lamp {fresh && native?.ready ? 'ok' : 'warn'}"></span> Claude
    routing: {native?.enabled === false
      ? "not enabled"
      : !fresh
        ? "availability unknown"
        : native?.ready
          ? "available"
          : "no account available"} for <code>{store.model}</code>
  </p>
  <p class="muted">
    {success
      ? `Last successful routed request ${ago(success.at, store.now)}.`
      : "No successful routed request in the recent status history."} This does not
    confirm which client is connected.
  </p>
  <a href="#activity">View request activity →</a>
</section>
<section class="section">
  <div class="section-head">
    <h2>Accounts</h2>
    <a class="key" href="#credentials">Manage accounts and quotas</a>
  </div>
  {#if store.creds.error}<p class="error">
      Could not refresh accounts. Displayed accounts may be out of date.
    </p>{/if}
  <ul class="list">
    {#each store.creds.data || [] as a}
      {@const s = accountStatus(
        a,
        native,
        store.native.error,
        store.model,
        store.now,
      )}
      <li>
        <a
          class="item account-summary"
          href={`#credentials/${encodeURIComponent(a.id || a.name)}`}
          ><span class="lamp {s.lamp}"></span><span class="grow"
            ><strong>{credName(a)}</strong><QuotaBrief {a} /></span
          ><span>{s.label}</span></a
        >
      </li>
    {:else}<li class="state">
        <p>
          {store.creds.loading
            ? "Loading accounts…"
            : "Connect an account to get started."}
        </p>
        <a class="key primary" href="#connect">Connect account</a>
      </li>{/each}
  </ul>
</section>
