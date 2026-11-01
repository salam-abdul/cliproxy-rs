<script lang="ts">
  import { store } from "./store.svelte";
  import { requestBuckets } from "./history";
  import { outcomeLabel, summarizeHistory } from "./traffic";
  import type { Data } from "./core";

  let { data }: { data: Data | undefined } = $props();
  const summary = $derived(summarizeHistory(data || {}));
  const buckets = $derived(requestBuckets(data || {}));
  const hasObserved = $derived(buckets.some((bucket) => bucket.known));
  const peak = $derived(
    Math.max(1, ...buckets.map((bucket) => bucket.requests)),
  );
  const mixTotal = $derived(
    summary.tokens.reduce((n, row) => n + (row.tokens ?? 0), 0),
  );
  const compact = (n: number | null) =>
    n === null
      ? "—"
      : Intl.NumberFormat(undefined, {
          notation: "compact",
          maximumFractionDigits: 1,
        }).format(n);
  const date = (n: number) =>
    new Date(n).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "numeric",
    });
</script>

<section class="section history-section">
  {#if store.history.error}
    <p class="overview-alert">
      History could not be loaded. Current account readings remain available.
    </p>
  {:else if !data}
    <p class="muted">Loading history…</p>
  {:else}
    <div class="history-grid">
      <section class="insight-card">
        <div class="section-head">
          <h3>Requests & failures</h3>
          <span class="muted small">Selected period</span>
        </div>
        <div class="history-totals">
          <strong>{compact(summary.requests)}</strong><span>requests</span>
          <strong class="history-failures">{summary.failures}</strong><span
            >failed</span
          >
        </div>
        {#if hasObserved}
          <div
            class="history-chart"
            role="img"
            aria-label={`${summary.requests} requests and ${summary.failures} failures in ${store.historyRange}`}
          >
            {#each buckets as bucket}
              <span
                class="history-column"
                class:unknown={!bucket.known}
                title={`${date(bucket.at)} · ${bucket.known ? `${bucket.requests} recorded requests · ${bucket.failures} failed` : "No observation"}`}
              >
                {#if bucket.known && bucket.requests > 0}<i
                    style:height={`${Math.max(3, (100 * bucket.requests) / peak)}%`}
                  >
                    {#if bucket.failures}<b
                        style:height={`${Math.max(4, (100 * bucket.failures) / bucket.requests)}%`}
                      ></b>{/if}
                  </i>{/if}
              </span>
            {/each}
          </div>
          <div class="history-axis">
            <span>{date(data.from)}</span><span>{date(data.to)}</span>
          </div>
        {:else}<div class="history-empty">
            The chart will fill as the proxy records activity and account
            status.
          </div>{/if}
        <div class="history-key muted small">
          <span><i class="key-requests"></i>Requests</span>
          <span><i class="key-failures"></i>Failures</span><span
            ><i class="key-unknown"></i>No observation</span
          >
        </div>
        {#if Array.isArray(data.errors) && data.errors.length}
          <div class="history-errors">
            {#each data.errors.slice(0, 3) as error}
              <span
                >{outcomeLabel(error)} <strong>{error.requests}</strong></span
              >
            {/each}
          </div>
        {/if}
      </section>
      <section class="insight-card">
        <div class="section-head">
          <h3>Tokens by category</h3>
          <span class="muted small">Reported counts</span>
        </div>
        {#if mixTotal > 0}
          <div class="history-mix" role="img" aria-label="Reported token mix">
            {#each summary.tokens as row, i}
              {#if row.tokens && row.tokens > 0}<span
                  class={`mix-${i}`}
                  style:width={`${(100 * row.tokens) / mixTotal}%`}
                  title={`${row.label}: ${compact(row.tokens)} tokens`}
                ></span>{/if}
            {/each}
          </div>
        {/if}
        {#each summary.tokens as row, i}
          <div class="history-token">
            <span><i class={`mix-${i}`}></i>{row.label}</span>
            <strong
              >{row.tokens !== null &&
              (row.reported < summary.requests || data.modelsTruncated)
                ? "≥"
                : ""}{compact(row.tokens)}</strong
            >
          </div>
        {/each}
        {#if summary.partialTokens}<p class="muted small">
            Some usage is omitted; ≥ values are lower bounds.
          </p>{/if}
      </section>
      <section class="insight-card top-models">
        <div class="section-head">
          <h3>Top models</h3>
          <span class="muted small">By requests · selected period</span>
        </div>
        {#each summary.models.slice(0, 5) as model}
          <div class="history-model">
            <code>{model.model || "Unknown model"}</code>
            <span class="meter-track"
              ><i
                style:width={`${summary.requests ? (100 * model.requests) / summary.requests : 0}%`}
              ></i></span
            >
            <strong>{compact(model.requests)}</strong>
          </div>
        {:else}<p class="muted small">
            Models appear after requests are recorded.
          </p>{/each}
      </section>
    </div>
    <p class="muted small history-coverage">
      {#if data.oldestRequest == null}No requests recorded yet.
      {:else if data.oldestRequest > data.from}Request records begin {date(
          data.oldestRequest,
        )}.
      {:else}Retained requests reach this period; offline gaps are unknown.{/if}
      {#if data.lostEvents}
        {data.lostEvents} records could not be saved.{/if}
    </p>
  {/if}
</section>

<style>
  .history-section .section-head {
    align-items: center;
  }
  .history-section h3 {
    margin: 0;
  }
  .history-coverage {
    margin: 0;
  }
  .history-grid {
    display: grid;
    grid-template-columns: 1.2fr 0.8fr;
    gap: 12px;
  }
  .insight-card {
    padding: 18px;
    border: 1px solid var(--line, #dce4ea);
    border-radius: 13px;
    background: var(--raised);
  }
  .insight-card .section-head {
    margin-bottom: 10px;
  }
  .history-totals {
    display: flex;
    align-items: baseline;
    gap: 6px;
    margin-bottom: 15px;
  }
  .history-totals strong {
    font-size: 23px;
    letter-spacing: -0.03em;
  }
  .history-totals span {
    font-size: 11px;
    color: var(--muted, #8494a0);
    margin-right: 12px;
  }
  .history-totals .history-failures {
    font-size: 17px;
    color: #c47765;
  }
  .history-chart {
    height: 142px;
    display: flex;
    align-items: end;
    gap: 3px;
    border-bottom: 1px solid var(--line, #dce4ea);
    background: repeating-linear-gradient(
      to top,
      transparent 0,
      transparent 34px,
      var(--line, #eaf0f3) 35px
    );
  }
  .history-column {
    flex: 1;
    height: 100%;
    display: flex;
    align-items: end;
    min-width: 2px;
  }
  .history-column.unknown {
    background: linear-gradient(to top, #b2bec744 0 3px, transparent 3px);
  }
  .history-column > i {
    display: block;
    width: 100%;
    background: #79a9cf;
    border-radius: 2px 2px 0 0;
    position: relative;
  }
  .history-column b {
    display: block;
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    background: #d98b7d;
  }
  .history-axis {
    display: flex;
    justify-content: space-between;
    color: var(--muted, #8494a0);
    font-size: 10px;
    margin-top: 5px;
  }
  .history-key {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    margin-top: 12px;
  }
  .history-key i,
  .history-token i {
    display: inline-block;
    width: 7px;
    height: 7px;
    border-radius: 2px;
    margin-right: 5px;
  }
  .key-requests {
    background: #79a9cf;
  }
  .key-failures {
    background: #d98b7d;
  }
  .key-unknown {
    background: #b2bec7;
  }
  .history-errors {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    margin-top: 9px;
  }
  .history-errors span {
    border: 1px solid var(--line, #dce4ea);
    border-radius: 6px;
    padding: 3px 6px;
    color: var(--muted, #8494a0);
    font-size: 10px;
  }
  .history-errors strong {
    color: #c47765;
    margin-left: 3px;
  }
  .history-empty {
    display: grid;
    place-items: center;
    height: 142px;
    color: var(--muted, #8494a0);
    font-size: 12px;
  }
  .history-mix {
    height: 7px;
    display: flex;
    overflow: hidden;
    border-radius: 8px;
    margin: 12px 0;
  }
  .mix-0 {
    background: #3f7faf;
  }
  .mix-1 {
    background: #52a48a;
  }
  .mix-2 {
    background: #9d82c1;
  }
  .mix-3 {
    background: #d9a65f;
  }
  .history-token {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 8px;
    padding: 7px 0;
    border-top: 1px solid var(--line, #eef2f5);
    font-size: 11px;
  }
  .history-token strong {
    text-align: right;
  }
  .history-model {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(65px, 0.8fr) 42px;
    align-items: center;
    gap: 10px;
    margin: 9px 0;
    font-size: 11px;
  }
  .history-model code {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .history-model .meter-track {
    height: 4px;
  }
  .history-model .meter-track i {
    display: block;
    height: 100%;
    background: #79a9cf;
  }
  .history-model strong {
    text-align: right;
  }
  .top-models {
    grid-column: 1/-1;
  }
  @media (max-width: 820px) {
    .history-grid {
      grid-template-columns: 1fr;
    }
    .top-models {
      grid-column: 1;
    }
  }
</style>
