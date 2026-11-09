<script lang="ts">
  import { tick, untrack } from "svelte";
  import { store, Res, every } from "../store.svelte";
  import { api, download } from "../api";
  import { type Data } from "../core";
  import { facets, keep, parseLine, type Filter } from "../logs";
  import Load from "../Load.svelte";

  const MAX = 1500;
  // Clearing bumps epoch so a tail read already in flight cannot restore the old cursor.
  let cursor = "",
    epoch = 0,
    pane = $state<HTMLDivElement>(),
    query = $state(""),
    tail = $state(true),
    own = $state(false),
    reqId = $state(store.route.arg),
    reqText = $state(""),
    reqError = $state(""),
    f = $state<Filter>({ method: "", status: "", path: "" }),
    wrap = $state(true),
    box = $state<HTMLElement>(),
    full = $state(false);
  const logs: Res<string[]> = new Res(async (): Promise<string[]> => {
    const follow = !pane || pane.scrollHeight - pane.scrollTop - pane.clientHeight < 40,
      at = epoch;
    const r = await api(`/observability/logs?limit=300${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ""}`);
    const lines: string[] = r.lines || [];
    const next: string[] = cursor && !r["cursor-reset"] ? [...(logs.data || []), ...lines] : lines;
    if (at === epoch) cursor = r["next-cursor"] || "";
    tick().then(() => follow && pane && (pane.scrollTop = pane.scrollHeight));
    return next.slice(-MAX);
  });
  const files = new Res<Data[]>(async () => (await api("/observability/logs/errors")).files || []);
  logs.load();
  files.load();
  $effect(() => every(3000, async () => tail && (await logs.load(true))));
  // The dashboard's own management calls (including this tail) are hidden unless asked for.
  const base = $derived(
    (logs.data || [])
      .filter((l) => (own || !l.includes("/management/")) && l.toLowerCase().includes(query.trim().toLowerCase()))
      .map(parseLine),
  );
  const options = $derived(facets(base));
  const shown = $derived(base.filter((l) => keep(l, f)));
  const filtered = $derived(!!(f.method || f.status || f.path));
  // Browsers expose the clipboard only to HTTPS and localhost pages.
  const copy = (lines: string[], done: string) =>
    store.act(async () => {
      if (!navigator.clipboard) throw new Error("Copying needs the dashboard on HTTPS or localhost.");
      await navigator.clipboard.writeText(lines.join("\n"));
    }, done);
  $effect(() => {
    const on = () => (full = !!box && document.fullscreenElement === box);
    document.addEventListener("fullscreenchange", on);
    return () => document.removeEventListener("fullscreenchange", on);
  });
  async function inspect(id: string) {
    reqId = id;
    reqText = reqError = "";
    try {
      const text = await api(`/observability/logs/requests/${encodeURIComponent(id)}`, "GET", undefined, "text");
      if (id === reqId) reqText = text;
    } catch (e) {
      if (id === reqId) reqError = e instanceof Error ? e.message : String(e);
    }
  }
  $effect(() => {
    const id = store.route.arg;
    if (id) untrack(() => inspect(id));
  });
</script>

<div class="head">
  <h1>Logs</h1>
  <button class="key" aria-pressed={tail} onclick={() => (tail = !tail)}
    ><span class="lamp {tail ? 'ok live' : 'off'}"></span>{tail ? "Following" : "Paused"}</button
  >
  <button
    class="key quiet danger"
    disabled={store.busy}
    onclick={() => store.call("DELETE", "/observability/logs", undefined, "Logs cleared.", "Delete the server’s application logs?", () => ((cursor = ""), epoch++, logs.load()))}
    >Clear</button
  >
</div>

