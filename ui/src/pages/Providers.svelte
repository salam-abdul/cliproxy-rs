<script lang="ts">
  import { store, Res, every } from "../store.svelte";
  import { api, text } from "../api";
  import { readPath, label, mask, sum, buckets, type Data, type Buckets } from "../core";
  import Grille from "../Grille.svelte";
  import Load from "../Load.svelte";

  const families = ["claude", "codex", "gemini", "vertex", "openai-compatibility", "interactions", "xai", "meta"];
  let family = $state("claude"),
    reveal = $state(false);
  let form = $state({ name: "", url: "", key: "" });
  /** The model list one OpenAI-compatible group offers, while choosing which to add. */
  let offer = $state<{ i: number; ids: string[]; pick: string[]; q: string } | null>(null);
  const usage = new Res<Data>(() => api("/observability/usage/api-keys"));
  usage.load();
  $effect(() => every(15_000, () => usage.load(true)));
  const groups = $derived(readPath(store.config.data || {}, `api-keys/${family}`, []) as Data[]);
  const compat = $derived(family === "openai-compatibility");
  /** Usage is keyed by provider (or compat group name) and "base-url|api-key". */
  function traffic(group: Data, k: Data): Buckets | null {
    const byKey = usage.data?.[compat ? String(group.name || "").toLowerCase() : family];
    const hit = byKey?.[`${group["base-url"] || ""}|${k["api-key"]}`];
    return hit ? buckets({ recent_requests: [], ...hit }) : null;
  }
  const max = $derived(
    Math.max(1, ...groups.flatMap((g) => (g.keys || []).flatMap((k: Data) => traffic(g, k)?.total || []))),
  );
  const write = (next: Data[], done: string) => store.act(() => store.replace(`api-keys/${family}`, groups, next), done);
  /** GET <base-url>/models with the group's first key, sent by the server through the key's proxy. No inference. */
  const listModels = (g: Data, i: number) =>
    store.act(async () => {
      const k: Data = g.keys?.[0] || {};
      const r = await api("/requests/api-call", "POST", {
        method: "GET",
        url: `${String(g["base-url"]).replace(/\/+$/, "")}/models`,
        header: { ...g.headers, ...k.headers, Authorization: `Bearer ${k["api-key"]}` },
        proxy_url: k["proxy-url"] || g["proxy-url"] || "",
      });
      if (r.status_code < 200 || r.status_code >= 300) throw new Error(`The provider answered HTTP ${r.status_code}.`);
      let ids: string[] = [];
      try {
        ids = (JSON.parse(r.body).data || []).map((m: Data) => String(m?.id || "")).filter(Boolean);
      } catch {}
      if (!ids.length) throw new Error("The provider returned no model list.");
      offer = { i, ids: [...new Set(ids)].sort(), pick: [], q: "" };
    });
  function addModels(o: NonNullable<typeof offer>) {
    const n = o.pick.length;
    write(
      groups.map((x, j) => (j === o.i ? { ...x, models: [...(x.models || []), ...o.pick.map((name) => ({ name }))] } : x)),
      `${n} model${n === 1 ? "" : "s"} added.`,
    ).then((ok) => ok && (offer = null));
  }
  function add(e: SubmitEvent) {
    e.preventDefault();
    const group: Data = { name: form.name.trim(), keys: [{ "api-key": form.key.trim() }] };
    if (form.url.trim()) group["base-url"] = form.url.trim();
    write([...groups, group], "Group added.").then((ok) => ok && (form = { name: "", url: "", key: "" }));
  }
</script>

