<script lang="ts">
  import { store, every, Res } from "../store.svelte";
  import { api, download } from "../api";
  import { credName, provider, label, span, type Data } from "../core";
  import QuotaBrief from "../QuotaBrief.svelte";
  import AccountQuota from "../AccountQuota.svelte";
  import { accountStatus } from "../accounts";
  import { checkAll } from "../quota";
  import Load from "../Load.svelte";
  import Missing from "../Missing.svelte";
  import ClaudePolicy from "../ClaudePolicy.svelte";

  $effect(() => {
    let active = true;
    const stop = every(
      15000,
      async () => {
        await store.creds.load(true);
        if (active && !store.creds.error)
          await checkAll(store.creds.data || [], true, () => active);
      },
      true,
    );
    return () => {
      active = false;
      stop();
    };
  });
  let customModel = $state(false);
  let query = $state(""),
    filter = $state("all"),
    open = $state(store.route.arg);
  $effect(() => {
    open = store.route.arg;
  });
  const key = (a: Data) => `${a.name}\u0000${a.auth_index}`;
  // Go allows several credentials from one file; its ID is unique and every per-credential
  // route accepts it in place of the file name. Downloads and deletes still use the file.
  const id = (a: Data) => String(a.id || a.name);
  const rows = $derived(
    (store.creds.data || []).map((a) => ({
      a,
      s: accountStatus(
        a,
        store.native.data,
        store.native.error,
        store.model,
        store.now,
      ),
    })),
  );
  const matches = (r: (typeof rows)[number], f: string) =>
    f === "all" ||
    (f === "available"
      ? r.s.available
      : f === "disabled"
        ? r.a.disabled
        : !r.a.disabled && !r.s.available);
  const filters = $derived(
    [
      ["all", "All"],
      ["available", "Available"],
      ["attention", "Needs attention"],
      ["disabled", "Disabled"],
    ].map(([id, name]) => ({
      id,
      name,
      n: rows.filter((r) => matches(r, id)).length,
    })),
  );
  const shown = $derived(
    rows.filter(
      (r) =>
        matches(r, filter) &&
        `${r.a.name} ${credName(r.a)} ${provider(r.a)} ${r.a.note || ""}`
          .toLowerCase()
          .includes(query.trim().toLowerCase()),
    ),
  );
  const groups = $derived(
    [...new Set(shown.map((r) => provider(r.a)))].map((p) => ({
      p,
      rows: shown.filter((r) => provider(r.a) === p),
    })),
  );

  // Detail panel state for the open credential.
  const selected = $derived(rows.find((r) => id(r.a) === open)?.a);
  let fields = $state({
    note: "",
    priority: "",
    weight: "",
    request_retry: "",
  });
  let baseline = $state("");
  $effect(() => {
    store.dirty =
      !!selected && !!baseline && JSON.stringify(fields) !== baseline;
  });
  let models = $state<Res<Data[]> | null>(null);
  // Fill the form when a credential is opened, not on every poll, so typing is not overwritten.
  let formFor = "";
  $effect(() => {
    const a = selected;
    const k = a ? id(a) : "";
    if (k === formFor) return;
    formFor = k;
    if (!a) return;
    fields = {
      note: a.note || "",
      priority: a.priority === undefined ? "" : String(a.priority),
      weight: a.weight === undefined ? "" : String(a.weight),
      request_retry:
        a.request_retry === undefined ? "" : String(a.request_retry),
    };
    baseline = JSON.stringify(fields);
    const name = id(a);
    const res = new Res<Data[]>(
      async () =>
        (await api(`/credentials/models?name=${encodeURIComponent(name)}`))
          .models || [],
    );
    models = res;
    res.load();
  });
  function toggle(a: Data) {
    store.go(
      open === id(a)
        ? "credentials"
        : `credentials/${encodeURIComponent(id(a))}`,
    );
  }
  const lookup = (a: Data) => ({
    name: a.name,
    ...(a.auth_index ? { auth_index: a.auth_index } : {}),
  });
  const fieldNames = [
    ["note", "Note", ""],
    ["priority", "Priority", "0"],
    ["weight", "Weight", "1"],
    ["request_retry", "Retries", "Global"],
  ] as const;
  const facts = (a: Data) =>
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
    ].filter((f) => f[1]);
  const actions = (
    a: Data,
  ): [string, string, string, Data, string, string, boolean][] => [
    [
      a.disabled ? "Enable" : "Disable",
      "PATCH",
      "/credentials/status",
      { ...lookup(a), disabled: !a.disabled },
      a.disabled ? "Enabled." : "Disabled.",
      "",
      false,
    ],
    [
      "Refresh token",
      "POST",
      "/credentials/refresh",
      lookup(a),
      "Token refreshed.",
      "",
      false,
    ],
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
      "Credential deleted.",
      `Delete ${a.name}? The file is removed from the server.`,
      !!a.runtime_only,
    ],
  ];
  const reload = () => store.creds.load(true);

  async function upload(input: HTMLInputElement) {
    const files = [...(input.files || [])];
    input.value = "";
    if (!files.length) return;
    await store.act(
      async () => {
        const form = new FormData();
        files.forEach((f) => form.append("file", f));
        const result = await api("/credentials", "POST", form);
        await reload();
        if (result.failed?.length)
          throw new Error(
            `${result.uploaded || 0} uploaded. Failed: ${result.failed.map((f: Data) => `${f.name} (${f.error})`).join(", ")}`,
          );
      },
      files.length > 1
        ? `${files.length} files uploaded.`
        : "Credential uploaded.",
    );
  }
  function refreshAll() {
    store.act(async () => {
      const result = await api("/credentials/refresh", "POST", { all: true });
      await reload();
      const bad = (result.results || []).filter((r: Data) => !r.success);
      if (bad.length)
        throw new Error(
          `${bad.length} could not refresh: ${bad.map((r: Data) => `${r.id} (${r.error})`).join(", ")}`,
        );
      store.notify(
        result.results?.length
          ? `${result.results.length} tokens refreshed.`
          : "No credentials can refresh.",
      );
    });
  }
  async function saveFields(a: Data) {
    const submitted = JSON.stringify(fields);
    const patch: Data = { name: id(a), note: fields.note.trim() };
    for (const k of ["priority", "weight", "request_retry"] as const) {
      const v = fields[k].trim();
      // An emptied field clears the override (Go deletes the key on null).
      if (v === "") {
        if (a[k] !== undefined) patch[k] = null;
        continue;
      }
      if (!/^-?\d+$/.test(v))
        return store.notify(
          `${k.replace("_", " ")} must be a whole number.`,
          true,
        );
      patch[k] = Number(v);
    }
    if (
      (await store.call("PATCH", "/credentials/fields", patch, "Saved.")) &&
      formFor === id(a)
    )
      baseline = submitted;
  }
