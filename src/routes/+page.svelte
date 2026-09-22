<script lang="ts">
  import { onMount, tick } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";

  type Tab = { id: string; name: string; zoom: number };
  type Settings = {
    conservative_threshold: number;
    strict_threshold: number;
    kill_threshold: number;
    unload_inactive_tabs: boolean;
  };
  type Payload = { tabs: Tab[]; selected: string; settings: Settings };
  type Toast = { text: string; id: number } | null;

  let tabs: Tab[] = $state([]);
  let selected = $state("");
  let dragging = $state<string | null>(null);
  let dropTarget = $state<string | null>(null);
  let editing = $state<string | null>(null);
  let editValue = $state("");
  let toast: Toast = $state(null);
  let settings: Settings = $state({
    conservative_threshold: 0.5,
    strict_threshold: 0.75,
    kill_threshold: 0.9,
    unload_inactive_tabs: true,
  });

  let editInput: HTMLInputElement | undefined = $state();
  let toastTimer: number | undefined;

  function notify(text: string) {
    toast = { text, id: Date.now() };
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = null), 1600);
  }

  async function refresh() {
    const payload = await invoke<Payload>("list_tabs");
    tabs = payload.tabs;
    selected = payload.selected;
    settings = payload.settings;
  }

  async function addTab() {
    await invoke("add_tab");
  }

  async function closeTab(id: string) {
    await invoke("close_tab", { id });
  }

  async function openSettings() {
    try {
      await invoke("open_settings");
    } catch (e) {
      notify(`Settings failed: ${e}`);
    }
  }

  function selectTab(id: string) {
    if (editing === id) return;
    invoke("select_tab", { id });
  }

  function startRename(tab: Tab) {
    editing = tab.id;
    editValue = tab.name;
    tick().then(() => {
      editInput?.focus();
      editInput?.select();
    });
  }

  async function commitRename() {
    const id = editing;
    if (!id) return;
    const name = editValue.trim();
    editing = null;
    if (name) await invoke("rename_tab", { id, name });
  }

  function cancelRename() {
    editing = null;
  }

  // Drag and drop -------------------------------------------------------
  function onDragStart(tab: Tab, event: DragEvent) {
    dragging = tab.id;
    event.dataTransfer?.setData("text/plain", tab.id);
  }

  function onDragOver(targetId: string, event: DragEvent) {
    if (!dragging || dragging === targetId) return;
    event.preventDefault();
    dropTarget = targetId;
  }

  function onDrop(targetId: string) {
    const fromId = dragging;
    dragging = null;
    dropTarget = null;
    if (!fromId || fromId === targetId) return;

    // Move `fromId` to sit where `targetId` currently is, computed against the
    // full list so the index is correct regardless of drag direction.
    const ids = tabs.map((t) => t.id);
    const fromIndex = ids.indexOf(fromId);
    const toIndex = ids.indexOf(targetId);
    if (fromIndex === -1 || toIndex === -1) return;

    ids.splice(fromIndex, 1);
    ids.splice(toIndex, 0, fromId);

    // Apply optimistically so the bar reorders immediately.
    const byId = new Map(tabs.map((t) => [t.id, t]));
    tabs = ids.map((id) => byId.get(id)!);

    invoke("reorder_tabs", { ids });
  }

  function onDragEnd() {
    dragging = null;
    dropTarget = null;
  }

  // Zoom ----------------------------------------------------------------
  async function zoomBy(delta: number) {
    if (!selected) return;
    const next = await invoke<number>("zoom_by", { id: selected, delta });
    notify(`Zoom ${Math.round(next * 100)}%`);
  }

  async function resetZoom() {
    if (!selected) return;
    await invoke("reset_zoom", { id: selected });
    notify("Zoom 100%");
  }

  function onKeydown(event: KeyboardEvent) {
    if (!(event.ctrlKey || event.metaKey)) return;
    const key = event.key;
    if (key === "t") {
      event.preventDefault();
      addTab();
    } else if (key === "w") {
      event.preventDefault();
      closeTab(selected);
    } else if (key === "=" || key === "+") {
      event.preventDefault();
      zoomBy(0.1);
    } else if (key === "-") {
      event.preventDefault();
      zoomBy(-0.1);
    } else if (key === "0") {
      event.preventDefault();
      resetZoom();
    } else if (key === ",") {
      event.preventDefault();
      openSettings();
    }
  }

  onMount(() => {
    refresh();
    const unlisten = listen("tabs-changed", refresh);
    const unlistenZoom = listen<number>("zoom-changed", (e) => {
      notify(`Zoom ${Math.round(e.payload * 100)}%`);
    });
    return () => {
      unlisten.then((fn) => fn());
      unlistenZoom.then((fn) => fn());
    };
  });
