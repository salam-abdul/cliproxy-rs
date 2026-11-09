<script lang="ts">
  import { untrack } from "svelte";
  import QuotaBrief from "./QuotaBrief.svelte";
  import Load from "./Load.svelte";
  import { store, Res } from "./store.svelte";
  import { api, download } from "./api";
  import { accountStatus } from "./accounts";
  import { accountHealth, requestBuckets } from "./history";
  import { credName, label, provider, span, type Data } from "./core";

  // One account's details, opened as a modal drawer from Overview and Accounts. The parent
  // remounts it per account, so the form starts from the account as it was when opened.
  let { a, onclose }: { a: Data; onclose: () => void } = $props();
  let dialog = $state<HTMLDialogElement>();
  const id = untrack(() => String(a.id || a.name));
  const native = $derived(store.native.data);
  const n = $derived(
    native?.enabled
      ? native.accounts?.find((x: Data) => x.fileId === (a.id || a.name))
      : undefined,
  );
  const status = $derived(
    accountStatus(a, native, store.native.error, "claude", store.serverNow),
  );
  const name = $derived(n?.name || a.label || credName(a));
  const history = $derived(
    store.history.data?.range === store.historyRange
      ? store.history.data
      : undefined,
  );
  const buckets = $derived(n?.id ? requestBuckets(history || {}, n.id) : []);
  const statuses = $derived(
    (history?.statuses || []).filter(
      (row: Data) => row.account === n?.id && row.model === "claude",
    ) as Data[],
  );
  const health = $derived(
    history && n?.id ? accountHealth(statuses, n.id, history.from, history.to) : [],
  );
  const total = (k: "requests" | "failures") =>
    buckets.reduce((sum, b) => sum + b[k], 0);
  const lookup = untrack(() => ({
    name: a.name,
    ...(a.auth_index ? { auth_index: a.auth_index } : {}),
  }));

  const fieldNames = [
    ["note", "Note", ""],
    ["priority", "Priority", "0"],
    ["weight", "Weight", "1"],
    ["request_retry", "Retries", "Global"],
  ] as const;
  const text = (v: unknown) => (v === undefined ? "" : String(v));
  let fields = $state(
    untrack(() => ({
      note: a.note || "",
      priority: text(a.priority),
      weight: text(a.weight),
      request_retry: text(a.request_retry),
    })),
  );
  let baseline = $state(untrack(() => JSON.stringify(fields)));
  $effect(() => {
    store.dirty = JSON.stringify(fields) !== baseline;
  });
  const models = new Res<Data[]>(
    async () =>
      (await api(`/credentials/models?name=${encodeURIComponent(id)}`))
        .models || [],
  );
  models.load();

  const facts = $derived(
    [
      ["File", a.name, true],
      ["Auth index", a.auth_index || "—", true],
      ["Source", a.runtime_only ? "Configuration" : a.source || "file"],
      [
        "Lifetime",
        a.success === undefined
          ? "Not reported"
          : `${a.success} ok · ${a.failed || 0} failed`,
      ],
      [
        "Token refreshed",
        a.last_refresh && new Date(a.last_refresh).toLocaleString(),
      ],
      ["Status", a.status_message],
    ].filter((f) => f[1]),
  );
  const actions = $derived<[string, string, string, Data, string, string, boolean][]>([
    ["Refresh token", "POST", "/credentials/refresh", lookup, "Token refreshed.", "", false],
    [
      "Reset cooldown",
      "POST",
      "/routing/cooldown/reset",
      { auth_index: a.auth_index },
      "Cooldown cleared.",
      "Clear the local cooldown? Provider quota is not restored.",
      !a.auth_index,
    ],
    [
      "Delete",
      "DELETE",
      "/credentials",
      { names: [a.name] },
      "Account deleted.",
      `Delete ${a.name}? The file is removed from the server.`,
      !!a.runtime_only,
    ],
  ]);
  const reload = async () => {
    await Promise.all([store.creds.load(true), store.native.load(true)]);
  };

  function close() {
    if (store.dirty && !confirm("Discard unsaved changes?")) return;
    store.dirty = false;
    dialog?.close();
  }
  async function saveFields() {
    const submitted = JSON.stringify(fields);
    const patch: Data = { name: id, note: fields.note.trim() };
    for (const k of ["priority", "weight", "request_retry"] as const) {
      const v = fields[k].trim();
      // An emptied field clears the override (the server deletes the key on null).
      if (v === "") {
        if (a[k] !== undefined) patch[k] = null;
        continue;
      }
      if (!/^-?\d+$/.test(v))
        return store.notify(`${k.replace("_", " ")} must be a whole number.`, true);
      patch[k] = Number(v);
    }
    if (await store.call("PATCH", "/credentials/fields", patch, "Saved."))
      baseline = submitted;
  }
