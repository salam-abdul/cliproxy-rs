<script lang="ts">
  import { untrack } from "svelte";
  import { ApiError } from "../api";
  import { store } from "../store.svelte";
  import { plural, type Data } from "../core";
  import { bytes, labelText, share, toolGroups } from "../tools";

  // Loads on open and on a range change; the index changes slowly, so nothing polls.
  $effect(() => {
    store.historyRange;
    untrack(() => {
      store.tools.data = undefined;
      store.tools.error = null;
      store.tools.load();
    });
  });
  // While finished files wait for the indexer, reload every 5 s until none do.
  $effect(() => {
    if (!(data?.pendingFiles > 0)) return;
    const timer = setTimeout(() => store.tools.load(true), 5000);
    return () => clearTimeout(timer);
  });
  const data = $derived(store.tools.data?.range === store.historyRange ? store.tools.data : undefined);
  const error = $derived(store.tools.error);
  const groups = $derived(data ? toolGroups(data) : []);
  const skills = $derived<Data[]>(data?.skills || []);
  const at = (ms: number) => new Date(ms).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
  const rate = (n: number | null) => (n === null ? "—" : n.toFixed(1));
  const skipped = $derived(
    data
      ? [
          data.skippedFormat > 0 && `${data.skippedFormat} skipped as not Claude Messages`,
          data.skippedCut > 0 && `${data.skippedCut} with a cut body`,
          data.unreadableFiles > 0 && `${plural(data.unreadableFiles, "unreadable history file")} (all dates)`,
        ].filter(Boolean)
      : [],
  );
</script>

<div class="head">
  <h1>Tools</h1>
  <div class="seg history-range" role="group" aria-label="History range">
    {#each [["24h", "24 hours"], ["7d", "7 days"], ["30d", "30 days"]] as [value, title]}
      <button aria-pressed={store.historyRange === value} onclick={() => (store.historyRange = value)}>{title}</button>
    {/each}
  </div>
</div>
{#if error && !data}
  <p class="alert">
    {error instanceof ApiError && error.status === 409
      ? "Body history is off. Turn on observability.history to see tools."
      : "Tools could not be loaded."}
  </p>
{:else if !data}
  <p class="muted">Loading tools…</p>
{:else}
  {#if data.pendingFiles > 0}<p class="note">Indexing history: {plural(data.pendingFiles, "file")} left.</p>{/if}
  <p class="muted small">
    {data.covered ? `Covered ${at(data.covered.fromMs)} to ${at(data.covered.toMs)}` : "No sends indexed in this period"}
    · {plural(data.sends, "send")} · {plural(data.conversations, "conversation")} with tools
    {#if skipped.length}<br />{skipped.join(" · ")}{/if}
  </p>
  <section class="section card">
    <h2>Tools</h2>
    {#if groups.length}
      <div class="tools-scroll">
        <table class="table" aria-label="Tools">
          <thead>
            <tr>
              <th>Tool</th>
              <th class="count">Carried</th>
              <th class="count">Used</th>
              <th class="count">Weight</th>
              <th class="count">Calls</th>
              <th class="count">Per conversation</th>
              <th class="count">Failure</th>
              <th class="count">Result mean</th>
              <th class="count">Result p95</th>
              <th class="count">Result total</th>
              <th>Labels</th>
            </tr>
          </thead>
          {#each groups as group}
            <tbody>
              <tr>
                <th colspan="11" scope="colgroup"
                  >{group.server
                    ? `${group.name} · ${plural(group.server.tools, "tool")} · carried by ${group.server.carried} · ${plural(group.server.calls, "call")} · ${plural(group.server.errors, "error")}`
                    : group.name}</th
                >
              </tr>
              {#each group.tools as t}
                <tr>
                  <th scope="row" title={t.name}
                    >{t.server ? t.name.slice(`mcp__${t.server}__`.length) : t.name}</th
                  >
                  <td class="count num">{t.carried}<small>{share(t.carriedShare)}</small></td>
                  <td class="count num">{t.used}</td>
                  <td class="count num">{bytes(t.weight)}</td>
                  <td class="count num">{t.calls}</td>
                  <td class="count num">{rate(t.callsPerConversation)}</td>
                  <td class="count num">{share(t.failureRate)}</td>
                  <td class="count num">{bytes(t.resultMean)}</td>
                  <td class="count num">{bytes(t.resultP95)}</td>
                  <td class="count num">{bytes(t.resultTotal)}</td>
                  <td><span class="chips">{#each t.labels as l}<span class="chip">{labelText(l)}</span>{/each}</span></td>
                </tr>
              {/each}
            </tbody>
          {/each}
        </table>
      </div>
    {:else}
      <p class="muted">No tools in this period.</p>
    {/if}
  </section>
  <section class="section card">
    <h2>Skills</h2>
    {#if skills.length}
      <div class="tools-scroll">
        <table class="table" aria-label="Skills">
          <thead>
            <tr>
              <th>Skill</th>
              <th class="count">Calls</th>
              <th class="count">Conversations</th>
              <th class="count">Errors</th>
              <th class="count">Loaded mean</th>
              <th class="count">Loaded p95</th>
            </tr>
          </thead>
          <tbody>
            {#each skills as s}
              <tr>
                <th scope="row">{s.name}</th>
                <td class="count num">{s.calls}</td>
                <td class="count num">{s.conversations}</td>
                <td class="count num">{s.errors}</td>
                <td class="count num">{bytes(s.loadedMean)}</td>
                <td class="count num">{bytes(s.loadedP95)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {:else}
      <p class="muted">No skill calls in this period.</p>
    {/if}
  </section>
  <p class="muted small">The server cannot tell whether a tool's result helped.</p>
{/if}

<style>
  .tools-scroll {
    overflow-x: auto;
  }
  .tools-scroll table {
    min-width: 820px;
  }
  .tools-scroll :is(th, td) {
    white-space: nowrap;
  }
  .tools-scroll small {
    display: block;
    color: var(--ink-muted);
    font-size: var(--t-xs);
  }
</style>