</script>

<div class="head">
  <h1>
    Accounts{#if rows.length}<span>{rows.length}</span>{/if}
  </h1>
  <a class="key primary" href="#connect"
    ><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Connect
    account</a
  >
</div>

<Missing
  actions={[
    ["POST", "/credentials", "upload"],
    ["POST", "/credentials/refresh", "token refresh"],
    ["PATCH", "/credentials/fields", "editing fields"],
    ["DELETE", "/credentials", "delete"],
    ["POST", "/routing/cooldown/reset", "cooldown reset"],
  ]}
/>

<label class="field model-choice"
  >Claude availability for
  <select
    aria-label="Routing model"
    value={customModel ? "custom" : store.model}
    onchange={(e) => {
      customModel = e.currentTarget.value === "custom";
      if (!customModel) store.model = e.currentTarget.value;
    }}
  >
    <option value="claude-sonnet-4-6">Sonnet 4.6</option><option
      value="claude-opus-4-6">Opus 4.6</option
    ><option value="claude-haiku-4-5-20251001">Haiku 4.5</option><option
      value="custom">Custom model…</option
    >
  </select>
</label>
{#if customModel}<label class="field model-choice"
    >Model identifier<input maxlength="200" bind:value={store.model} /></label
  >{/if}

<Load res={store.creds} what="Credentials">
  {#snippet children(list)}
    {#if !list.length}
      <div class="state">
        <div class="row"><span class="lamp off"></span>No credentials yet</div>
        <p>
          Connect an account with the provider’s sign-in, or upload credential
          JSON files exported from another CLIProxyAPI server.
        </p>
      </div>
    {:else}
      <section class="section">
        <div class="row">
          <label class="search grow">
            <svg class="i" aria-hidden="true"><use href="#i-search" /></svg
            ><span class="sr">Filter credentials</span>
            <input
              bind:value={query}
              placeholder="Filter by name, provider or note"
            />
          </label>
          <div class="seg" role="group" aria-label="Show">
            {#each filters as f (f.id)}<button
                aria-pressed={filter === f.id}
                onclick={() => (filter = f.id)}>{f.name}<b>{f.n}</b></button
              >{/each}
          </div>
        </div>
        <ul class="list accounts">
          {#each groups as g (g.p)}
            <li class="group">{label(g.p)}<span>{g.rows.length}</span></li>
            {#each g.rows as r (key(r.a))}
              {@const isOpen = open === id(r.a)}
              <li class:open={isOpen}>
                <button
                  class="item"
                  aria-expanded={isOpen}
                  onclick={() => toggle(r.a)}
                >
                  <span class="lamp {r.s.lamp}"></span>
                  <span class="name"
                    ><strong>{credName(r.a)}</strong
                    >{#if r.a.note || credName(r.a) !== r.a.name}<small
                        >{r.a.note
                          ? `${r.a.note} · ${r.a.name}`
                          : r.a.name}</small
                      >{/if}<QuotaBrief a={r.a} /></span
                  >
                  <span class="state-label"
                    >{r.s.label}{#if r.s.detail}<small title={r.s.detail}
                        >{r.s.detail}</small
                      >{/if}</span
                  >
                </button>
                {#if isOpen}
                  {@const a = r.a}
                  {@const routeAccount = store.native.data?.accounts?.find(
                    (n: Data) => n.fileId === (a.id || a.name),
                  )}
                  <div class="detail window">
                    <div class="row">
                      <button
                        class="key"
                        disabled={store.busy ||
                          !store.can("PATCH", "/credentials/status")}
                        onclick={() =>
                          store.call(
                            "PATCH",
                            "/credentials/status",
                            { ...lookup(a), disabled: !a.disabled },
                            a.disabled ? "Enabled." : "Disabled.",
                            "",
                            async () => {
                              await reload();
                              await store.native.load(true);
                            },
                          )}
                        >{a.disabled
                          ? "Enable account"
                          : "Disable account"}</button
                      >
                      {#if r.s.label === "Sign in again"}<a
                          class="key primary"
                          href="#connect">Reconnect account</a
                        >{/if}
                    </div>
                    <AccountQuota {a} />
                    {#if routeAccount}<p class="muted">
                        {store.native.data?.affinity?.find(
                          (n: Data) => n.account === routeAccount.id,
                        )?.sessions || 0} bound conversations · not necessarily active
                      </p>{/if}
                    <details>
                      <summary>Advanced account settings</summary>
                      <dl class="facts">
                        {#each facts(a) as [k, v, mono]}<div>
                            <dt>{k}</dt>
                            <dd class:mono>{v}</dd>
                          </div>{/each}
                      </dl>
                      {#if a.cooldowns?.length || (a.quota?.signals && Object.keys(a.quota.signals).length)}
                        <div class="chips">
                          {#each a.cooldowns || [] as c}<span class="chip"
                              ><span class="lamp warn"></span>{c.model_key ||
                                "credential"} · {c.reason?.replaceAll(
                                "_",
                                " ",
                              )}{c.http_status ? ` ${c.http_status}` : ""} · {span(
                                Date.parse(c.retry_at) - Date.now(),
                              )}</span
                            >{/each}
                          {#each Object.entries(a.quota?.signals || {}) as [k, v]}<span
                              class="chip">{k}: {v}</span
                            >{/each}
                        </div>
                      {/if}
                      {#if !a.runtime_only}
                        <form
                          class="form"
                          onsubmit={(e) => (e.preventDefault(), saveFields(a))}
                          inert={!store.can("PATCH", "/credentials/fields")}
                        >
                          {#each fieldNames as [k, name, hint]}<label
                              class="field"
                              class:note-field={k === "note"}
                              >{name}<input
                                bind:value={fields[k]}
                                placeholder={hint}
                                inputmode={k === "note" ? undefined : "numeric"}
                              /></label
                            >{/each}
                          <button
                            class="key"
                            disabled={store.busy ||
                              !store.can("PATCH", "/credentials/fields")}
                            >Save</button
                          >
                        </form>
                      {/if}
                      <div class="stack">
                        <h3>Models</h3>
                        {#if models}<Load res={models} what="Models">
                            {#snippet children(list)}<div class="chips">
                                {#each list as m}<span class="chip">{m.id}</span
                                  >{:else}<span class="muted"
                                    >No models reported.</span
                                  >{/each}
                              </div>{/snippet}
                          </Load>{/if}
                      </div>
                      <div class="row">
                        {#each actions(a).slice(1) as [name, method, path, body, done, ask, off]}<button
                            class="key small"
                            class:danger={method === "DELETE"}
                            class:quiet={method === "DELETE"}
                            disabled={store.busy ||
                              off ||
                              !store.can(method, path)}
                            onclick={() =>
                              store
                                .call(method, path, body, done, ask)
                                .then(
                                  (ok) =>
                                    ok &&
                                    method === "DELETE" &&
                                    store.go("credentials"),
                                )}>{name}</button
                          >{/each}
                        <button
                          class="key small"
                          disabled={store.busy ||
                            a.runtime_only ||
                            !store.can("GET", "/credentials/download")}
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
                          ><svg class="i" aria-hidden="true"
                            ><use href="#i-download" /></svg
                          >Download</button
                        >
                      </div>
                    </details>
                  </div>
                {/if}
              </li>
            {/each}
            {#if g.p === "claude"}<li class="policy"><ClaudePolicy /></li>{/if}
          {:else}
            <li class="state"><p>No credentials match.</p></li>
          {/each}
        </ul>
      </section>
    {/if}
  {/snippet}
</Load>
<p class="legend">
  Status refreshes every 10 seconds; quotas every minute while this page is
  visible. Disabled accounts are skipped.
</p>

<details class="section">
  <summary>Import and maintenance</summary>
  <div class="row">
    <label class="key" aria-disabled={!store.can("POST", "/credentials")}>
      <svg class="i" aria-hidden="true"><use href="#i-upload" /></svg>Upload
      <input
        class="file"
        type="file"
        accept=".json,application/json"
        multiple
        disabled={store.busy || !store.can("POST", "/credentials")}
        onchange={(e) => upload(e.currentTarget)}
      />
    </label>
    <button
      class="key"
      disabled={store.busy ||
        !rows.length ||
        !store.can("POST", "/credentials/refresh")}
      onclick={refreshAll}
      ><svg class="i" aria-hidden="true"><use href="#i-refresh" /></svg>Refresh
      tokens</button
    >
  </div>
</details>
