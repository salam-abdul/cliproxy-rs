<script lang="ts">
  import QuotaBrief from "../QuotaBrief.svelte";
  import HistoryPanels from "../HistoryPanels.svelte";
  import Load from "../Load.svelte";
  import AccountDetail from "../AccountDetail.svelte";
  import { store, every } from "../store.svelte";
  import { accountStatus } from "../accounts";
  import { requestBuckets } from "../history";
  import { checkAll } from "../quota";
  import { loadResets } from "../resets";
  import { credName, provider, label, resetSummary, type Data } from "../core";
  $effect(() => every(15000, () => store.creds.load(true), true));
  const enabled = $derived(!!store.native.data?.enabled);
  $effect(() => {
    if (!enabled) return;
    store.historyRange;
    store.history.data = undefined;
    store.history.error = null;
    return every(30000, () => store.history.load(true), true);
  });
  let refreshing = $state(false);
  let selectedId = $state("");
  let mounted = false;
  $effect(() => {
    mounted = true;
    return () => {
      mounted = false;
    };
  });
  async function refreshQuotas() {
    refreshing = true;
    try {
      await checkAll(store.creds.data || [], () => mounted);
      if (mounted) await Promise.all((store.creds.data || []).map((a) => loadResets(a, true)));
      if (mounted) await store.native.load(true);
    } finally {
      refreshing = false;
    }
  }
  const native = $derived(store.native.data);
  const historyData = $derived(
    store.history.data?.range === store.historyRange
      ? store.history.data
      : undefined,
  );
  const fresh = $derived(
    !store.native.error &&
      native?.model === "claude" &&
      native?.validUntil > store.serverNow,
  );
  const order = $derived(
    fresh && Array.isArray(native?.selectionOrder)
      ? (native.selectionOrder as string[])
      : null,
  );
  const accounts = $derived(
    (store.creds.data || [])
      .map((a) => {
        const n = native?.accounts?.find(
          (n: Data) => n.fileId === (a.id || a.name),
        );
        const state = accountStatus(
          a,
          native,
          store.native.error,
          "claude",
          store.serverNow,
        );
        const rank = state.available && n && order ? order.indexOf(n.id) : -1;
        const recent = n?.id
          ? requestBuckets(historyData || {}, n.id)
          : [];
        return { a, n, rank, state, recent };
      })
      .sort(
        (a, b) =>
          (a.rank < 0 ? Infinity : a.rank) - (b.rank < 0 ? Infinity : b.rank),
      ),
  );
  const claudeOnly = $derived(
    accounts.length > 0 && accounts.every(({ a }) => provider(a) === "claude"),
  );
  const selected = $derived(
    accounts.find(({ a, n }) => (n?.id || a.id || a.name) === selectedId)?.a,
  );
</script>

