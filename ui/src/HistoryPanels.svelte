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

<section class="section">
  {#if store.history.error}
    <p class="alert">
      History could not be loaded. Current account readings remain available.
    </p>
  {:else if !data}
    <p class="muted">Loading history…</p>
  {:else}
    <div class="history-grid">
      <section class="card">
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
          <div class="history-axis legend">
            <span>{date(data.from)}</span><span>{date(data.to)}</span>
          </div>
        {:else}<div class="history-empty muted small">
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
          <div class="history-errors legend">
            {#each data.errors.slice(0, 3) as error}
              <span
                >{outcomeLabel(error)} <strong>{error.requests}</strong></span
              >
            {/each}
          </div>
        {/if}
      </section>
      <section class="card">
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
      <section class="card top-models">
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
    <p class="muted small">
      {#if data.oldestRequest == null}No requests recorded yet.
      {:else if data.oldestRequest > data.from}Request records begin {date(
          data.oldestRequest,
        )}.
      {:else}Retained requests reach this period; offline gaps are unknown.{/if}
      {#if data.lostEvents}
        {data.lostEvents} records could not be saved since history began.{/if}
      {#if data.coverageUncertain}A previous unclean shutdown may have left gaps.{/if}
    </p>
  {/if}
</section>

<style>
  .history-grid {
    display: grid;
    grid-template-columns: 1.2fr 0.8fr;
    gap: var(--s3);
  }
  .card .section-head {
    align-items: center;
    margin-bottom: var(--s3);
  }
  .history-totals {
    display: flex;
    align-items: baseline;
    gap: var(--s2);
    margin-bottom: var(--s4);
  }
  .history-totals strong {
    font-size: var(--t-xl);
    letter-spacing: -0.03em;
  }
  .history-totals span {
    font-size: var(--t-xs);
    color: var(--ink-muted);
    margin-right: var(--s3);
  }
  .history-totals .history-failures,
  .history-errors strong {
    color: var(--bad-ink);
  }
  .history-chart,
  .history-empty {
    height: 144px;
  }
  .history-chart {
    display: flex;
    align-items: end;
    gap: 3px;
    border-bottom: 1px solid var(--line);
    background: repeating-linear-gradient(to top, transparent 0 35px, var(--line) 35px 36px);
  }
  .history-column {
    flex: 1;
    height: 100%;
    display: flex;
    align-items: end;
    min-width: 2px;
  }
  .history-column.unknown {
    background: linear-gradient(to top, var(--series-unknown) 0 3px, transparent 3px);
  }
  .history-column > i {
    display: block;
    width: 100%;
    background: var(--series-1);
    border-radius: 2px 2px 0 0;
    position: relative;
  }
  .history-column b {
    display: block;
    position: absolute;
    inset: auto 0 0;
    background: var(--series-fail);
  }
  .history-axis {
    display: flex;
    justify-content: space-between;
    margin-top: var(--s1);
  }
  .history-key,
  .history-errors {
    display: flex;
    flex-wrap: wrap;
    gap: var(--s3);
    margin-top: var(--s3);
  }
  .history-key i,
  .history-token i {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 2px;
    margin-right: var(--s1);
  }
  .key-requests {
    background: var(--series-1);
  }
  .key-failures {
    background: var(--series-fail);
  }
  .key-unknown {
    background: var(--series-unknown);
  }
  .history-errors span {
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    padding: 2px var(--s2);
  }
  .history-empty {
    display: grid;
    place-items: center;
  }
  .history-mix {
    height: 8px;
    display: flex;
    overflow: hidden;
    border-radius: var(--r-pill);
    margin: var(--s3) 0;
  }
  .mix-0 {
    background: var(--mix-1);
  }
  .mix-1 {
    background: var(--mix-2);
  }
  .mix-2 {
    background: var(--mix-3);
  }
  .mix-3 {
    background: var(--mix-4);
  }
  .history-token,
  .history-model {
    display: grid;
    gap: var(--s2);
    align-items: center;
    font-size: var(--t-xs);
  }
  .history-token {
    grid-template-columns: 1fr auto;
    padding: var(--s2) 0;
    border-top: 1px solid var(--line);
  }
  .history-model {
    grid-template-columns: minmax(0, 1fr) minmax(64px, 0.8fr) 48px;
    margin: var(--s2) 0;
  }
  .history-token strong,
  .history-model strong {
    text-align: right;
  }
  .history-model code {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .history-model .meter-track i {
    background: var(--series-1);
  }
  .top-models {
    grid-column: 1/-1;
  }
  @media (max-width: 820px) {
    .history-grid {
      grid-template-columns: 1fr;
    }
  }
</style>
