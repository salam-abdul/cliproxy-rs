<script lang="ts">
  import { ApiError } from "./api";
  import { store } from "./store.svelte";
  import { healthBlocks, providerActivity, type Block } from "./providers";
  import { label, type Data } from "./core";

  let { data }: { data: Data | undefined } = $props();
  const rows = $derived(data ? providerActivity(data, store.creds.data || []) : []);
  const sends = $derived(rows.reduce((n, r) => n + r.totals.sends, 0));
  const exact = $derived(rows.reduce((n, r) => n + r.totals.exactSends, 0));
  const error = $derived(store.providerActivity.error);
  let open = $state<Record<string, boolean>>({});
  const count = (n: number) =>
    Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(n);
  const pct = (n: number | null) =>
    n === null
      ? "—"
      : Intl.NumberFormat(undefined, { style: "percent", maximumFractionDigits: 1 }).format(n);
  const points = (n: number | null) => (n === null ? "—" : `${Math.round(n)}%`);
  const date = (n: number) =>
    new Date(n).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric" });
  const blockText = (b: Block) =>
    `${date(b.at)}: ${
      b.state === "rated"
        ? `${pct(b.rate)} succeeded`
        : b.state === "unknown"
          ? "unknown"
          : b.state === "idle"
            ? "no sends"
            : "all rate-limited or cancelled"
    }`;
</script>

{#snippet figures(r: { totals: Data; sendShare: number | null; outputShare: number | null; failureRate: number | null })}
  <td class="count num">{count(r.totals.sends)}<small>{pct(r.sendShare)}</small></td>
  <td class="count num">{count(r.totals.outputTokens)}<small>{pct(r.outputShare)}</small></td>
  <td class="count num">{pct(r.failureRate)}</td>
  <td class="count num">{count(r.totals.rateLimited)}</td>
  <td class="count num">{count(r.totals.failedOver)}</td>
{/snippet}

<section class="section card provider-activity" aria-labelledby="provider-activity-title">
  <div class="section-head">
    <h2 id="provider-activity-title">Provider activity</h2>
    <span class="muted small">Upstream sends · selected period</span>
  </div>
  {#if error}
    <p class="alert">
      {error instanceof ApiError && error.status === 409
        ? "The activity store is not running."
        : "Provider activity could not be loaded."}
    </p>
  {:else if !data}
    <p class="muted">Loading provider activity…</p>
  {:else if !rows.length}
    <p class="muted">No upstream sends recorded in this period.</p>
  {:else}
    <div class="provider-scroll">
      <table class="table">
        <thead>
          <tr>
            <th>Provider / Account</th>
            <th class="count">Sends</th>
            <th class="count">Output tokens</th>
            <th class="count">Failure rate</th>
            <th class="count">429s</th>
            <th class="count">Failovers</th>
            <th class="count">Weekly peak</th>
            <th class="count">Unused weekly quota</th>
            <th class="count">Tokens per %</th>
            <th class="count">Cache read rate</th>
          </tr>
        </thead>
        {#each rows as row (row.provider)}
          <tbody>
            <tr class="provider-row">
              <th scope="row">
                <button
                  class="provider-toggle"
                  aria-expanded={!!open[row.provider]}
                  onclick={() => (open[row.provider] = !open[row.provider])}
                  >{label(row.provider)}</button
                >
                <div class="provider-health">
                  {#each healthBlocks(data, row.provider) as block}<span
                      class="{block.state} band-{block.band}"><span class="sr">{blockText(block)}</span></span
                    >{/each}
                </div>
              </th>
              {@render figures(row)}
              <td class="count">—</td>
              <td class="count num"
                >{row.unusedAccounts === null
                  ? "—"
                  : `${row.unusedAccounts.toFixed(1)} of ${row.endedAccounts} accounts`}</td
              >
              <td class="count">—</td>
              <td class="count num">{pct(row.cacheReadRate)}</td>
            </tr>
            {#if open[row.provider]}
              {#each row.accounts as account (account.credential)}
                <tr class="provider-account">
                  <th scope="row">{account.name}</th>
                  {@render figures(account)}
                  <td class="count num">{points(account.peak)}</td>
                  <td class="count num"
                    >{points(account.unused)}{#if account.unused !== null && !account.unusedInRange}<small
                        >before this period</small
                      >{/if}</td
                  >
                  <td class="count num"
                    >{account.tokensPerPercent === null
                      ? "—"
                      : count(account.tokensPerPercent)}{#if account.tokensPerPercent !== null && account.tokensPartial}<small
                        >partial</small
                      >{/if}</td
                  >
                  <td class="count num">{pct(account.cacheReadRate)}</td>
                </tr>
              {/each}
            {/if}
          </tbody>
        {/each}
      </table>
    </div>
    <p class="muted small">
      {#if data.lostEvents > 0}{data.lostEvents} events could not be saved.{/if}
      {#if data.coverageUncertain}Coverage is uncertain for this period.{/if}
      {#if exact < sends}Token figures cover {exact} of {sends} sends.{/if}
      Tokens per percent moves with the model mix and with use outside this server.
    </p>
  {/if}
</section>

<style>
  .provider-scroll {
    overflow-x: auto;
  }
  .provider-scroll table {
    min-width: 920px;
  }
  .provider-scroll :is(th, td) {
    white-space: nowrap;
    overflow-wrap: normal;
    vertical-align: top;
  }
  .provider-scroll small {
    display: block;
    color: var(--ink-muted);
    font-size: var(--t-xs);
  }
  .provider-toggle {
    border: 0;
    padding: 0;
    background: transparent;
    color: inherit;
    font-weight: 600;
    cursor: pointer;
  }
  .provider-toggle::before {
    content: "";
    display: inline-block;
    margin-right: var(--s2);
    border: 4px solid transparent;
    border-left-color: currentColor;
    border-right-width: 0;
  }
  .provider-toggle[aria-expanded="true"]::before {
    transform: rotate(90deg);
  }
  .provider-row th {
    color: inherit;
    font-size: inherit;
  }
  .provider-account th {
    padding-left: var(--s4);
    font-weight: 400;
    color: inherit;
    font-size: inherit;
  }
  .provider-health {
    display: flex;
    gap: 1px;
    width: 240px;
    margin-top: var(--s2);
  }
  .provider-health > span {
    flex: 1;
    box-sizing: border-box;
    height: 10px;
    border-radius: 1px;
  }
  .provider-health .unknown {
    border: 1px solid var(--series-unknown);
  }
  .provider-health .rejected {
    border: 1px solid var(--warn);
    background: repeating-linear-gradient(45deg, var(--warn) 0 1px, transparent 1px 3px);
  }
  .provider-health .band-0 {
    background: var(--bad);
  }
  .provider-health .band-1 {
    background: var(--warn);
  }
  .provider-health .band-2 {
    background: var(--series-1);
    opacity: 0.5;
  }
  .provider-health .band-3 {
    background: var(--series-1);
  }
</style>
