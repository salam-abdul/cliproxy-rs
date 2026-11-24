<script lang="ts">
  import { store, Res } from "../store.svelte";
  import { api } from "../api";
  import { readPath, label, type Data } from "../core";
  import Load from "../Load.svelte";
  import { addEntry, entries, modelState, switchModel } from "../models";

  const channels = ["claude", "codex", "antigravity", "kimi", "xai", "meta", "vertex", "aistudio"];
  let channel = $state("claude");
  let alias = $state({ name: "", alias: "", fork: false }),
    pattern = $state("");
  const aliases = $derived(readPath(store.config.data || {}, `oauth/model-alias/${channel}`, []) as Data[]);
  // The server's copy, for the conflict check, and its entries as strings.
  const stored = $derived(readPath(store.config.data || {}, `oauth/excluded-models/${channel}`, []));
  const excluded = $derived(entries(stored));
  let catalog = $state.raw<Res<Data[]>>();
  $effect(() => {
    const c = channel;
    const r = new Res<Data[]>(async () => (await api(`/routing/model-definitions/${encodeURIComponent(c)}`)).models || []);
    catalog = r;
    r.load();
  });
  const k = (n: number) => (n >= 1000 ? `${Math.round(n / 1000)}k` : String(n || "—"));
  function addAlias(e: SubmitEvent) {
    e.preventDefault();
    const next = { name: alias.name.trim(), alias: alias.alias.trim(), ...(alias.fork ? { fork: true } : {}) };
    if (aliases.some((a) => a.alias === next.alias)) return store.notify("That alias already exists.", true);
    store
      .act(() => store.replace(`oauth/model-alias/${channel}`, aliases, [...aliases, next]), "Alias added.")
      .then((ok) => ok && (alias = { name: "", alias: "", fork: false }));
  }
  const saveExcluded = (next: string[], done?: string) =>
    store.act(() => store.replace(`oauth/excluded-models/${channel}`, stored, next), done);
  const setModel = (id: string, on: boolean) => saveExcluded(switchModel(id, excluded, on), `${id} ${on ? "enabled" : "excluded"}.`);
  function exclude(e: SubmitEvent) {
    e.preventDefault();
    saveExcluded(addEntry(excluded, pattern), "Model excluded.").then((ok) => ok && (pattern = ""));
  }
</script>

<div class="head"><h1>Models</h1></div>
<div class="seg tabs" role="group" aria-label="Channel">
  {#each channels as c}<button aria-pressed={channel === c} onclick={() => (channel = c)}>{label(c)}</button>{/each}
</div>

<Load res={store.config} what="Model settings">
  {#snippet children()}
    <section class="section">
      <div class="section-head"><h2>Aliases</h2><span class="legend">Clients ask for the alias; the upstream receives the model.</span></div>
      {#if aliases.length}
        <ul class="list">
          {#each aliases as a, i}
            <li class="item">
              <code>{a.name}</code><svg class="i" width="14" height="14" aria-hidden="true"><use href="#i-chevron" /></svg><code class="grow">{a.alias}</code>
              {#if a.fork}<span class="legend">keeps original</span>{/if}
              <button
                class="key quiet danger"
                disabled={store.busy}
                onclick={() => store.act(() => store.replace(`oauth/model-alias/${channel}`, aliases, aliases.filter((_, n) => n !== i)), "Alias removed.")}
                >Remove</button
              >
            </li>
          {/each}
        </ul>
      {:else}<p class="muted">No aliases. Clients use upstream model names.</p>{/if}
      <form class="form" onsubmit={addAlias}>
        <label class="field">Upstream model<input list="defs" required bind:value={alias.name} /></label>
        <label class="field">Alias<input required bind:value={alias.alias} /></label>
        <label class="check"><input type="checkbox" bind:checked={alias.fork} />Keep original</label>
        <button class="key" disabled={store.busy}><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Add alias</button>
      </form>
    </section>

    <section class="section">
      <div class="section-head"><h2>Excluded</h2><span class="legend">Hidden and refused for signed-in accounts on the proxy only. Wildcards allowed.</span></div>
      <div class="chips">
        {#each excluded as m, i}<span class="chip"
            >{m}<button
              aria-label={`Allow ${m}`}
              disabled={store.busy}
              onclick={() => saveExcluded(excluded.filter((_, n) => n !== i))}
              ><svg class="i" width="12" height="12" aria-hidden="true"><use href="#i-close" /></svg></button
            ></span
          >{:else}<span class="muted">Every model is available.</span>{/each}
      </div>
      <form class="form" onsubmit={exclude}>
        <label class="field">Model or pattern<input required bind:value={pattern} placeholder="*-preview" /></label>
        <button class="key" disabled={store.busy}>Exclude</button>
      </form>
    </section>
  {/snippet}
</Load>

<section class="section">
  <div class="section-head">
    <h2>Catalog</h2>{#if catalog?.data}<span class="legend"
        >{catalog.data.length} {catalog.data.length === 1 ? "model" : "models"}</span
      >{/if}
  </div>
  {#if catalog}<Load res={catalog} what="The model catalog">
    {#snippet children(list)}
      {#if list.length}
        <table class="table">
          <thead><tr><th>Model</th><th>Name</th><th class="count">Context</th><th class="count">Output</th><th>Enabled</th></tr></thead>
          <tbody>
            {#each list as m (m.id)}
              <!-- Unknown until the settings load: no switch claims a model is on. -->
              {@const state = store.config.data && modelState(m.id, excluded)}
              <tr>
                <td><code>{m.id}</code></td><td>{m.display_name || ""}</td>
                <td class="num count">{k(m.context_length)}</td><td class="num count">{k(m.max_completion_tokens)}</td>
                <td>
                  {#if state}<button
                      class="key quiet"
                      role="switch"
                      aria-checked={state.on}
                      aria-label={`${m.id} enabled`}
                      disabled={store.busy || state.lockedBy.length > 0}
                      onclick={() => setModel(m.id, !state.on)}
                      ><span class="switch-track" aria-hidden="true"><span></span></span></button
                    >{#if state.lockedBy.length}<span class="legend">Excluded by <code>{state.lockedBy.join(", ")}</code></span>{/if}{/if}
                </td>
              </tr>{/each}
          </tbody>
        </table>
        <datalist id="defs">{#each list as m}<option value={m.id}></option>{/each}</datalist>
      {:else}<p class="muted">No static definitions for {label(channel)}.</p>{/if}
    {/snippet}
  </Load>{/if}
</section>