</script>

<dialog
  bind:this={dialog}
  {@attach (d) => d.showModal()}
  class="account-drawer"
  aria-label={`${name} details`}
  oncancel={(e) => (e.preventDefault(), close())}
  onclose={() => ((store.dirty = false), onclose())}
  onclick={(e) => e.target === dialog && close()}
>
  <div class="drawer-head">
    <div>
      <span class="muted small">Account details</span>
      <h2>{name}</h2>
      <span class="account-state"
        ><span class="muted">{label(provider(a))}</span><span class="lamp {status.lamp}"
        ></span><span>{status.label}</span>{#if status.detail}<span class="muted">· {status.detail}</span>{/if}</span
      >
    </div>
    <button class="key quiet round" aria-label="Close" onclick={close}
      ><svg class="i" aria-hidden="true"><use href="#i-close" /></svg></button
    >
  </div>
  <div class="drawer-body">
    <section class="card">
      <h3>Current capacity</h3>
      <QuotaBrief {a} />
      <p class="muted small">
        {provider(a) === "claude"
          ? "Claude quota and availability are model scoped."
          : "Quota is reported when the provider supplies it."}
      </p>
      {#if n}<p class="muted small">
          {native?.affinity?.find((x: Data) => x.account === n.id)?.sessions || 0} bound
          conversations · not necessarily active
        </p>{/if}
    </section>
    {#if n?.id}
      <section class="card">
        <div class="section-head">
          <h3>Availability history</h3>
          <span class="muted small">{store.historyRange}</span>
        </div>
        {#if !history}<p class="muted small">
            {store.history.error
              ? "History could not be loaded for this period."
              : "Loading history…"}
          </p>
        {:else if statuses.length}<div
            class="availability-strip"
            role="img"
            aria-label="Account availability over the selected period"
          >
            {#each health as availability}<span class={availability}></span>{/each}
          </div>
          <p class="muted small">
            Worst recorded state per interval · gaps remain unknown
          </p>
        {:else}<p class="muted small">
            No availability observations in this period.
          </p>{/if}
      </section>
      <section class="card">
        <div class="section-head">
          <h3>Usage history</h3>
          <span class="muted small">{store.historyRange}</span>
        </div>
        {#if !history}<p class="muted small">
            {store.history.error
              ? "History could not be loaded for this period."
              : "Loading history…"}
          </p>
        {:else}
          <div class="drawer-numbers">
            <div><strong>{total("requests")}</strong><span>requests</span></div>
            <div><strong>{total("failures")}</strong><span>failed</span></div>
          </div>
          {#if statuses.length}<h4>5-hour quota observations</h4>
            <div
              class="quota-trend"
              role="img"
              aria-label="Recorded 5-hour quota remaining"
            >
              {#each statuses
                .filter((s: Data) => typeof s.fiveHourUsed === "number")
                .slice(-36) as point}<span
                  title={`${new Date(point.at).toLocaleString()} · ${Math.max(0, 100 - point.fiveHourUsed).toFixed(1)}% left`}
                  style:height={`${Math.max(2, 100 - point.fiveHourUsed)}%`}
                ></span>{/each}
            </div>{/if}
        {/if}
      </section>
    {/if}
    <details class="card">
      <summary>Advanced account settings</summary>
      <dl class="facts">
        {#each facts as [k, v, mono]}<div>
            <dt>{k}</dt>
            <dd class:mono>{v}</dd>
          </div>{/each}
      </dl>
      {#if a.cooldowns?.length || Object.keys(a.quota?.signals || {}).length}
        <div class="chips">
          {#each a.cooldowns || [] as c}<span class="chip"
              ><span class="lamp warn"></span>{c.model_key || "account"} · {c.reason?.replaceAll(
                "_",
                " ",
              )}{c.http_status ? ` ${c.http_status}` : ""} · {span(
                Date.parse(c.retry_at) - store.serverNow,
              )}</span
            >{/each}
          {#each Object.entries(a.quota?.signals || {}) as [k, v]}<span class="chip"
              >{k}: {v}</span
            >{/each}
        </div>
      {/if}
      {#if !a.runtime_only}
        <form class="form" onsubmit={(e) => (e.preventDefault(), saveFields())}>
          {#each fieldNames as [k, title, hint]}<label class="field" class:note-field={k === "note"}
              >{title}<input
                bind:value={fields[k]}
                placeholder={hint}
                inputmode={k === "note" ? undefined : "numeric"}
              /></label
            >{/each}
          <button class="key" disabled={store.busy}>Save</button>
        </form>
      {/if}
      <h3>Models</h3>
      <Load res={models} what="Models">
        {#snippet children(list)}<div class="chips">
            {#each list as m}<span class="chip">{m.id}</span>{:else}<span class="muted"
                >No models reported.</span
              >{/each}
          </div>{/snippet}
      </Load>
      <div class="row">
        {#each actions as [title, method, path, body, done, ask, off]}<button
            class="key"
            class:danger={method === "DELETE"}
            class:quiet={method === "DELETE"}
            disabled={store.busy || off}
            onclick={async () =>
              (await store.call(method, path, body, done, ask)) &&
              method === "DELETE" &&
              dialog?.close()}>{title}</button
          >{/each}
        <button
          class="key"
          disabled={store.busy || a.runtime_only}
          onclick={() =>
            store.act(async () =>
              download(
                await api(
                  `/credentials/download?name=${encodeURIComponent(a.name)}`,
                  "GET",
                  undefined,
                  "blob",
                ),
                a.name,
              ),
            )}
          ><svg class="i" aria-hidden="true"><use href="#i-download" /></svg>Download</button
        >
      </div>
    </details>
  </div>
  <div class="drawer-actions">
    {#if status.label === "Sign in again"}<a class="key primary" href="#connect"
        >Reconnect account</a
      >{/if}
    <button
      class="key"
      disabled={store.busy}
      onclick={() =>
        store.call(
          "PATCH",
          "/credentials/status",
          { ...lookup, disabled: !a.disabled },
          a.disabled ? "Account enabled." : "Account disabled.",
          "",
          reload,
        )}>{a.disabled ? "Enable account" : "Disable account"}</button
    >
  </div>
</dialog>

<style>
  .account-drawer::backdrop {
    background: var(--shade);
  }
  .account-drawer[open] {
    position: fixed;
    inset: 0 0 0 auto;
    margin: 0;
    width: min(480px, 100%);
    max-width: 100%;
    max-height: 100%;
    height: 100%;
    padding: 0;
    border: 0;
    color: var(--ink);
    background: var(--bg);
    box-shadow: -10px 0 30px var(--shade);
    display: flex;
    flex-direction: column;
  }
  .drawer-head {
    display: flex;
    justify-content: space-between;
    gap: var(--s3);
    padding: var(--s4);
    border-bottom: 1px solid var(--line);
  }
  .drawer-head h2 {
    margin: var(--s1) 0 var(--s2);
  }
  .drawer-body {
    overflow: auto;
    padding: var(--s4);
    flex: 1;
    display: grid;
    gap: var(--s3);
    align-content: start;
  }
  .card {
    display: grid;
    gap: var(--s3);
  }
  .card :global(.quota-meters) {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }
  .card h3 {
    font-size: var(--t-sm);
  }
  summary {
    font-weight: 600;
  }
  .facts {
    grid-template-columns: 1fr 1fr;
    gap: var(--s3) var(--s4);
  }
  .form > .field {
    flex: 0 1 88px;
  }
  .form > .note-field {
    flex: 1 1 100%;
  }
  .availability-strip {
    display: flex;
    gap: 2px;
    height: 16px;
  }
  .availability-strip span {
    flex: 1;
    border-radius: 2px;
    background: var(--series-unknown);
  }
  .availability-strip span.included {
    background: var(--ok);
  }
  .availability-strip span.extra {
    background: var(--warn);
  }
  .availability-strip span.cooling,
  .availability-strip span.limited,
  .availability-strip span.signin {
    background: var(--bad);
  }
  .availability-strip span.disabled {
    background: var(--ink-muted);
  }
  .availability-strip span.unknown {
    background: transparent;
    border: 1px solid var(--series-unknown);
  }
  .drawer-numbers {
    display: flex;
    gap: var(--s6);
  }
  .drawer-numbers div {
    display: grid;
  }
  .drawer-numbers strong {
    font-size: var(--t-xl);
  }
  .drawer-numbers span {
    font-size: var(--t-xs);
    color: var(--ink-muted);
  }
  .quota-trend {
    height: 56px;
    border-bottom: 1px solid var(--line);
    display: flex;
    align-items: end;
    gap: 2px;
  }
  .quota-trend span {
    flex: 1;
    background: var(--accent);
    min-width: 2px;
  }
  .drawer-actions {
    padding: var(--s3) var(--s4);
    border-top: 1px solid var(--line);
    display: flex;
    gap: var(--s2);
    justify-content: flex-end;
    background: var(--surface);
  }
</style>