<div class="head">
  <h1>Provider keys</h1>
  {#if groups.length}<button class="key quiet" aria-pressed={reveal} onclick={() => (reveal = !reveal)}
      >{reveal ? "Hide keys" : "Show keys"}</button
    >{/if}
  <a class="key" href="#config"><svg class="i" aria-hidden="true"><use href="#i-edit" /></svg>Edit in Configuration</a>
</div>

<div class="seg tabs" role="group" aria-label="Provider">
  {#each families as f}<button aria-pressed={family === f} onclick={() => (family = f)}
      >{label(f)}<b>{readPath(store.config.data || {}, `api-keys/${f}`, []).length || ""}</b></button
    >{/each}
</div>

  <Load res={store.config} what="Providers">
    {#snippet children()}
      {#if groups.length}
        <ul class="list groups">
          {#each groups as g, i (i)}
            <li class="stack">
              <div class="section-head">
                <h2>{g.name || `Group ${i + 1}`}</h2>
                {#if g.disabled}<span class="row legend"><span class="lamp off"></span>Disabled</span>{/if}
                {#if compat}<button class="key" disabled={store.busy || !g.keys?.length || !g["base-url"]} onclick={() => listModels(g, i)}>Import models</button
                  ><button class="key" disabled={store.busy} onclick={() => write(groups.map((x, n) => (n === i ? { ...x, disabled: !x.disabled } : x)), g.disabled ? "Group enabled." : "Group disabled.")}
                    >{g.disabled ? "Enable" : "Disable"}</button
                  >{/if}
                <button
                  class="key quiet danger"
                  disabled={store.busy}
                  onclick={() => confirm(`Delete group ${g.name || i + 1} and its keys?`) && write(groups.filter((_, n) => n !== i), "Group deleted.")}
                  >Delete</button
                >
              </div>
              <p class="legend mono">{g["base-url"] || "Provider default endpoint"}</p>
              {#each g.keys || [] as k}
                {@const t = traffic(g, k)}
                <div class="item key-row">
                  <code class="grow ellipsis">{reveal ? k["api-key"] : mask(String(k["api-key"] || ""))}</code>
                  <span class="legend">weight {k.weight ?? 1}</span>
                  {#if t}<Grille data={t} {max} /><span class="num count">{sum(t.total).toLocaleString()}</span>{/if}
                </div>
              {/each}
              {#if g.models?.length}<div class="chips">
                  {#each g.models as m}<span class="chip">{m.name}{m.alias && m.alias !== m.name ? ` → ${m.alias}` : ""}</span>{/each}
                </div>{/if}
              {#if offer?.i === i}
                {@const o = offer}
                {@const have = (g.models || []).map((m: Data) => m.name)}
                <div class="row">
                  <label class="search grow"
                    ><svg class="i" aria-hidden="true"><use href="#i-search" /></svg><span class="sr">Filter offered models</span><input
                      bind:value={o.q}
                      placeholder="Filter offered models"
                    /></label
                  >
                  <span class="legend">{o.ids.length} offered</span>
                </div>
                <div class="picklist" role="group" aria-label="Offered models">
                  {#each o.ids.filter((id) => id.toLowerCase().includes(o.q.trim().toLowerCase())) as id (id)}<label class="check"
                      ><input
                        type="checkbox"
                        disabled={have.includes(id)}
                        checked={have.includes(id) || o.pick.includes(id)}
                        onchange={(e) => (o.pick = e.currentTarget.checked ? [...o.pick, id] : o.pick.filter((x) => x !== id))}
                      />{id}</label
                    >{/each}
                </div>
                <div class="row">
                  <button class="key primary" disabled={store.busy || !o.pick.length} onclick={() => addModels(o)}>Add {o.pick.length} selected</button>
                  <button class="key quiet" onclick={() => (offer = null)}>Cancel</button>
                </div>
              {/if}
            </li>
          {/each}
        </ul>
      {:else}
        <div class="state">
          <div class="row"><span class="lamp off"></span>No {label(family)} API keys</div>
          <p>
            API keys from the provider’s developer platform, billed per request. A group is one endpoint with one or more keys,
            used in turn by weight.
          </p>
        </div>
      {/if}
      {#if usage.error}<p class="note"><span class="lamp bad"></span>{text(usage.error)}</p>{/if}
      <form class="form" onsubmit={add}>
        <label class="field">Group name<input required bind:value={form.name} /></label>
        <label class="field"
          >Base URL<input type="url" required={compat} bind:value={form.url} placeholder={compat ? "https://…/v1" : "Provider default"} /></label
        >
        <label class="field">API key<input type="password" autocomplete="off" required bind:value={form.key} /></label>
        <button class="key primary" disabled={store.busy}><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Add group</button>
      </form>
    {/snippet}
  </Load>

