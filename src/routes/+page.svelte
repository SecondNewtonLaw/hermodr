<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";

  type StoredMessage = {
    chat: string;
    id: string;
    sender: string;
    sender_name: string | null;
    timestamp: number;
    from_me: boolean;
    text: string;
  };
  type ChatSummary = {
    chat: string;
    display_name: string | null;
    last_message_at: number;
    last_text: string;
    message_count: number;
  };
  type Retention = {
    max_age_hours: number | null;
    max_messages_per_chat: number | null;
  };
  type UiSettings = { retention: Retention; accept_full_history: boolean };
  type ConnectionState = { started: boolean; connected: boolean; qr: string | null };

  type ServiceEvent =
    | { kind: "qrCode"; value: string }
    | { kind: "connected" }
    | { kind: "disconnected" }
    | { kind: "message"; value: StoredMessage }
    | { kind: "retentionApplied"; removed: number };

  let connected = $state(false);
  let connecting = $state(false);
  let started = $state(false);
  let qr = $state<string | null>(null);
  let qrSvg = $state<string | null>(null);
  let chats: ChatSummary[] = $state([]);
  let selectedChat = $state<string | null>(null);
  let messages: StoredMessage[] = $state([]);
  let draft = $state("");
  let settings: UiSettings = $state({
    retention: { max_age_hours: 24, max_messages_per_chat: 500 },
    accept_full_history: false,
  });
  let showSettings = $state(false);
  let error = $state<string | null>(null);

  /** Strip the server suffix, used when no name has been resolved. */
  function bareJid(jid: string) {
    return jid.replace(/@.*$/, "");
  }

  function chatLabel(chat: ChatSummary) {
    return chat.display_name || bareJid(chat.chat);
  }

  function senderLabel(message: StoredMessage) {
    return message.sender_name || bareJid(message.sender);
  }

  function formatTime(seconds: number) {
    return new Date(seconds * 1000).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
  }

  async function refreshChats() {
    try {
      chats = await invoke<ChatSummary[]>("chats");

      // Group subjects are not carried on messages, so ask the service to fill
      // in any that are still missing, then refresh only if it found some.
      // A failure here must not prevent the chat list from updating.
      try {
        const resolved = await invoke<number>("resolve_names");
        if (resolved > 0) {
          chats = await invoke<ChatSummary[]>("chats");
        }
      } catch {
        // Names are cosmetic; leave them unresolved rather than breaking refresh.
      }
    } catch (e) {
      error = String(e);
    }
  }

  function selectedChatLabel() {
    const found = chats.find((c) => c.chat === selectedChat);
    return found ? chatLabel(found) : selectedChat ? bareJid(selectedChat) : "";
  }

  async function openChat(chat: string) {
    selectedChat = chat;
    try {
      messages = await invoke<StoredMessage[]>("messages", { chat, limit: 200 });
    } catch (e) {
      error = String(e);
    }
  }

  async function send() {
    const text = draft.trim();
    if (!text || !selectedChat) return;
    draft = "";
    try {
      await invoke("send_text", { chat: selectedChat, text });
      // The store is authoritative; refetch rather than assuming.
      await openChat(selectedChat);
      await refreshChats();
    } catch (e) {
      error = String(e);
    }
  }

  /** Renders a pairing code, if one is current. */
  async function showQr(code: string | null) {
    qr = code;
    qrSvg = code ? await invoke<string>("qr_svg", { value: code }) : null;
  }

  /** Pulls the authoritative state, used after connecting and on mount. */
  async function syncState() {
    const state = await invoke<ConnectionState>("connection_state");
    started = state.started;
    connected = state.connected;
    await showQr(state.connected ? null : state.qr);
    if (connected) await refreshChats();
  }

  async function connect() {
    connecting = true;
    error = null;
    try {
      await invoke("connect");
      // The service may have paired already, or emitted its code before the
      // listener attached, so read the state rather than assuming.
      await syncState();
    } catch (e) {
      error = String(e);
    } finally {
      connecting = false;
    }
  }

  async function saveSettings() {
    await invoke("set_settings", { settings });
    showSettings = false;
  }

  // `onMount` must return its cleanup synchronously, so the async setup runs in
  // an inner function and the listener handle is captured for teardown.
  onMount(() => {
    let unlisten: (() => void) | undefined;

    async function setup() {
      settings = await invoke<UiSettings>("get_settings");
      await syncState();

      unlisten = await listen<ServiceEvent>("service-event", async (event) => {
      const payload = event.payload;
      switch (payload.kind) {
        case "qrCode":
          await showQr(payload.value);
          break;
        case "connected":
          connected = true;
          await showQr(null);
          await refreshChats();
          break;
        case "disconnected":
          connected = false;
          break;
        case "message":
          await refreshChats();
          if (payload.value.chat === selectedChat) {
            await openChat(payload.value.chat);
          }
          break;
        case "retentionApplied":
          if (payload.removed > 0) {
            await refreshChats();
            if (selectedChat) await openChat(selectedChat);
          }
          break;
        }
      });
    }

    setup();

    return () => unlisten?.();
  });
