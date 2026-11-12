<script lang="ts">
  import { onMount, type Component } from "svelte";
  import { store, sections, every } from "./store.svelte";
  import { nextTheme } from "./core";
  import Overview from "./pages/Overview.svelte";
  import Use from "./pages/Use.svelte";
  import Credentials from "./pages/Credentials.svelte";
  import Connect from "./pages/Connect.svelte";
  import Providers from "./pages/Providers.svelte";
  import Keys from "./pages/Keys.svelte";
  import Models from "./pages/Models.svelte";
  import Payload from "./pages/Payload.svelte";
  import Activity from "./pages/Activity.svelte";
  import Logs from "./pages/Logs.svelte";
  import Config from "./pages/Config.svelte";
  import System from "./pages/System.svelte";

  $effect(() => {
    if (!store.logged) return;
    const timer = setInterval(() => (store.now = Date.now()), 1000);
    const stop = every(10000, () => store.native.load(true));
    return () => {
      clearInterval(timer);
      stop();
    };
  });
  const views: Record<string, Component> = {
    overview: Overview,
    use: Use,
    credentials: Credentials,
    connect: Connect,
    providers: Providers,
    keys: Keys,
    models: Models,
    payload: Payload,
    activity: Activity,
    logs: Logs,
    config: Config,
    system: System,
  };
  const View = $derived(views[store.route.page]);
  const current = $derived(
    store.route.page === "connect" ? "credentials" : store.route.page,
  );
  const section = $derived(
    sections.find((s) => s[1].some((p) => p[0] === current))!,
  );
  // On a phone the section tabs scroll sideways; keep the current one in view.
  let tabs = $state<HTMLElement>();
  $effect(() => {
    const a = current && tabs?.querySelector<HTMLElement>("[aria-current]");
    if (tabs && a) tabs.scrollLeft = a.offsetLeft - tabs.offsetLeft - (tabs.clientWidth - a.offsetWidth) / 2;
  });
  const title = $derived(
    store.route.page === "connect"
      ? "Connect an account"
      : section[1].find((p) => p[0] === current)?.[1],
  );
  const host = location.host;
  const version = $derived(
    /^\d/.test(store.meta.version)
      ? `v${store.meta.version}`
      : store.meta.version,
  );
  // After a page change, move focus to the new content (not on credential or log arguments).
  let page = store.route.page;
  $effect(() => {
    if (store.route.page === page) return;
    page = store.route.page;
    document.getElementById("main")?.focus({ preventScroll: true });
  });
  let secret = $state(""),
    failure = $state(""),
    connecting = $state(false);
  let starting = $state(true);
  onMount(async () => {
    try {
      await store.login("", true);
    } catch {
    } finally {
      starting = false;
    }
  });

  async function login(e: SubmitEvent) {
    e.preventDefault();
    connecting = true;
    failure = "";
    try {
      await store.login(secret);
      secret = "";
    } catch (err) {
      failure = err instanceof Error ? err.message : String(err);
    } finally {
      connecting = false;
    }
  }
  $effect(() => {
    const sync = () => {
      store.sync();
    };
    addEventListener("hashchange", sync);
    return () => removeEventListener("hashchange", sync);
  });
</script>

<svelte:head
  ><title
    >{store.logged ? `${title} · cliproxy-rs` : "Sign in · cliproxy-rs"}</title
  ></svelte:head
>
<svelte:window onbeforeunload={(e) => store.dirty && e.preventDefault()} />

{#snippet mark()}<svg
    width="22"
    height="22"
    viewBox="0 0 40 40"
    aria-hidden="true"
    ><rect width="40" height="40" rx="10" fill="var(--ink)" /><path
      d="M11 11h0M20 11h0M29 11h0M11 20h0M29 20h0M11 29h0M20 29h0M29 29h0"
      stroke="var(--ink-muted)"
      stroke-width="7"
      stroke-linecap="round"
    /><circle cx="20" cy="20" r="4.5" fill="var(--accent)" /></svg
  >cliproxy-rs{/snippet}
{#snippet theme()}<button
    class="key quiet round"
    onclick={() => store.toggleTheme()}
    aria-label={`Theme: ${store.theme}. Use ${nextTheme(store.theme)}`}
    ><svg class="i" aria-hidden="true"
      ><use href={`#i-${store.theme === "auto" ? "auto" : store.theme === "dark" ? "moon" : "sun"}`} /></svg
    ></button
  >{/snippet}

{#if starting}
  <main class="login">
    <p role="status">Connecting to the local dashboard…</p>
  </main>
{:else if !store.logged}
  <main class="login">
    <form onsubmit={login}>
      <div class="top-row">
        <h1 class="brand">{@render mark()}</h1>
        {@render theme()}
      </div>
      <div class="window">
        <label class="field"
          >Management key<input
            type="password"
            required
            bind:value={secret}
            autocomplete="current-password"
          /></label
        >
        {#if failure}<p class="note error" role="alert">
            <span class="lamp bad"></span>{failure}
          </p>{/if}
        <button class="key primary" disabled={connecting}
          >{connecting ? "Connecting…" : "Connect"}</button
        >
      </div>
      <p class="small muted">
        Use <code>management.secret-key</code> from the server’s config.yaml. It stays
        in this tab’s memory only; reloading signs you out.
      </p>
    </form>
  </main>
{:else}
  <a
    class="key skip"
    href="#main"
    onclick={(e) => (
      e.preventDefault(),
      document.getElementById("main")?.focus()
    )}>Skip to content</a
  >
  <div class="shell">
    <header class="top">
      <a class="brand" href="#overview">{@render mark()}</a>
      <nav class="nav" aria-label="Pages">
        {#each sections as s}<a
            href={`#${s[1][0][0]}`}
            aria-current={section === s ? "page" : undefined}>{s[0]}</a
          >{/each}
      </nav>
      <span class="top-spacer"></span>
      <span class="row connection" role="status">
        <span class="lamp {store.online ? 'ok' : 'bad'}"></span>
        {store.online ? "Local proxy online" : "Connection lost"}
        <span class="sr">{host}{version ? ` · ${version}` : ""}</span>
      </span>
      <div class="top-actions">
        {@render theme()}
        {#if !store.localDashboard}<button
            class="key quiet"
            onclick={() =>
              (!store.dirty || confirm("Discard unsaved changes?")) &&
              store.logout()}
            ><svg class="i" aria-hidden="true"><use href="#i-out" /></svg>Sign
            out</button
          >{/if}
      </div>
    </header>
    <div class="main">
      <main class="page" id="main" tabindex="-1">
        {#if section[1].length > 1}<nav class="nav section-tabs" aria-label={section[0]} bind:this={tabs}>
            {#each section[1] as [id, name]}<a
                href={`#${id}`}
                aria-current={current === id ? "page" : undefined}>{name}</a
              >{/each}
          </nav>{/if}
        {#key store.route.page}<View />{/key}
      </main>
    </div>
  </div>
{/if}
<div class="toast" role="status">{#if store.toast}<span
      class="lamp {store.toast.bad ? 'bad' : 'ok'}"
    ></span>{store.toast.text}{#if store.toast.bad}<button
        class="key quiet round"
        aria-label="Dismiss"
        onclick={() => (store.toast = null)}
        ><svg class="i" aria-hidden="true"><use href="#i-close" /></svg></button
      >{/if}{/if}</div>
