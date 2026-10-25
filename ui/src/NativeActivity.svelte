<script lang="ts">
  import { store, Res, every } from "./store.svelte";
  import { api, text } from "./api";
  import { requestOutcomes } from "./accounts";
  import { type Data } from "./core";
  const native = $derived(!!store.native.data?.enabled);
  const activity = new Res<Data>(() => api("/observability/native/events"));
  $effect(() => {
    if (native) {
      activity.load();
      return every(5000, () => activity.load(true));
    }
  });
  const events = $derived(
    requestOutcomes((activity.data?.events || []) as Data[]),
  );
  let failuresOnly = $state(false);
  let account = $state("");
  const failures = (e: Data) =>
    !["success", "pending", "client_cancelled"].includes(e.outcome);
  const visible = $derived(
    events
      .filter(
        (e) =>
          (!failuresOnly || failures(e)) &&
          (!account ||
            e.events.some((attempt: Data) => attempt.account === account)),
      )
      .slice(0, 50),
  );
  const label = (v: unknown) => String(v || "unknown").replaceAll("_", " ");
  const ms = (v: unknown) =>
    typeof v === "number" ? `${v.toLocaleString()} ms` : "—";
</script>

{#if native}
  <section class="section">
    <div class="section-head">
      <h2>Claude requests</h2>
      <button
        class="key"
        aria-pressed={failuresOnly}
        onclick={() => (failuresOnly = !failuresOnly)}
        >{failuresOnly ? "Show all requests" : "Show failures"}</button
      >
    </div>
    <label class="field model-choice"
      >Account<select bind:value={account}
        ><option value="">All accounts</option
        >{#each store.native.data?.accounts || [] as a}<option value={a.id}
            >{a.name}</option
          >{/each}</select
      ></label
    >
    <p class="note">
      Recent requests since startup. Each request groups its failover attempts.
      Pending means no final outcome is recorded in this history.
    </p>
    {#if activity.error}<p class="note error">{text(activity.error)}</p>{/if}
    {#if activity.data?.lostEvents}<p class="note error">
        {activity.data.lostEvents} events could not be written to disk. Recent memory
        records may still be available.
      </p>{/if}
    {#if !visible.length}<p class="muted">
        {activity.data ? "No matching events." : "Loading…"}
      </p>{/if}
    {#each visible as e}
      <details class="native-event">
        <summary>
          <span
            class="lamp {failures(e)
              ? 'bad'
              : e.outcome !== 'success'
                ? 'warn'
                : 'ok'}"
          ></span>
          <span class="legend">{new Date(e.at).toLocaleTimeString()}</span>
          <code>{e.model || "Claude"}</code>
          <span
            >{e.recovered
              ? "Recovered after failover"
              : label(e.outcome || e.reason || e.type)}</span
          >
          {#if e.status}<span class="legend">{e.status}</span>{/if}
        </summary>
        <p>{e.events.length} recorded events</p>
        {#each e.events.filter((event: Data) => event.type === "failover") as attempt}<p
            class="muted"
          >
            Retry · {store.native.data?.accounts?.find(
              (a: Data) => a.id === attempt.account,
            )?.name || attempt.account} · HTTP {attempt.status ||
              attempt.upstreamStatus ||
              "unknown"}
          </p>{/each}
        <dl>
          {#if e.account}<dt>Account</dt>
            <dd>
              {store.native.data?.accounts?.find(
                (a: Data) => a.id === e.account,
              )?.name || e.account}
            </dd>{/if}
          {#if e.requestId}<dt>Request</dt>
            <dd><code>{e.requestId}</code></dd>{/if}
          {#if e.upstreamStatus}<dt>Provider HTTP status</dt>
            <dd>{e.upstreamStatus}</dd>{/if}
          {#if e.attempts}<dt>Attempts</dt>
            <dd>{e.attempts}</dd>{/if}
          {#if e.failureKind}<dt>Transport failure</dt>
            <dd>{label(e.failureKind)} · request not replayed</dd>{/if}
          {#if e.shadow}<dt>Shadow selection</dt>
            <dd>
              Would choose {e.proposedAccount || "no account"}; active selection
              unchanged
            </dd>{/if}
          {#if e.retryAfterMs != null}<dt>Provider retry delay</dt>
            <dd>{ms(e.retryAfterMs)}</dd>{/if}
          {#if e.timing}
            <dt>Preparation</dt>
            <dd>{ms(e.timing.preparationMs)}</dd>
            <dt>Session wait</dt>
            <dd>{ms(e.timing.sessionWaitMs)}</dd>
            <dt>Quota admission</dt>
            <dd>{ms(e.timing.admissionMs)}</dd>
            <dt>Upstream headers, all attempts</dt>
            <dd>{ms(e.timing.upstreamHeadersMs)}</dd>
            <dt>First response byte, from arrival</dt>
            <dd>{ms(e.timing.firstByteMs)}</dd>
            <dt>Response stream</dt>
            <dd>{ms(e.timing.responseStreamMs)}</dd>
            <dt>Total</dt>
            <dd>{ms(e.timing.totalMs)}</dd>
          {/if}
        </dl>
      </details>
    {/each}
  </section>
{:else}<p class="muted">
    Native request history is not available on this server.
  </p>{/if}

<style>
  .native-event {
    border-bottom: 1px solid var(--line, #7774);
    padding: 0.65rem 0;
  }
  summary {
    cursor: pointer;
    display: flex;
    gap: 0.75rem;
    flex-wrap: wrap;
    align-items: center;
  }
  dl {
    display: grid;
    grid-template-columns: minmax(10rem, 1fr) 2fr;
    gap: 0.35rem 1rem;
    margin: 1rem;
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
</style>
