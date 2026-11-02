<script lang="ts">
  import { tick } from "svelte";
  import QuotaBrief from "../QuotaBrief.svelte";
  import HistoryPanels from "../HistoryPanels.svelte";
  import Load from "../Load.svelte";
  import { store, every } from "../store.svelte";
  import { api } from "../api";
  import { accountStatus } from "../accounts";
  import { accountHealth, requestBuckets } from "../history";
  import { checkAll } from "../quota";
  import { credName, provider, label, type Data } from "../core";
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
  let drawer = $state<HTMLDialogElement>();
  function openAccount(id: string) {
    selectedId = id;
    void tick().then(() => drawer?.showModal());
  }
  function trapDrawerFocus(event: KeyboardEvent) {
    if (event.key !== "Tab" || !drawer) return;
    const controls = Array.from(drawer.querySelectorAll<HTMLElement>("button:not(:disabled), a[href]"));
    const first = controls[0], last = controls.at(-1);
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last?.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first?.focus();
    }
  }
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
      native?.validUntil > store.now,
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
          store.now,
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
    accounts.find(({ a, n }) => (n?.id || a.id || a.name) === selectedId),
  );
  const selectedBuckets = $derived(
    selected?.n?.id
      ? requestBuckets(historyData || {}, selected.n.id)
      : [],
  );
  const selectedStatuses = $derived(
    (historyData?.statuses || []).filter(
      (row: Data) => row.account === selected?.n?.id && row.model === "claude",
    ) as Data[],
  );
  const selectedHealth = $derived(
    historyData && selected?.n?.id
      ? accountHealth(
          selectedStatuses,
          selected.n.id,
          historyData.from,
          historyData.to,
        )
      : [],
  );
  async function toggleSelected() {
    const a = selected?.a;
    if (!a) return;
    await store.act(
      async () => {
        await api("/credentials/status", "PATCH", {
          name: a.name,
          ...(a.auth_index ? { auth_index: a.auth_index } : {}),
          disabled: !a.disabled,
        });
        await Promise.all([store.creds.load(true), store.native.load(true)]);
      },
      a.disabled ? "Account enabled." : "Account disabled.",
    );
  }
</script>