</script>

<svelte:head><title>Hermóðr</title></svelte:head>

{#if error}
  <div class="error" role="alert">{error}</div>
{/if}

{#if !connected}
  <div class="pairing">
    <h1>Hermóðr</h1>
    <p class="lede">
      Link this device from WhatsApp &rsaquo; Linked devices &rsaquo; Link a device.
    </p>

    {#if qrSvg}
      <div class="qr" aria-label="Pairing QR code">{@html qrSvg}</div>
      <p class="hint">The code refreshes automatically.</p>
    {:else if started || connecting}
      <!-- The service is running but has not issued a code yet. Showing the
           button again here would invite a second, pointless click. -->
      <p class="hint">Waiting for a pairing code…</p>
    {:else}
      <button class="primary" onclick={connect} disabled={connecting}>
        Start pairing
      </button>
    {/if}
  </div>
{:else}
  <div class="layout">
    <aside class="chats">
      <header>
        <span>Chats</span>
        <button class="icon" title="Settings" onclick={() => (showSettings = !showSettings)}>
          ⚙
        </button>
      </header>
      <ul>
        {#each chats as chat (chat.chat)}
          <li>
            <button
              class:active={chat.chat === selectedChat}
              onclick={() => openChat(chat.chat)}
            >
              <span class="name">{chatLabel(chat)}</span>
              <span class="preview">{chat.last_text}</span>
              <span class="time">{formatTime(chat.last_message_at)}</span>
            </button>
          </li>
        {/each}
        {#if chats.length === 0}
          <li class="empty">No conversations yet.</li>
        {/if}
      </ul>
    </aside>

    <section class="conversation">
      {#if selectedChat}
        <header>{selectedChatLabel()}</header>
        <div class="messages">
          {#each messages.slice().reverse() as message (message.id)}
            <div class="bubble" class:mine={message.from_me}>
              {#if !message.from_me}
                <span class="sender">{senderLabel(message)}</span>
              {/if}
              <span class="text">{message.text}</span>
              <span class="meta">{formatTime(message.timestamp)}</span>
            </div>
          {/each}
        </div>
        <form class="composer" onsubmit={(e) => (e.preventDefault(), send())}>
          <input bind:value={draft} placeholder="Type a message" autocomplete="off" />
          <button class="primary" type="submit" disabled={!draft.trim()}>Send</button>
        </form>
      {:else}
        <div class="placeholder">Select a conversation</div>
      {/if}
    </section>
  </div>
{/if}

{#if showSettings}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div
    class="sheet-backdrop"
    role="presentation"
    onclick={() => (showSettings = false)}
  >
    <div class="sheet" role="dialog" aria-modal="true" aria-label="Retention settings">
      <h2>Retention</h2>
      <p class="hint">
        History is kept locally only for the window below. Nothing older is
        downloaded during pairing unless you opt in.
      </p>

      <label>
        <span>Keep messages for (hours)</span>
        <input
          type="number"
          min="1"
          value={settings.retention.max_age_hours ?? ""}
          oninput={(e) =>
            (settings.retention.max_age_hours = e.currentTarget.value
              ? Number(e.currentTarget.value)
              : null)}
        />
      </label>

      <label>
        <span>Max messages per chat</span>
        <input
          type="number"
          min="1"
          value={settings.retention.max_messages_per_chat ?? ""}
          oninput={(e) =>
            (settings.retention.max_messages_per_chat = e.currentTarget.value
              ? Number(e.currentTarget.value)
              : null)}
        />
      </label>

      <label class="check">
        <input type="checkbox" bind:checked={settings.accept_full_history} />
        <span>Download full history on next pairing</span>
      </label>

      <div class="actions">
        <button class="primary" onclick={saveSettings}>Save</button>
        <button onclick={() => (showSettings = false)}>Cancel</button>
      </div>
    </div>
  </div>
{/if}

<style>
  :global(html, body) {
    margin: 0;
    height: 100%;
    background: #111214;
    color: #e4e4e7;
    font-family: system-ui, sans-serif;
    font-size: 14px;
  }
  :global(#app) {
    height: 100%;
  }
  .error {
    background: #7f1d1d;
    padding: 8px 12px;
    font-size: 13px;
  }
  .pairing {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 14px;
    padding: 24px;
    text-align: center;
  }
  .pairing h1 {
    margin: 0;
    font-size: 22px;
  }
  .lede {
    margin: 0;
    color: #a1a1aa;
    max-width: 44ch;
    line-height: 1.5;
  }
  .hint {
    margin: 0;
    color: #71717a;
    font-size: 12px;
    max-width: 44ch;
  }
  .qr {
    background: #111214;
    padding: 12px;
    border-radius: 10px;
    line-height: 0;
  }
  .layout {
    display: grid;
    grid-template-columns: 300px 1fr;
    height: 100%;
  }
  .chats {
    border-right: 1px solid #27272a;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .chats header,
  .conversation header {
    padding: 12px 14px;
    font-weight: 600;
    border-bottom: 1px solid #27272a;
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .chats ul {
    list-style: none;
    margin: 0;
    padding: 0;
    overflow-y: auto;
    flex: 1;
  }
  .chats button {
    width: 100%;
    display: grid;
    grid-template-columns: 1fr auto;
    grid-template-areas: "name time" "preview preview";
    gap: 2px 8px;
    text-align: left;
    background: transparent;
    color: inherit;
    border: 0;
    padding: 10px 14px;
    cursor: pointer;
    border-bottom: 1px solid #1c1c1f;
    font: inherit;
  }
  .chats button:hover {
    background: #1c1c1f;
  }
  .chats button.active {
    background: #27272a;
  }
  .name {
    grid-area: name;
    font-weight: 500;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .time {
    grid-area: time;
    color: #71717a;
    font-size: 11px;
  }
  .preview {
    grid-area: preview;
    color: #a1a1aa;
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .empty {
    padding: 16px 14px;
    color: #71717a;
  }
  .conversation {
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .messages {
    flex: 1;
    overflow-y: auto;
    padding: 14px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .bubble {
    max-width: 70%;
    align-self: flex-start;
    background: #27272a;
    border-radius: 8px;
    padding: 6px 10px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    word-break: break-word;
  }
  .bubble.mine {
    align-self: flex-end;
    background: #14532d;
  }
  .sender {
    font-size: 11px;
    color: #86efac;
  }
  .meta {
    font-size: 10px;
    color: #a1a1aa;
    align-self: flex-end;
  }
  .placeholder {
    margin: auto;
    color: #71717a;
  }
  .composer {
    display: flex;
    gap: 8px;
    padding: 10px 14px;
    border-top: 1px solid #27272a;
  }
  .composer input {
    flex: 1;
    background: #1c1c1f;
    border: 1px solid #3f3f46;
    border-radius: 6px;
    padding: 8px 10px;
    color: inherit;
    font: inherit;
  }
  .primary {
    background: #2563eb;
    color: white;
    border: 0;
    border-radius: 6px;
    padding: 8px 16px;
    cursor: pointer;
    font: inherit;
  }
  .primary:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .icon {
    background: transparent;
    border: 0;
    color: inherit;
    cursor: pointer;
    font-size: 15px;
  }
  .sheet-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .sheet {
    background: #1c1c1f;
    border: 1px solid #3f3f46;
    border-radius: 10px;
    padding: 18px 20px;
    width: min(420px, 90vw);
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .sheet h2 {
    margin: 0;
    font-size: 15px;
  }
  .sheet label {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
    font-size: 13px;
  }
  .sheet label.check {
    justify-content: flex-start;
  }
  .sheet input[type="number"] {
    width: 100px;
    background: #111214;
    border: 1px solid #3f3f46;
    border-radius: 4px;
    padding: 4px 8px;
    color: inherit;
    font: inherit;
  }
  .actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
  }
  .actions button:not(.primary) {
    background: #3f3f46;
    color: inherit;
    border: 0;
    border-radius: 6px;
    padding: 8px 16px;
    cursor: pointer;
    font: inherit;
  }
</style>