<Load res={logs} what="Application logs">
  {#snippet children(lines)}
    <section class="section logs" bind:this={box}>
      <div class="row">
        <label class="search grow"><svg class="i" aria-hidden="true"><use href="#i-search" /></svg><span class="sr">Filter lines</span><input bind:value={query} placeholder="Filter lines" /></label>
        <label class="check"><input type="checkbox" bind:checked={own} />Management API calls</label>
      </div>
      <div class="row">
        {#each [["method", "Method"], ["status", "Status"]] as const as [k, name]}{#if options[k].length}<div class="seg" role="group" aria-label={name}>
              {#each options[k] as [v, n]}<button aria-pressed={f[k] === v} onclick={() => (f[k] = f[k] === v ? "" : v)}>{v}<b>{n}</b></button>{/each}
            </div>{/if}{/each}
        {#if options.path.length || f.path}<select aria-label="Path" bind:value={f.path}
            ><option value="">All paths</option>{#if f.path && !options.path.some((o) => o[0] === f.path)}<option>{f.path}</option>{/if}{#each options.path.slice(0, 50) as [v, n]}<option value={v}>{v} ({n})</option>{/each}</select
          >{/if}
        {#if filtered}<button class="key quiet" onclick={() => (f = { method: "", status: "", path: "" })}>Clear filters</button>{/if}
        <span class="grow"></span>
        <label class="check"><input type="checkbox" bind:checked={wrap} />Wrap lines</label>
        <button class="key" disabled={!shown.length} onclick={() => copy(shown.map((l) => l.raw), `${shown.length} lines copied.`)}
          ><svg class="i" aria-hidden="true"><use href="#i-copy" /></svg>Copy shown</button
        >
        <button class="key" disabled={!document.fullscreenEnabled} onclick={() => (full ? document.exitFullscreen() : box?.requestFullscreen())}
          ><svg class="i" aria-hidden="true"><use href="#i-open" /></svg>{full ? "Exit full screen" : "Full screen"}</button
        >
      </div>
      <!-- svelte-ignore a11y_no_noninteractive_tabindex: the scrolling log must be reachable by keyboard -->
      <div class="code-window log" class:nowrap={!wrap} bind:this={pane}
        ondblclick={(e) => {
          const line = (e.target as Element).closest(".log > div");
          const i = line ? [...pane!.children].indexOf(line) : -1;
          if (shown[i]) copy([shown[i].raw], "Line copied.");
        }} role="log" aria-label="Application log" tabindex="0">
        {#each shown as l}
          {#if l.time !== undefined}<div class={l.level}>
              <span class="t">{l.time.slice(11)}</span>
              {#if /[^-]/.test(l.id || "")}<a class="t" href={`#logs/${encodeURIComponent(l.id || "")}`}>{l.id}</a>{:else}<span class="t">{l.id}</span>{/if}
              <span class="lvl">{l.level}</span>
              {l.text}
            </div>{:else}<div>{l.raw}</div>{/if}
        {:else}<div class="muted">{query || filtered ? "No lines match." : lines.length ? "" : "No lines yet. Needs observability.logs.logging-to-file."}</div>{/each}
      </div>
      <p class="legend">{shown.length.toLocaleString()} of {lines.length.toLocaleString()} lines · newest {MAX.toLocaleString()} kept in this tab · double-click a line to copy it</p>
    </section>
  {/snippet}
</Load>

<section class="section">
  <h2>Request log</h2>
  <form class="form" onsubmit={(e) => {
      e.preventDefault();
      const id = reqId.trim();
      if (store.route.arg === id) inspect(id);
      else store.go(`logs/${encodeURIComponent(id)}`);
    }}>
    <label class="field">Request ID<input required bind:value={reqId} placeholder="From a log line or the X-Request-Id header" /></label>
    <button class="key">Open</button>
    {#if reqText}<button class="key" onclick={() => download(new Blob([reqText]), `request-${reqId}.log`)}><svg class="i" aria-hidden="true"><use href="#i-download" /></svg>Download</button>{/if}
  </form>
  {#if reqError}<p class="note error"><span class="lamp bad"></span>{reqError}</p>{/if}
  {#if reqText}<pre class="code-window request">{reqText}</pre>{/if}
</section>

<section class="section">
  <h2>Error logs</h2>
  <Load res={files} what="Error logs">
    {#snippet children(list)}
      {#if list.length}
        <ul class="list">
          {#each list as f (f.name)}
            <li class="item">
              <code class="grow ellipsis">{f.name}</code>
              <span class="legend">{f.modified ? new Date(f.modified * 1000).toLocaleString() : ""}</span>
              <button
                class="key"
                onclick={() =>
                  store.act(async () =>
                    download(await api(`/observability/logs/errors/${encodeURIComponent(f.name)}`, "GET", undefined, "blob"), f.name),
                  )}><svg class="i" width="14" height="14" aria-hidden="true"><use href="#i-download" /></svg>Download</button
              >
            </li>
          {/each}
        </ul>
      {:else}<p class="muted">No error logs.</p>{/if}
    {/snippet}
  </Load>
</section>
