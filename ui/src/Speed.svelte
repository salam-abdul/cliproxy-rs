<script lang="ts" module>
  const rate = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
  const partNames = {
    server: "In the server",
    earlier: "Earlier sends",
    first: "First response",
    generation: "Generation",
  } as const;
</script>

<script lang="ts">
  import { ApiError } from "./api";
  import { store } from "./store.svelte";
  import { duration, partBars, slowLines, speedRows } from "./speed";
  import { label, type Data } from "./core";

  let { data }: { data: Data | undefined } = $props();
  const bars = $derived(data ? partBars(data) : []);
  const rows = $derived(data ? speedRows(data) : []);
  const slow = $derived(data ? slowLines(data, label, store.creds.data || []) : []);
  const requests = $derived(new Map(((data?.parts as Data[]) || []).map((p) => [`${p.provider}/${p.source}`, p.requests])));
  const error = $derived(store.speed.error);
  const speed = (n: number | null) => (n === null ? "—" : rate.format(n));
</script>

<section class="section card speed" aria-labelledby="speed-title">
  <div class="section-head">
    <h2 id="speed-title">Speed</h2>
    <span class="muted small">Completed requests · selected period</span>
  </div>
  {#if error && !data}
    <p class="alert">
      {error instanceof ApiError && error.status === 409
        ? "The activity store is not running."
        : "Speed could not be loaded."}
    </p>
  {:else if !data}
    <p class="muted">Loading speed…</p>
  {:else if !rows.length}
    <p class="muted">No completed requests recorded in this period.</p>
  {:else}
    <div class="speed-legend" aria-hidden="true">
      {#each Object.entries(partNames) as [part, name]}<span><i class="part-{part}"></i>{name}</span>{/each}
    </div>
    <div class="speed-bars">
      {#each bars as bar (`${bar.provider}/${bar.source}`)}
        <div class="speed-bar">
          <span class="speed-name"
            >{label(bar.provider)} ({bar.source})<small
              >{duration(bar.total)} mean · {requests.get(`${bar.provider}/${bar.source}`)} requests</small
            ></span
          >
          <div class="speed-track">
            <div class="speed-fill" style:width="{bar.width * 100}%">
              {#each bar.segments as segment}<span class="part-{segment.part}" style:flex-grow={segment.share}
                  ><span class="sr">{partNames[segment.part]} {duration(segment.ms)}</span></span
                >{/each}
            </div>
          </div>
        </div>
      {/each}
    </div>
    <div class="speed-scroll">
      <table class="table">
        <thead>
          <tr>
            <th>Model</th>
            <th class="count">Requests</th>
            <th class="count">First response p50</th>
            <th class="count">First response p95</th>
            <th class="count">Total p50</th>
            <th class="count">Total p95</th>
            <th class="count">Tokens per second</th>
            <th class="count">Decode speed</th>
          </tr>
        </thead>
        <tbody>
          {#each rows as row}
            <tr>
              <th scope="row"
                >{row.model ?? "Unknown model"}<small
                  >{label(row.provider)} ({row.source}){#if row.firstEvent}
                    · <span>first event</span>{/if}</small
                ></th
              >
              <td class="count num">{row.requests}</td>
              <td class="count num">{duration(row.firstP50)}</td>
              <td class="count num">{duration(row.firstP95)}</td>
              <td class="count num">{duration(row.totalP50)}</td>
              <td class="count num">{duration(row.totalP95)}</td>
              <td class="count num">{speed(row.tokensPerSecond)}</td>
              <td class="count num">{speed(row.decodeSpeed)}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    <h3 class="speed-slow-title">Slow slices</h3>
    {#if slow.length}
      <ul class="speed-slow">
        {#each slow as line}<li>{line}</li>{/each}
      </ul>
    {:else}
      <p class="muted">No slice is slow against the rest.</p>
    {/if}
    <p class="muted small">
      {#if data.untimed > 0}{data.untimed} of {data.requests} completed requests have no timing and are left out.{/if}
      {#if data.lostEvents > 0}{data.lostEvents} events could not be saved.{/if}
      {#if data.coverageUncertain}Coverage is uncertain for this period.{/if}
      First event marks a first response timed at the first translated event rather than the first byte.
    </p>
  {/if}
</section>

<style>
  .speed-legend {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s2) var(--s4);
    font-size: var(--t-xs);
    color: var(--ink-muted);
  }
  .speed-legend i {
    display: inline-block;
    width: 10px;
    height: 10px;
    margin-right: var(--s1);
    border-radius: 2px;
  }
  .speed-bars {
    display: grid;
    gap: var(--s2);
    margin: var(--s3) 0 var(--s4);
  }
  .speed-bar {
    display: grid;
    grid-template-columns: minmax(0, 12rem) minmax(0, 1fr);
    gap: var(--s3);
    align-items: center;
  }
  .speed-name small,
  .speed-scroll small {
    display: block;
    color: var(--ink-muted);
    font-size: var(--t-xs);
    font-weight: 400;
  }
  .speed-track {
    height: 12px;
    background: var(--sunken);
    border-radius: 2px;
  }
  .speed-fill {
    display: flex;
    height: 100%;
    overflow: hidden;
    border-radius: 2px;
  }
  .speed-fill > span {
    flex-basis: 0;
  }
  .part-server {
    background: var(--mix-1);
  }
  .part-earlier {
    background: var(--mix-4);
  }
  .part-first {
    background: var(--mix-2);
  }
  .part-generation {
    background: var(--mix-3);
  }
  .speed-scroll {
    overflow-x: auto;
  }
  .speed-scroll table {
    min-width: 820px;
  }
  .speed-scroll :is(th, td) {
    white-space: nowrap;
    overflow-wrap: normal;
    vertical-align: top;
  }
  .speed-scroll tbody th {
    color: inherit;
    font-size: inherit;
  }
  .speed-slow-title {
    margin: var(--s4) 0 var(--s2);
    font-size: var(--t);
  }
  .speed-slow {
    margin: 0;
    padding-left: var(--s4);
  }
</style>