<div class="overview-heading">
  <div>
    <p class="overview-eyebrow">Local routing</p>
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
{#if store.creds.error || store.native.error || !store.online}
  <p class="overview-alert">Refresh failed. Showing the last readings.</p>
{/if}
<section class="section overview-accounts">
  <div class="section-head account-panel-head">
    <h2>
      Accounts <span class="muted account-count"
        >{store.creds.data ? accounts.length : ""}</span
      >
    </h2>
    {#if native?.enabled}<span class="muted small order-summary" title="Session affinity and model limits can change the account selected for a request.">
        {#if order === null}New-session order · awaiting fresh status{:else if native.selectionStrategy === "soonest-reset"}New-session order · soonest weekly reset{#if native.newSessionMinRemaining > 0} · ≥{native.newSessionMinRemaining}% preferred{/if}{:else}New-session order · strategy unavailable{/if}
      </span>{/if}
    <button
      class="key small"
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
            <span class="rank-badge" title={rank >= 0 ? "Default new-session order" : "Excluded from new-session order"}>{rank >= 0 ? rank + 1 : "—"}</span>
            <span class="account-avatar" aria-hidden="true">{(n?.name || a.label || credName(a)).slice(0, 1).toUpperCase()}</span>
            <button
              class="account-title"
              onclick={() => openAccount(n?.id || a.id || a.name)}
              >{n?.name || a.label || credName(a)}</button
            >
          </div>
          <span class="account-state" title={state.detail}
            ><span class="muted">{label(provider(a))}</span><span
              class="lamp {state.lamp}"
            ></span><span>{state.label}</span></span
          >
        </div>
        <QuotaBrief {a} compact />
        <div class="account-history-wrap">
          {#if !native?.enabled || !n?.id}<span class="muted small">Not tracked</span>
          {:else if recent.length}<div
              class="account-history"
              role="img"
              aria-label={`${recent.reduce((sum, b) => sum + b.requests, 0)} recorded requests in ${store.historyRange}`}
            >
              {#each recent as bucket}<span
                  class:unknown={!bucket.known}
                  class:failed={bucket.failures > 0}
                  class:served={bucket.requests > 0}
                  title={bucket.known
                    ? `${bucket.requests} requests · ${bucket.failures} failed`
                    : "No observation"}
                ></span>{/each}
            </div>{:else}<span class="muted small">{historyData ? "No history" : store.history.error ? "History unavailable" : "Loading history…"}</span>{/if}
        </div>
      </article>
    {:else}<div class="state">
        <p>No accounts connected.</p>
        <a class="key primary" href="#connect">Connect account</a>
      </div>{/each}
  </div>{/snippet}</Load>
</section>
{#if native?.enabled}<HistoryPanels data={historyData} />{/if}

{#if selected}
    <dialog
      bind:this={drawer}
      class="account-drawer"
      aria-label={`${selected.n?.name || selected.a.label || credName(selected.a)} details`}
      onclose={() => (selectedId = "")}
      onclick={(event) => { if (event.target === drawer) drawer.close(); }}
      onkeydown={trapDrawerFocus}
    >
      <div class="drawer-head">
        <div>
          <span class="muted small">Account details</span>
          <h2>
            {selected.n?.name || selected.a.label || credName(selected.a)}
          </h2>
          <span class="account-state" title={selected.state.detail}
            ><span class="lamp {selected.state.lamp}"></span>{selected.state
              .label}</span
          >
        </div>
        <button
          class="key small"
          aria-label="Close"
          onclick={() => drawer?.close()}>×</button
        >
      </div>
      <div class="drawer-body">
        <section class="drawer-card">
          <h3>Current capacity</h3>
          <QuotaBrief a={selected.a} />
          <p class="muted small">
            {provider(selected.a) === "claude"
              ? "Claude quota and availability are model scoped."
              : "Quota is reported when the provider supplies it."}
          </p>
        </section>
        {#if native?.enabled && selected.n?.id}<section class="drawer-card">
          <div class="section-head">
            <h3>Availability history</h3>
            <span class="muted small">{store.historyRange}</span>
          </div>
          {#if !historyData}<p class="muted small">{store.history.error ? "History could not be loaded for this period." : "Loading history…"}</p>
          {:else if selectedStatuses.length}<div
              class="availability-strip"
              role="img"
              aria-label="Account availability over the selected period"
            >
              {#each selectedHealth as availability}<span
                  class={availability}
                  title={availability}
                ></span>{/each}
            </div>
            <p class="muted small">
              Worst recorded state per interval · gaps remain unknown
            </p>
          {:else}<p class="muted small">
              No availability observations in this period.
            </p>{/if}
        </section>
        <section class="drawer-card">
          <div class="section-head">
            <h3>Usage history</h3>
            <span class="muted small">{store.historyRange}</span>
          </div>
          {#if !historyData}<p class="muted small">{store.history.error ? "History could not be loaded for this period." : "Loading history…"}</p>
          {:else}
          <div class="drawer-numbers">
            <div>
              <strong
                >{selectedBuckets.reduce((n, b) => n + b.requests, 0)}</strong
              ><span>requests</span>
            </div>
            <div>
              <strong
                >{selectedBuckets.reduce((n, b) => n + b.failures, 0)}</strong
              ><span>failed</span>
            </div>
          </div>
          {#if selectedStatuses.length}<h4>5-hour quota observations</h4>
            <div
              class="quota-trend"
              role="img"
              aria-label="Recorded 5-hour quota remaining"
            >
              {#each selectedStatuses
                .filter((s: Data) => typeof s.fiveHourUsed === "number")
                .slice(-36) as point}<span
                  title={`${new Date(point.at).toLocaleString()} · ${Math.max(0, 100 - point.fiveHourUsed).toFixed(1)}% left`}
                  style:height={`${Math.max(2, 100 - point.fiveHourUsed)}%`}
                ></span>{/each}
            </div>{/if}
          {/if}
        </section>
        {/if}
      </div>
      <div class="drawer-actions">
        <button
          class="key small"
          disabled={store.busy}
          onclick={toggleSelected}
          >{selected.a.disabled ? "Enable" : "Disable"}</button
        >
        <a
          class="key small"
          href={`#credentials/${encodeURIComponent(selected.a.id || selected.a.name)}`}
          onclick={() => drawer?.close()}>Account settings</a
        >
      </div>
    </dialog>
{/if}

<style>
  .account-history {
    display: flex;
    gap: 2px;
    max-width: 180px;
  }
  .account-history span {
    height: 4px;
    flex: 1;
    background: #c9d4dc;
    border-radius: 2px;
  }
  .account-history span.served {
    background: #58ae91;
  }
  .account-history span.failed {
    background: #db887b;
  }
  .account-history span.unknown {
    background: transparent;
    border: 1px solid #c9d4dc;
  }
  .account-title {
    border: 0;
    padding: 0;
    background: transparent;
    color: inherit;
    text-align: left;
    cursor: pointer;
  }
  .account-drawer::backdrop {
    background: #0e1c2b80;
  }
  .account-drawer {
    position: fixed;
    inset: 0 0 0 auto;
    margin: 0;
    width: min(480px, 100%);
    max-width: 100%;
    max-height: 100%;
    padding: 0;
    border: 0;
    color: var(--ink);
    background: var(--body);
    box-shadow: -10px 0 30px #0e1c2b40;
    display: flex;
    flex-direction: column;
  }
  .drawer-head {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    padding: 20px;
    border-bottom: 1px solid var(--line);
  }
  .drawer-head h2 {
    font-size: 21px;
    margin: 3px 0 8px;
  }
  .drawer-head .key {
    align-self: flex-start;
    width: 29px;
    height: 29px;
    padding: 0;
  }
  .drawer-body {
    overflow: auto;
    padding: 14px 18px;
    flex: 1;
  }
  .drawer-card {
    background: var(--raised);
    border: 1px solid var(--line);
    border-radius: 10px;
    padding: 14px;
    margin-bottom: 12px;
  }
  .drawer-card h3 {
    font-size: 13px;
    margin: 0 0 12px;
  }
  .drawer-card h4 {
    font-size: 11px;
    margin: 16px 0 7px;
  }
  .drawer-card > .muted {
    margin-bottom: 0;
  }
  .availability-strip {
    display: flex;
    gap: 2px;
    height: 17px;
  }
  .availability-strip span {
    flex: 1;
    border-radius: 2px;
    background: #cdd5dc;
  }
  .availability-strip span.included {
    background: #5dae91;
  }
  .availability-strip span.extra {
    background: #d5a75a;
  }
  .availability-strip span.cooling,
  .availability-strip span.limited,
  .availability-strip span.signin {
    background: #d88274;
  }
  .availability-strip span.disabled {
    background: #8c99a4;
  }
  .availability-strip span.unknown {
    background: transparent;
    border: 1px solid #aebbc5;
  }
  .drawer-numbers {
    display: flex;
    gap: 20px;
  }
  .drawer-numbers div {
    display: grid;
    gap: 2px;
  }
  .drawer-numbers strong {
    font-size: 22px;
  }
  .drawer-numbers span {
    font-size: 11px;
    color: var(--muted);
  }
  .quota-trend {
    height: 54px;
    border-bottom: 1px solid var(--line);
    display: flex;
    align-items: end;
    gap: 2px;
  }
  .quota-trend span {
    flex: 1;
    background: #55a88b;
    min-width: 2px;
  }
  .drawer-actions {
    padding: 12px 18px;
    border-top: 1px solid var(--line);
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    background: var(--raised);
  }
</style>
