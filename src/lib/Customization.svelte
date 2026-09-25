<script lang="ts">
  import Icon from "$lib/Icon.svelte";
  import {
    TOKENS,
    activeTheme,
    allThemes,
    customization,
    duplicate,
    isBuiltIn,
    newId,
    type Theme,
  } from "$lib/theme.svelte";

  const groups = [...new Set(TOKENS.map((t) => t.group))];
  const HEX = /^#[0-9a-f]{6}$/i;

  let importText = $state("");
  let importError = $state<string | null>(null);
  let copied = $state(false);

  const theme = $derived(activeTheme());
  const builtIn = $derived(isBuiltIn(theme));

  /** Built-in themes stay pristine: the first edit forks a copy. */
  function editable(): Theme {
    return builtIn ? duplicate(theme) : theme;
  }

  function setToken(key: string, value: string) {
    editable().tokens[key] = value;
  }

  function removeTheme(id: string) {
    customization.themes = customization.themes.filter((t) => t.id !== id);
    if (customization.theme === id) customization.theme = "dark";
  }

  async function exportTheme() {
    await navigator.clipboard.writeText(JSON.stringify({ name: theme.name, tokens: theme.tokens }, null, 2));
    copied = true;
    setTimeout(() => (copied = false), 1500);
  }

  function importTheme() {
    importError = null;
    try {
      const parsed = JSON.parse(importText);
      if (typeof parsed?.tokens !== "object" || parsed.tokens === null) throw new Error();
      const tokens: Record<string, string> = {};
      for (const [key, value] of Object.entries(parsed.tokens)) {
        if (typeof value === "string") tokens[key] = value;
      }
      const imported = { id: newId("theme"), name: String(parsed.name ?? "Imported"), tokens };
      customization.themes.push(imported);
      customization.theme = imported.id;
      importText = "";
    } catch {
      importError = "That is not a theme. Paste the JSON that Export copies.";
    }
  }

  function addExtension() {
    customization.extensions.push({
      id: newId("ext"),
      name: `Extension ${customization.extensions.length + 1}`,
      css: "",
      enabled: true,
    });
  }
</script>

