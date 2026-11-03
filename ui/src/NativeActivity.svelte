<script lang="ts">
  import { store, every } from "./store.svelte";
  import { text } from "./api";
  import { isFailure, outcomeLabel } from "./traffic";
  import { requestOutcomes, switchedAccounts } from "./accounts";
  import { type Data } from "./core";
  const native = $derived(!!store.native.data?.enabled);
  const activity = store.activity;
  $effect(() => {
    if (native) {
      activity.load();
      return every(5000, () => activity.load(true));
    }
  });
  const events = $derived(
    requestOutcomes((activity.data?.events || []) as Data[]),
  );
  let filter = $state("all");
  let account = $state("");
  const visible = $derived(
    events
      .filter(
        (e) =>
          (filter === "all" ||
            (filter === "failed" ? isFailure(e) : switchedAccounts(e))) &&
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
      <h2>Native Claude routing</h2>
      <span class="legend"
        >{visible.length} of {events.length} recent requests</span
      >
    </div>
    <div class="request-toolbar">
      <div class="seg" role="group" aria-label="Request outcome">
        {#each [["all", "All"], ["failed", "Failed"], ["switched", "Account switches"]] as [value, name]}
          <button
            aria-pressed={filter === value}
            onclick={() => (filter = value)}>{name}</button
          >
        {/each}
      </div>
      <label
        ><span class="sr">Account</span><select bind:value={account}
          ><option value="">All accounts</option
          >{#each store.native.data?.accounts || [] as a}<option value={a.id}
              >{a.name}</option
            >{/each}</select
        ></label
      >
    </div>
    {#if activity.error}<p class="note error">{text(activity.error)}</p>{/if}
    {#if activity.data?.lostEvents}<p class="note error">
        {activity.data.lostEvents} events could not be written to disk. Recent memory
        records may still be available.
      </p>{/if}
    {#if !visible.length}<p class="muted">
        {activity.data ? "No matching events." : "Loading…"}
      </p>{/if}
    {#if visible.length}<div class="request-columns eyebrow" aria-hidden="true">
        <span>Time</span><span>Model</span><span>Account</span><span>Outcome</span>
      </div>{/if}
    {#each visible as e}
      <details class="native-event">
        <summary>
          <time class="legend" title={new Date(e.at).toLocaleString()}
            >{new Date(e.at).toLocaleTimeString([], {
              hour: "2-digit",
              minute: "2-digit",
            })}</time
          >
          <code>{e.model || "Model not reported"}</code>
          <span class="request-account"
            >{store.native.data?.accounts?.find((a: Data) => a.id === e.account)
              ?.name || "—"}</span
          >
          <span
            class="request-result"
            title={e.outcome === "pending"
              ? "No final outcome recorded in this history"
              : undefined}
            ><span
              class="lamp {isFailure(e)
                ? 'bad'
                : e.outcome !== 'success'
                  ? 'warn'
                  : 'ok'}"
            ></span><span>{outcomeLabel(e)}</span>{#if e.status}<span
                class="legend">HTTP {e.status}</span
              >{/if}</span
          >
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
  .section {
    gap: 0;
  }
  .request-toolbar {
    display: flex;
    align-items: center;
    gap: var(--s3);
    flex-wrap: wrap;
    margin-top: var(--s3);
  }
  .native-event {
    border-bottom: 1px solid var(--line);
  }
  summary,
  .request-columns {
    display: grid;
    grid-template-columns: 72px repeat(3, minmax(0, 1fr)) 12px;
    gap: var(--s3);
    align-items: center;
    padding: var(--s3) 0;
    font-size: var(--t-xs);
  }
  .request-columns {
    border-bottom: 1px solid var(--line);
  }
  summary::after {
    content: "›";
    color: var(--ink-muted);
    font-size: var(--t-lg);
  }
  details[open] > summary::after {
    transform: rotate(90deg);
  }
  summary code {
    font: inherit;
    overflow-wrap: anywhere;
  }
  .request-account {
    overflow-wrap: anywhere;
    color: var(--ink-muted);
  }
  .request-result {
    display: flex;
    align-items: center;
    gap: var(--s2);
    flex-wrap: wrap;
  }
  .request-result .lamp {
    width: 8px;
    height: 8px;
  }
  dl {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 2fr);
    gap: var(--s1) var(--s4);
    padding: var(--s3);
    margin: var(--s3) 0;
    background: var(--sunken);
    border-radius: var(--r);
    font-size: var(--t-xs);
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  @media (max-width: 600px) {
    .request-columns {
      display: none;
    }
    summary {
      grid-template-columns: 60px minmax(0, 1fr) 12px;
      gap: var(--s1) var(--s3);
    }
    summary::after {
      grid-column: 3;
      grid-row: 1 / 4;
    }
    .request-result {
      grid-area: 2 / 2;
    }
    .request-account {
      grid-area: 3 / 2;
    }
  }
</style>