<div class="overview-heading">
  <div>
    <p class="eyebrow">Local routing</p>
    <h1>Overview</h1>
    <p class="muted small">Accounts, capacity, and history in one place.</p>
  </div>
  {#if native?.enabled}<div class="seg history-range" role="group" aria-label="History range">
      {#each [["24h", "24 hours"], ["7d", "7 days"], ["30d", "30 days"]] as [value, title]}
        <button
          aria-pressed={store.historyRange === value}
          onclick={() => (store.historyRange = value)}>{title}</button
        >
      {/each}
    </div>{/if}
</div>
{#if !store.creds.error && (store.native.error || !store.online)}
  <p class="alert">Refresh failed. Showing the last readings.</p>
{/if}
<section class="section card overview-accounts">
  <div class="section-head account-panel-head">
    <h2>
      Accounts <span class="muted account-count"
        >{store.creds.data ? accounts.length : ""}</span
      >
    </h2>
    {#if native?.enabled}<span class="muted small order-summary">
        {#if order === null}New-session order · awaiting fresh status{:else if native.selectionStrategy === "soonest-reset"}New-session order · soonest weekly reset{#if native.newSessionMinRemaining > 0} · ≥{native.newSessionMinRemaining}% preferred{/if}{:else}New-session order · strategy unavailable{/if}
        · bound conversations and model limits can override it
      </span>{/if}
    <button
      class="key"
      disabled={refreshing || !store.creds.data?.length}
      onclick={refreshQuotas}
      >{refreshing ? "Checking…" : "Check quotas"}</button
    >
  </div>
  <div class="account-columns" aria-hidden="true">
    <span>Account</span><span>{claudeOnly ? "5-hour quota" : "Short window"}</span><span>{claudeOnly ? "Weekly quota" : "Long window"}</span><span>Recent requests</span>
  </div>
  <Load res={store.creds} what="Accounts">{#snippet children()}<div class="account-list">
    {#each accounts as { a, n, rank, state, recent } (`${a.name}:${a.auth_index}`)}
      <article class="account-row" class:account-disabled={a.disabled}>
        <div class="account-identity">
          <div class="row">
            <span class="sr">{rank >= 0 ? "New-session position" : "Not in the new-session order"}</span>
            <span class="rank-badge" aria-hidden={rank < 0}>{rank >= 0 ? rank + 1 : "—"}</span>
            <span class="account-avatar" aria-hidden="true">{(n?.name || a.label || credName(a)).slice(0, 1).toUpperCase()}</span>
            <button
              class="account-title"
              onclick={() => (selectedId = n?.id || a.id || a.name)}
              >{n?.name || a.label || credName(a)}</button
            >
          </div>
          <span class="account-state"
            ><span class="muted">{label(provider(a))}</span><span class="lamp {state.lamp}"
            ></span><span>{state.label}</span>{#if state.detail}<span class="muted">· {state.detail}</span>{/if}</span
          >{#if resetSummary(store.resets[a.auth_index]?.resets, store.serverNow)}<span class="muted small"
              >{resetSummary(store.resets[a.auth_index]?.resets, store.serverNow)}</span
            >{/if}
        </div>
        <QuotaBrief {a} compact />
        <div class="account-history-wrap">
          {#if !native?.enabled || !n?.id}<span class="muted account-history-note">Not tracked</span>
          {:else if recent.length}<span class="quota-caption"
              ><span>Last {store.historyRange}</span><strong class="num"
                >{recent.reduce((sum, b) => sum + b.requests, 0)} requests</strong
              ></span
            >
            <div class="account-history" aria-hidden="true">
              {#each recent as bucket}<span
                  class:unknown={!bucket.known}
                  class:failed={bucket.failures > 0}
                  class:served={bucket.requests > 0}
                ></span>{/each}
            </div>
            <span class="muted"
              >{recent.reduce((sum, b) => sum + b.failures, 0)} failed</span
            >{:else}<span class="muted account-history-note">{historyData ? "No history" : store.history.error ? "History unavailable" : "Loading history…"}</span>{/if}
        </div>
      </article>
    {:else}<div class="state">
        <p>No accounts connected.</p>
        <a class="key primary" href="#connect">Connect account</a>
      </div>{/each}
  </div>{/snippet}</Load>
</section>
{#if native?.enabled}<HistoryPanels data={historyData} />{/if}

{#if selected}{#key selectedId}<AccountDetail a={selected} onclose={() => (selectedId = "")} />{/key}{/if}

<style>
  .account-history {
    display: flex;
    gap: 2px;
  }
  .account-history span {
    height: 4px;
    flex: 1;
    background: var(--line-strong);
    border-radius: 2px;
  }
  .account-history span.served {
    background: var(--series-1);
  }
  .account-history span.failed {
    background: var(--series-fail);
  }
  .account-history span.unknown {
    background: transparent;
    border: 1px solid var(--series-unknown);
  }
  .account-title {
    border: 0;
    padding: 0;
    background: transparent;
    color: inherit;
    text-align: left;
    cursor: pointer;
  }
</style>
