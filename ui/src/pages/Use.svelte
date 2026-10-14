<script lang="ts">
  import { tick } from "svelte";
  import { store } from "../store.svelte";
  import { base, clientModels } from "../api";
  import { readPath, mask, newKey, tools, snippet, flag, type Lamp } from "../core";
  import Load from "../Load.svelte";

  const keys = $derived(readPath(store.config.data || {}, "access/api-keys", []) as string[]);
  let pick = $state(0),
    tool = $state<(typeof tools)[number]>("Claude Code"),
    models = $state<string[]>([]),
    model = $state(""),
    test = $state<{ lamp: Lamp; text: string }>({ lamp: "off", text: "" });
  const key = $derived(keys[Math.min(pick, keys.length - 1)] || "");
  const acpLauncher = $derived(readPath(store.config.data || {}, "native-claude/acp-launcher", "/absolute/path/to/cliproxy-native/acp/launch") as string);
  const nativeEnabled = $derived(!!readPath(store.config.data || {}, "native-claude/config-file", ""));
  const acpSetup = $derived(JSON.stringify({agent_servers:{"claude-acp":{type:"custom",command:acpLauncher,args:[],env:{},default_config_options:{mode:"auto",model:"opus",effort:"medium"}}}},null,2));
  const local = /^(localhost|127\.|\[?::1\]?$)/.test(location.hostname);
  // Cursor calls the proxy from its own servers, so a local address cannot work there.
  const target = $derived(tool === "Cursor" && local ? "https://your-public-address" : base);
  const notes: Record<string, string> = {
    "Claude Code": "Run these in the shell you start Claude Code from. To keep them, put both variables in the env block of ~/.claude/settings.json. /status in Claude Code shows the address in use.",
    "Codex CLI": "Add the provider to ~/.codex/config.toml and set the key in your shell.",
    Cursor: "Cursor sends requests from its own servers, so the proxy needs a public HTTPS address, for example through a Cloudflare tunnel. Enter both values under Cursor Settings, Models.",
    "OpenAI SDK": "Python. The JavaScript SDK takes the same baseURL and apiKey. Chat Completions and Responses both work.",
    "Anthropic SDK": "Python. The address has no /v1 here: the SDK adds it.",
    curl: "Lists the models your connected accounts can serve. A 401 answer means the key is wrong.",
  };

  /** Asks this proxy for its model list with the chosen client key, as a tool would. */
  async function check() {
    const k = key;
    test = { lamp: "off", text: "Testing the key…" };
    try {
      const ids = await clientModels(k);
      if (k !== key) return;
      models = ids;
      if (!ids.includes(model)) model = ids[0] || "";
      test = ids.length
        ? { lamp: "ok", text: `Working. This key reaches ${ids.length} model${ids.length > 1 ? "s" : ""}.` }
        : { lamp: "warn", text: "The key works, but no models are available yet. Connect an account first." };
    } catch (e) {
      if (k === key) test = { lamp: "bad", text: e instanceof Error ? e.message : "Cannot reach the proxy." };
    }
  }
  $effect(() => {
    if (key) check();
  });
  async function create() {
    if (!(await store.act(() => store.replace("access/api-keys", keys, [...keys, newKey()]), "Client key created."))) return;
    // The button is gone now; continue at the tool choice unless the user already moved on.
    await tick();
    if (document.activeElement === document.body) document.querySelector<HTMLElement>('.seg button[aria-pressed="true"]')?.focus();
  }
</script>

<div class="head"><h1>Use with tools</h1></div>
<p class="muted">
  Point a coding tool or an SDK at this proxy. It speaks the OpenAI, Anthropic and Gemini APIs and answers with the accounts you
  connected.
</p>

<Load res={store.config} what="Client keys">
  {#snippet children()}
    {#if nativeEnabled}
      <section class="section" aria-label="Native Claude and Zed ACP setup">
        <h2>Zed Claude ACP · native routing</h2>
        <p>Keep native login, history and automatic Remote Control. Conversations stay pinned until their account is unavailable or out of included quota.</p>
        <ol>
          <li>Sign into Claude Code. Set <code>remoteControlAtStartup: true</code> in Claude settings.</li>
          <li>Merge this into <code>~/.config/zed/settings.json</code>.</li>
          <li>Choose <strong>Claude Agent</strong> in Zed. Saved ACP threads resume normally.</li>
        </ol>
        <pre class="code-window" aria-label="Zed Claude ACP setup"><div>{acpSetup}</div></pre>
        <div class="row"><button class="key primary" onclick={() => store.act(() => navigator.clipboard.writeText(acpSetup), "ACP setup copied.")}>Copy ACP settings</button></div>
        <p class="note">Requires the installed ACP launcher. Check routing and cache with <code>cliproxy-native status</code>.</p>
        <p class="note">Keep <code>ANTHROPIC_BASE_URL</code> on Anthropic. The launcher sets CONNECT and certificate trust. Custom-endpoint examples below do not preserve Remote Control.</p>
      </section>
    {/if}
    <section class="section">
      <h2>Address and key</h2>
      <dl class="facts">
        <div><dt>Proxy address</dt><dd class="mono">{base}</dd></div>
        <div><dt>OpenAI-style base URL</dt><dd class="mono">{base}/v1</dd></div>
      </dl>
      {#if local}<p class="note">
          <span class="lamp warn"></span>This address works only on this computer. Other machines need its network address, and
          cloud services such as Cursor need a public HTTPS address.
        </p>{/if}
      {#if keys.length}
        {#if keys.length > 1 || models.length}<div class="form picks">
            {#if keys.length > 1}<label class="field"
                >Client key<select bind:value={pick}>{#each keys as k, i}<option value={i}>{mask(k)}</option>{/each}</select></label
              >{/if}
            {#if models.length}<label class="field"
                >Model<select bind:value={model}>{#each models as m}<option>{m}</option>{/each}</select></label
              >{/if}
          </div>{/if}
        <div class="row">
          <p class="note grow" role="status"><span class="lamp {test.lamp}" class:live={test.text.endsWith("…")}></span>{test.text}</p>
          <button class="key small" onclick={check}>Test again</button>
        </div>
      {:else}
        <div class="state">
          <div class="row"><span class="lamp warn"></span>No client key yet</div>
          <p>Tools need a client key, a password they send to this proxy. Create one here and manage it under Client keys.</p>
          <button class="key primary" disabled={store.busy} onclick={create}
            ><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Create a client key</button
          >
        </div>
      {/if}
    </section>

    {#if keys.length}
      <section class="section">
        <h2>Set up a tool</h2>
        <div class="seg" role="group" aria-label="Tool">
          {#each tools as t}<button aria-pressed={tool === t} onclick={() => (tool = t)}>{t}</button>{/each}
        </div>
        <p class="muted">{notes[tool]}</p>
        <pre class="code-window" aria-label={`${tool} setup`}><div>{snippet(tool, target, mask(key), model || "MODEL_ID")}</div></pre>
        <div class="row">
          <button
            class="key primary"
            onclick={async () => {
              if (await store.act(() => navigator.clipboard.writeText(snippet(tool, target, key, model || "MODEL_ID")), "Copied, with the full key."))
                flag.set("start-tool", "1");
            }}
            ><svg class="i" aria-hidden="true"><use href="#i-copy" /></svg>Copy</button
          >
          <span class="legend">The copy includes the full key; the page shows it shortened.</span>
        </div>
      </section>
    {/if}
  {/snippet}
</Load>
