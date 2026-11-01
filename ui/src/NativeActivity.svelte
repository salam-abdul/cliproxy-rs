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
  const failures = isFailure;
  const visible = $derived(
    events
      .filter(
        (e) =>
          (filter === "all" ||
            (filter === "failed" ? failures(e) : switchedAccounts(e))) &&
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
      <span class="muted small"
        >{visible.length} of {events.length} recent requests</span
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
              class="lamp {failures(e)
                ? 'bad'
                : e.outcome !== 'success'
                  ? 'warn'
                  : 'ok'}"
            ></span><span>{outcomeLabel(e)}</span>{#if e.status}<span
                class="legend">{e.status}</span
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
  .request-toolbar {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
  }
  .request-toolbar > span {
    margin-left: auto;
  }
  .section {
    gap: 0;
  }
  .request-toolbar {
    margin-bottom: 12px;
  }
  .native-event {
    border-bottom: 1px solid var(--line);
  }
  summary {
    cursor: pointer;
    display: grid;
    grid-template-columns:
      70px minmax(0, 1.2fr) minmax(70px, 0.6fr) minmax(0, 1.4fr)
      10px;
    gap: 12px;
    align-items: center;
    padding: 10px 0;
    font-size: 12px;
  }
  summary::after {
    content: "›";
    color: var(--ink-2);
    font-size: 18px;
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
    color: var(--ink-2);
  }
  .request-result {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .request-result .lamp {
    width: 7px;
    height: 7px;
  }
  dl {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 2fr);
    gap: 0.35rem 1rem;
    padding: 12px;
    margin: 12px 0;
    background: var(--panel);
    border-radius: 8px;
    font-size: 12px;
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  @media (max-width: 600px) {
    summary {
      grid-template-columns: 60px minmax(0, 1fr) 10px;
      gap: 6px 10px;
    }
    summary::after {
      grid-column: 3;
      grid-row: 1 / 3;
    }
    .request-account {
      grid-column: 1;
    }
    .request-result {
      grid-column: 2;
    }
  }
</style>
