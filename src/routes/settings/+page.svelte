<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";

  type Settings = {
    conservative_threshold: number;
    strict_threshold: number;
    kill_threshold: number;
    unload_inactive_tabs: boolean;
  };

  let settings: Settings | null = $state(null);
  let saved = $state(false);

  onMount(async () => {
    settings = await invoke<Settings>("get_settings");
  });

  async function save() {
    if (!settings) return;
    await invoke("set_settings", { settings });
    saved = true;
    setTimeout(() => (saved = false), 1500);
  }
</script>

<main>
  <h1>Memory</h1>
  <p class="hint">
    WebKit releases cached message data once process memory passes these levels.
    Lower values reclaim more aggressively at the cost of reloading chats.
  </p>

  {#if settings}
    <label>
      <span>Conservative (0-1)</span>
      <input
        type="number"
        min="0.1"
        step="0.05"
        max="1"
        bind:value={settings.conservative_threshold}
      />
    </label>
    <label>
      <span>Strict (0-1)</span>
      <input
        type="number"
        min="0.1"
        step="0.05"
        max="1"
        bind:value={settings.strict_threshold}
      />
    </label>
    <label>
      <span>Kill threshold (0-1)</span>
      <input
        type="number"
        min="0.1"
        step="0.05"
        max="1"
        bind:value={settings.kill_threshold}
      />
    </label>

    <div class="actions">
      <button onclick={save}>Save</button>
      {#if saved}<span class="saved">Saved</span>{/if}
    </div>
  {/if}
</main>

<style>
  :global(body) {
    margin: 0;
    background: #18181b;
    color: #e4e4e7;
    font-family: system-ui, sans-serif;
    font-size: 14px;
  }
  main {
    padding: 20px 24px;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  h1 {
    margin: 0;
    font-size: 15px;
  }
  .hint {
    margin: 0;
    font-size: 12px;
    color: #a1a1aa;
    line-height: 1.45;
  }
  label {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
  }
  input {
    width: 90px;
    background: #27272a;
    color: inherit;
    border: 1px solid #3f3f46;
    border-radius: 4px;
    padding: 4px 8px;
    font: inherit;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 4px;
  }
  button {
    background: #3f3f46;
    color: inherit;
    border: 0;
    border-radius: 4px;
    padding: 6px 16px;
    cursor: pointer;
    font: inherit;
  }
  button:hover {
    background: #52525b;
  }
  .saved {
    font-size: 12px;
    color: #4ade80;
  }
</style>