</script>

<svelte:window onkeydown={onKeydown} />

<nav class="bar">
  {#each tabs as tab (tab.id)}
    <div
      class="tab"
      class:active={tab.id === selected}
      class:dragging={dragging === tab.id}
      class:drop-target={dropTarget === tab.id}
      role="tab"
      aria-selected={tab.id === selected}
      tabindex="0"
      draggable={editing !== tab.id}
      ondragstart={(e) => onDragStart(tab, e)}
      ondragover={(e) => onDragOver(tab.id, e)}
      ondragleave={() => (dropTarget = null)}
      ondrop={() => onDrop(tab.id)}
      ondragend={onDragEnd}
      onclick={() => selectTab(tab.id)}
      onauxclick={(e) => {
        if (e.button === 1) {
          e.preventDefault();
          closeTab(tab.id);
        }
      }}
      onkeydown={(e) => {
        if (e.key === "Enter") selectTab(tab.id);
        if (e.key === "F2") startRename(tab);
      }}
      ondblclick={() => startRename(tab)}
    >
      {#if editing === tab.id}
        <input
          class="edit"
          bind:this={editInput}
          bind:value={editValue}
          onkeydown={(e) => {
            if (e.key === "Enter") commitRename();
            if (e.key === "Escape") cancelRename();
            e.stopPropagation();
          }}
          onblur={commitRename}
          onclick={(e) => e.stopPropagation()}
        />
      {:else}
        <span class="name">{tab.name}</span>
        <button
          class="close"
          aria-label="Close tab"
          onclick={(e) => {
            e.stopPropagation();
            closeTab(tab.id);
          }}>×</button
        >
      {/if}
    </div>
  {/each}
  <button class="add" aria-label="Add account" onclick={addTab}>+</button>
  <button
    class="add gear"
    aria-label="Settings"
    onclick={openSettings}>⚙</button
  >
</nav>

{#if toast}
  <div class="toast">{toast.text}</div>
{/if}

<style>
  :global(html, body) {
    margin: 0;
    height: 100%;
    overflow: hidden;
    background: #18181b;
    font-family: system-ui, sans-serif;
  }
  .bar {
    display: flex;
    align-items: stretch;
    height: 40px;
    background: #18181b;
    color: #e4e4e7;
    user-select: none;
    overflow-x: auto;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 0 8px 0 12px;
    font-size: 13px;
    background: #27272a;
    border-right: 1px solid #18181b;
    cursor: pointer;
    white-space: nowrap;
    border-left: 2px solid transparent;
  }
  .tab.active {
    background: #3f3f46;
  }
  .tab.dragging {
    opacity: 0.5;
  }
  .tab.drop-target {
    border-left-color: #3b82f6;
  }
  .name {
    max-width: 16ch;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .edit {
    font: inherit;
    color: inherit;
    background: #18181b;
    border: 1px solid #3b82f6;
    border-radius: 3px;
    padding: 2px 4px;
    width: 12ch;
    outline: none;
  }
  .close {
    border: 0;
    background: transparent;
    color: inherit;
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    padding: 2px 4px;
    border-radius: 4px;
  }
  .close:hover,
  .add:hover {
    background: #52525b;
  }
  .add {
    border: 0;
    background: #27272a;
    color: inherit;
    font-size: 18px;
    padding: 0 14px;
    cursor: pointer;
  }
  .gear {
    font-size: 14px;
  }
  .toast {
    position: fixed;
    left: 50%;
    bottom: 14px;
    transform: translateX(-50%);
    background: #3f3f46;
    color: #fafafa;
    padding: 6px 14px;
    border-radius: 999px;
    font-size: 13px;
    pointer-events: none;
    z-index: 10;
  }
</style>
