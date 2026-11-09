<script lang="ts">
  import { untrack } from "svelte";
  import { store, every } from "../store.svelte";
  import { api } from "../api";
  import { credName, provider, label, type Data } from "../core";
  import { accountStatus } from "../accounts";
  import Load from "../Load.svelte";
  import AccountDetail from "../AccountDetail.svelte";
  import ClaudePolicy from "../ClaudePolicy.svelte";

  $effect(() => every(15000, () => store.creds.load(true), true));
  // The drawer shows the account's history; Overview keeps it current, this page reads it once.
  $effect(() => {
    if (store.native.data?.enabled) untrack(() => store.history.load(true));
  });

  let query = $state(""),
    filter = $state("all");
  const key = (a: Data) => `${a.name}\u0000${a.auth_index}`;
  // One file can hold several credentials; the ID is unique and every per-credential
  // route accepts it in place of the file name. Downloads and deletes still use the file.
  const id = (a: Data) => String(a.id || a.name);
  const rows = $derived(
    (store.creds.data || []).map((a) => ({
      a,
      s: accountStatus(a, store.native.data, store.native.error, "claude", store.serverNow),
    })),
  );
  const matches = (r: (typeof rows)[number], f: string) =>
    f === "all" ||
    (f === "enabled"
      ? !r.a.disabled
      : f === "disabled"
        ? r.a.disabled
        : !r.a.disabled && ["warn", "bad"].includes(r.s.lamp));
  const filters = $derived(
    [
      ["all", "All"],
      ["enabled", "Enabled"],
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
        `${r.a.name} ${r.a.label || ""} ${credName(r.a)} ${provider(r.a)} ${r.a.note || ""}`
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
  const selected = $derived(rows.find((r) => id(r.a) === store.route.arg)?.a);
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
      files.length > 1 ? `${files.length} files uploaded.` : "Account file uploaded.",
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
          : "No accounts can refresh.",
      );
    });
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

<Load res={store.creds} what="Accounts">
  {#snippet children(list)}
    {#if !list.length}
      <div class="state">
        <div class="row"><span class="lamp off"></span>No accounts yet</div>
        <p>
          Connect an account with the provider’s sign-in, or upload account
          JSON files exported from another CLIProxyAPI server.
        </p>
      </div>
    {:else}
      <section class="section">
        <div class="row">
          <label class="search grow">
            <svg class="i" aria-hidden="true"><use href="#i-search" /></svg
            ><span class="sr">Filter accounts</span>
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
              {@const title = r.a.label || credName(r.a)}
              <li>
                <a class="item" href={`#credentials/${encodeURIComponent(id(r.a))}`}>
                  <span class="lamp {r.s.lamp}"></span>
                  <span class="name"
                    ><strong>{title}</strong
                    >{#if r.a.note || r.a.name !== title}<small
                        >{r.a.note || r.a.name}</small
                      >{/if}</span
                  >
                  <span class="state-label"
                    >{r.s.label}{#if r.s.detail}<small>{r.s.detail}</small>{/if}</span
                  >
                </a>
              </li>
            {/each}
          {:else}
            <li class="state"><p>No accounts match.</p></li>
          {/each}
        </ul>
      </section>
    {/if}
  {/snippet}
</Load>
<ClaudePolicy />

<details class="section">
  <summary>Import and maintenance</summary>
  <div class="row">
    <label class="key">
      <svg class="i" aria-hidden="true"><use href="#i-upload" /></svg>Upload
      <input
        class="file"
        type="file"
        accept=".json,application/json"
        multiple
        disabled={store.busy}
        onchange={(e) => upload(e.currentTarget)}
      />
    </label>
    <button class="key" disabled={store.busy || !rows.length} onclick={refreshAll}
      ><svg class="i" aria-hidden="true"><use href="#i-refresh" /></svg>Refresh
      tokens</button
    >
  </div>
</details>

{#if selected}{#key store.route.arg}<AccountDetail
      a={selected}
      onclose={() => store.go("credentials")}
    />{/key}{/if}