<section class="block">
  <h3>Theme</h3>
  <div class="themes">
    {#each allThemes() as option (option.id)}
      <button
        class="theme"
        class:active={option.id === theme.id}
        onclick={() => (customization.theme = option.id)}>
        <span class="swatch">
          <span style="background: {option.tokens['chat-bg']}"></span>
          <span style="background: {option.tokens['bubble-mine']}"></span>
          <span style="background: {option.tokens.accent}"></span>
        </span>
        {option.name}
      </button>
    {/each}
  </div>

  <div class="row">
    {#if builtIn}
      <span class="hint">Built-in themes are read-only. Editing a colour makes a copy.</span>
    {:else}
      <input
        class="name"
        value={theme.name}
        aria-label="Theme name"
        oninput={(e) => (theme.name = e.currentTarget.value)} />
      <button class="ghost" onclick={() => removeTheme(theme.id)}>Delete</button>
    {/if}
    <button class="ghost" onclick={() => duplicate(theme)}>Duplicate</button>
    <button class="ghost" onclick={exportTheme}>{copied ? "Copied" : "Export"}</button>
  </div>
</section>

{#each groups as group (group)}
  <section class="block">
    <h3>{group}</h3>
    <div class="tokens">
      {#each TOKENS.filter((t) => t.group === group) as token (token.key)}
        {@const value = theme.tokens[token.key] ?? ""}
        <label class="token">
          <span>{token.label}</span>
          {#if HEX.test(value)}
            <input
              type="color"
              {value}
              aria-label="{token.label} colour"
              oninput={(e) => setToken(token.key, e.currentTarget.value)} />
          {/if}
          <input
            class="value"
            {value}
            spellcheck="false"
            onchange={(e) => setToken(token.key, e.currentTarget.value.trim())} />
        </label>
      {/each}
    </div>
  </section>
{/each}

<section class="block">
  <h3>Import a theme</h3>
  <textarea
    class="code"
    rows="3"
    placeholder={'{ "name": "…", "tokens": { "accent": "#00a884" } }'}
    bind:value={importText}></textarea>
  {#if importError}<p class="error-text">{importError}</p>{/if}
  <div class="row">
    <button class="ghost" disabled={!importText.trim()} onclick={importTheme}>Import</button>
  </div>
</section>

<section class="block">
  <h3>CSS extensions</h3>
  <p class="hint">
    Custom CSS, applied after the theme. Component styles are scoped, so use
    <code>!important</code> or a doubled class (<code>.bubble.bubble</code>) to override them.
  </p>
  {#each customization.extensions as extension, i (extension.id)}
    <div class="extension">
      <div class="row">
        <input type="checkbox" bind:checked={extension.enabled} aria-label="Enabled" />
        <input class="name" bind:value={extension.name} aria-label="Extension name" />
        <button
          class="icon-btn"
          title="Delete"
          aria-label="Delete {extension.name}"
          onclick={() => customization.extensions.splice(i, 1)}><Icon name="x" size={16} /></button>
      </div>
      <textarea
        class="code"
        rows="6"
        spellcheck="false"
        placeholder=".bubble.bubble {'{'} border-radius: 18px; {'}'}"
        bind:value={extension.css}></textarea>
    </div>
  {/each}
  <div class="row">
    <button class="ghost" onclick={addExtension}><Icon name="plus" size={15} /> Add extension</button>
  </div>
</section>

<style>
  .block {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  h3 {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    color: var(--muted);
  }
  .themes {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  .theme {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 12px 6px 6px;
    background: var(--bg);
    border: 1px solid var(--line-strong);
    border-radius: var(--radius);
    color: var(--text);
    font: inherit;
    cursor: pointer;
  }
  .theme.active {
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .swatch {
    display: flex;
    overflow: hidden;
    border-radius: 6px;
    border: 1px solid var(--line-strong);
  }
  .swatch span {
    width: 12px;
    height: 24px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .tokens {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
    gap: 6px 16px;
  }
  .token {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
  }
  .token span {
    flex: 1;
    min-width: 0;
  }
  input[type="color"] {
    width: 28px;
    height: 28px;
    padding: 0;
    border: 1px solid var(--line-strong);
    border-radius: 6px;
    background: none;
    cursor: pointer;
  }
  .value,
  .name {
    background: var(--bg);
    border: 1px solid var(--line-strong);
    border-radius: 6px;
    padding: 5px 8px;
    color: inherit;
    font: inherit;
    font-size: 12.5px;
  }
  .value {
    width: 120px;
    font-family: ui-monospace, Consolas, monospace;
  }
  .name {
    flex: 1;
    min-width: 120px;
  }
  .code {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
    background: var(--bg);
    border: 1px solid var(--line-strong);
    border-radius: 6px;
    padding: 8px 10px;
    color: inherit;
    font: 12.5px/1.5 ui-monospace, Consolas, monospace;
  }
  .extension {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 10px;
    background: var(--bg);
    border-radius: var(--radius);
  }
  .extension .name {
    background: var(--surface);
  }
  .extension .code {
    background: var(--surface);
  }
  .ghost {
    display: flex;
    align-items: center;
    gap: 6px;
    background: var(--raised);
    border: 1px solid var(--line-strong);
    border-radius: 6px;
    color: inherit;
    font: inherit;
    font-size: 13px;
    padding: 6px 12px;
    cursor: pointer;
  }
  .ghost:hover:not(:disabled) {
    background: var(--raised-2);
  }
  .ghost:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .icon-btn {
    display: grid;
    place-items: center;
    width: 30px;
    height: 30px;
    background: transparent;
    border: 0;
    border-radius: 6px;
    color: var(--muted);
    cursor: pointer;
  }
  .icon-btn:hover {
    background: var(--raised);
    color: var(--text);
  }
  input[type="checkbox"] {
    accent-color: var(--accent);
    width: 16px;
    height: 16px;
    margin: 0;
  }
  .hint {
    margin: 0;
    color: var(--muted);
    font-size: 12.5px;
  }
  code {
    font-family: ui-monospace, Consolas, monospace;
    font-size: 12px;
  }
  .error-text {
    margin: 0;
    color: var(--danger);
    font-size: 12.5px;
  }
</style>
