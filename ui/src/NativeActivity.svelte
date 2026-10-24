<script lang="ts">
  import { store, Res, every } from "./store.svelte";
  import { api, text } from "./api";
  import { type Data } from "./core";
  const native = $derived(!!store.native.data?.enabled);
  const activity = new Res<Data>(() => api("/observability/native/events"));
  $effect(() => { if (native) { activity.load(); return every(5000, () => activity.load(true)); } });
  const events = $derived((activity.data?.events || []) as Data[]);
  let failuresOnly = $state(false);
  const failures = (e: Data) => ["rejected", "transport-failure"].includes(e.type) || e.type === "inference" && e.outcome !== "success";
  const visible = $derived(events.filter(e => failuresOnly ? failures(e) : ["inference", "rejected", "transport-failure", "failover", "selection"].includes(e.type)).slice(0, 50));
  const label = (v: unknown) => String(v || "unknown").replaceAll("_", " ");
  const ms = (v: unknown) => typeof v === "number" ? `${v.toLocaleString()} ms` : "—";
</script>

{#if native}
<section class="section">
  <div class="section-head"><h2>Native routing</h2><button class="key" aria-pressed={failuresOnly} onclick={() => failuresOnly = !failuresOnly}>{failuresOnly ? "Show all events" : "Show failures"}</button></div>
  <p class="note">Recent activity since startup. Reading this view preserves records for other consumers. Failover attempts and final request outcomes are shown separately.</p>
  {#if activity.error}<p class="note error">{text(activity.error)}</p>{/if}
  {#if activity.data?.lostEvents}<p class="note error">{activity.data.lostEvents} events could not be written to disk. Recent memory records may still be available.</p>{/if}
  {#if !visible.length}<p class="muted">{activity.data ? "No matching events." : "Loading…"}</p>{/if}
  {#each visible as e}
    <details class="native-event">
      <summary>
        <span class="lamp {failures(e) ? 'bad' : e.type === 'failover' ? 'warn' : 'ok'}"></span>
        <span class="legend">{new Date(e.at).toLocaleTimeString()}</span>
        <code>{e.model || "Claude"}</code>
        <span>{e.recovered ? "Recovered after failover" : label(e.outcome || e.reason || e.type)}</span>
        {#if e.status}<span class="legend">{e.status}</span>{/if}
      </summary>
      <dl>
        {#if e.account}<dt>Account</dt><dd><code>{e.account}</code></dd>{/if}
        {#if e.requestId}<dt>Request</dt><dd><code>{e.requestId}</code></dd>{/if}
        {#if e.upstreamStatus}<dt>Provider HTTP status</dt><dd>{e.upstreamStatus}</dd>{/if}
        {#if e.attempts}<dt>Attempts</dt><dd>{e.attempts}</dd>{/if}
        {#if e.failureKind}<dt>Transport failure</dt><dd>{label(e.failureKind)} · request not replayed</dd>{/if}
        {#if e.shadow}<dt>Shadow selection</dt><dd>Would choose {e.proposedAccount || "no account"}; active selection unchanged</dd>{/if}
        {#if e.retryAfterMs != null}<dt>Provider retry delay</dt><dd>{ms(e.retryAfterMs)}</dd>{/if}
        {#if e.timing}
          <dt>Preparation</dt><dd>{ms(e.timing.preparationMs)}</dd>
          <dt>Session wait</dt><dd>{ms(e.timing.sessionWaitMs)}</dd>
          <dt>Quota admission</dt><dd>{ms(e.timing.admissionMs)}</dd>
          <dt>Upstream headers, all attempts</dt><dd>{ms(e.timing.upstreamHeadersMs)}</dd>
          <dt>First response byte, from arrival</dt><dd>{ms(e.timing.firstByteMs)}</dd>
          <dt>Response stream</dt><dd>{ms(e.timing.responseStreamMs)}</dd>
          <dt>Total</dt><dd>{ms(e.timing.totalMs)}</dd>
        {/if}
      </dl>
    </details>
  {/each}
</section>
{/if}
<style>
  .native-event { border-bottom: 1px solid var(--line, #7774); padding: .65rem 0; }
  summary { cursor: pointer; display: flex; gap: .75rem; flex-wrap: wrap; align-items: center; }
  dl { display: grid; grid-template-columns: minmax(10rem, 1fr) 2fr; gap: .35rem 1rem; margin: 1rem; }
  dd { margin: 0; overflow-wrap: anywhere; }
</style>
