<script lang="ts">
  import { onMount } from "svelte";
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";

  type StoredMessage = {
    chat: string;
    id: string;
    sender: string;
    sender_name: string | null;
    timestamp: number;
    from_me: boolean;
    text: string;
    media_kind: string | null;
    media_path: string | null;
    reply_to_id: string | null;
    reply_to_text: string | null;
    reply_to_sender: string | null;
    read: boolean;
    revoked: boolean;
    status: string | null;
  };
  type ChatSummary = {
    chat: string;
    display_name: string | null;
    last_message_at: number;
    last_text: string;
    message_count: number;
    unread_count: number;
  };
  type Retention = {
    max_age_hours: number | null;
    max_messages_per_chat: number | null;
  };
  type UiSettings = {
    retention: Retention;
    accept_full_history: boolean;
    media_dir: string | null;
  };
  type ConnectionState = { started: boolean; connected: boolean; qr: string | null };

  type ServiceEvent =
    | { kind: "qrCode"; code: string }
    | { kind: "connected" }
    | { kind: "disconnected" }
    | { kind: "message"; message: StoredMessage }
    | { kind: "retentionApplied"; removed: number };

  let connected = $state(false);
  let connecting = $state(false);
  let started = $state(false);
  let qrSvg = $state<string | null>(null);
  let chats: ChatSummary[] = $state([]);
  let selectedChat = $state<string | null>(null);
  let messages: StoredMessage[] = $state([]);
  let draft = $state("");
  let replyingTo: StoredMessage | null = $state(null);
  let settings: UiSettings = $state({
    retention: { max_age_hours: 24, max_messages_per_chat: 500 },
    accept_full_history: false,
    media_dir: null,
  });
  let showSettings = $state(false);
  let error = $state<string | null>(null);

  let scroller: HTMLDivElement | undefined = $state();
  /** True while the user is reading older messages with new ones below. */
  let scrolledUp = $state(false);
  let filePicker: HTMLInputElement | undefined = $state();
  /** Attachment staged for review before it is sent. */
  let pending: { file: File; url: string; isImage: boolean; isVideo: boolean } | null = $state(null);
  let caption = $state("");

  function bareJid(jid: string) {
    return jid.replace(/@.*$/, "");
  }
  function chatLabel(chat: ChatSummary) {
    return chat.display_name || bareJid(chat.chat);
  }
  function senderLabel(message: StoredMessage) {
    return message.sender_name || bareJid(message.sender);
  }
  /** Resolves a JID to a known name, falling back to the bare address. */
  function senderName(jid: string) {
    const known = messages.find((m) => m.sender === jid && m.sender_name);
    return known?.sender_name || bareJid(jid);
  }
  /** Human-readable delivery state for a message we sent. */
  function statusMark(status: string | null) {
    switch (status) {
      case "pending":
        return "🕓";
      case "sent":
        return "✓";
      case "delivered":
        return "✓✓";
      case "read":
        return "✓✓";
      default:
        return "";
    }
  }

  function formatTime(seconds: number) {
    return new Date(seconds * 1000).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
  }

  /** Chat list only: cheap, local, never blocks on the network. */
  async function refreshChats() {
    try {
      chats = await invoke<ChatSummary[]>("chats");
    } catch (e) {
      error = String(e);
    }
  }

  /**
   * Group subjects need a network query. Runs after the list is already
   * rendered so a slow query cannot delay showing new messages.
   */
  async function resolveNames() {
    try {
      const resolved = await invoke<number>("resolve_names");
      if (resolved > 0) await refreshChats();
    } catch {
      // Names are cosmetic.
    }
  }

  async function openChat(chat: string) {
    selectedChat = chat;
    scrolledUp = false;
    try {
      messages = await invoke<StoredMessage[]>("messages", { chat, limit: 200 });
      // Opening a conversation is what marks it seen.
      await invoke("mark_read", { chat });
      await refreshChats();
      scrollToBottom();
    } catch (e) {
      error = String(e);
    }
  }

  /** Reloads the open conversation without touching the unread state. */
  async function reloadMessages() {
    if (!selectedChat) return;
    try {
      messages = await invoke<StoredMessage[]>("messages", {
        chat: selectedChat,
        limit: 200,
      });
    } catch (e) {
      error = String(e);
    }
  }

  function scrollToBottom() {
    // Wait for the new messages to render before measuring.
    requestAnimationFrame(() => {
      if (scroller) scroller.scrollTop = scroller.scrollHeight;
      scrolledUp = false;
    });
  }

  function onScroll() {
    if (!scroller) return;
    const distance = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    scrolledUp = distance > 120;
  }

  async function send() {
    const text = draft.trim();
    if (!text || !selectedChat) return;
    const reply = replyingTo;
    draft = "";
    replyingTo = null;
    try {
      if (reply) {
        await invoke("send_reply", {
          chat: selectedChat,
          text,
          replyToId: reply.id,
          replyToSender: reply.sender,
          replyToText: reply.text,
        });
      } else {
        await invoke("send_text", { chat: selectedChat, text });
      }
      await reloadMessages();
      await refreshChats();
      scrollToBottom();
    } catch (e) {
      error = String(e);
    }
  }

  /**
   * Builds a small preview image data URL.
   *
   * The full-size bitmap is never handed to the layout. Decoding a large photo
   * into the render tree is what killed the webview: pasting a screenshot
   * crashed the renderer and took the chat with it. Drawing a downscaled copy
   * keeps the decoded surface small.
   */
  async function imagePreview(file: File): Promise<string> {
    const bitmap = await createImageBitmap(file);
    const maxSide = 480;
    const scale = Math.min(1, maxSide / Math.max(bitmap.width, bitmap.height));
    const width = Math.max(1, Math.round(bitmap.width * scale));
    const height = Math.max(1, Math.round(bitmap.height * scale));

    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    const context = canvas.getContext("2d");
    if (!context) {
      bitmap.close();
      throw new Error("no 2d context for preview");
    }
    context.drawImage(bitmap, 0, 0, width, height);
    bitmap.close();
    return canvas.toDataURL("image/jpeg", 0.7);
  }

  /** Stages a file for review rather than sending it straight away. */
  async function stageFile(file: File) {
    try {
      // Release the previous preview before replacing it.
      if (pending?.url.startsWith("blob:")) URL.revokeObjectURL(pending.url);

      const isImage = file.type.startsWith("image/");
      const isVideo = file.type.startsWith("video/");

      let url: string;
      if (isImage) {
        url = await imagePreview(file);
      } else if (isVideo) {
        // A video element needs a real URL; it is not decoded until played.
        url = URL.createObjectURL(file);
      } else {
        url = "";
      }

      pending = { file, url, isImage, isVideo };
      caption = "";
    } catch (e) {
      // Staging must never take the chat down with it.
      pending = null;
      error = `Could not preview that file: ${e}`;
    }
  }

  function attach(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = "";
    if (file) void stageFile(file);
  }

  /** Pasting an image into the composer stages it, like attaching. */
  function onPaste(event: ClipboardEvent) {
    const items = event.clipboardData?.items;
    if (!items) return;
    const item = Array.from(items).find((i) => i.type.startsWith("image/"));
    const file = item?.getAsFile();
    if (!file) return;
    // Only intercept when it is actually an image; text paste stays native.
    event.preventDefault();
    void stageFile(file);
  }

  function cancelPending() {
    if (pending?.url.startsWith("blob:")) URL.revokeObjectURL(pending.url);
    pending = null;
    caption = "";
  }

  async function sendPending() {
    if (!pending || !selectedChat) return;
    const { file } = pending;
    try {
      const buffer = new Uint8Array(await file.arrayBuffer());
      // Base64 keeps the payload a single IPC value. Fine for the images and
      // documents a picker is normally used for.
      let binary = "";
      for (let i = 0; i < buffer.length; i += 0x8000) {
        binary += String.fromCharCode(...buffer.subarray(i, i + 0x8000));
      }
      await invoke("send_media", {
        chat: selectedChat,
        name: file.name,
        data: btoa(binary),
        caption: caption.trim() || null,
      });
      cancelPending();
      await reloadMessages();
      await refreshChats();
      scrollToBottom();
    } catch (e) {
      error = String(e);
    }
  }

  async function showQr(code: string | null) {
    qrSvg = code ? await invoke<string>("qr_svg", { value: code }) : null;
  }

  async function syncState() {
    const state = await invoke<ConnectionState>("connection_state");
    started = state.started;
    connected = state.connected;
    await showQr(state.connected ? null : state.qr);
    if (connected) await refreshChats();
  }

  /** Connects, reusing a stored session when there is one. */
  async function connect() {
    connecting = true;
    error = null;
    try {
      await invoke("connect");
      await syncState();
      resolveNames();
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

  onMount(() => {
    let unlisten: (() => void) | undefined;

    // Surface anything that escapes a handler, so a failure shows a message
    // rather than leaving the interface silently unresponsive.
    const onError = (event: ErrorEvent) => {
      error = event.message || "Unexpected error";
    };
    const onRejection = (event: PromiseRejectionEvent) => {
      error = String(event.reason ?? "Unexpected error");
    };
    window.addEventListener("error", onError);
    window.addEventListener("unhandledrejection", onRejection);

    async function setup() {
      settings = await invoke<UiSettings>("get_settings");

      // The listener is attached before connecting so no event can be missed.
      unlisten = await listen<ServiceEvent>("service-event", async (event) => {
        const payload = event.payload;
        switch (payload.kind) {
          case "qrCode":
            await showQr(payload.code);
            break;
          case "connected":
            connected = true;
            await showQr(null);
            await refreshChats();
            resolveNames();
            break;
          case "disconnected":
            connected = false;
            break;
          case "message":
            // Refresh the list first (cheap), then the conversation, so the
            // open chat updates immediately rather than after a network query.
            await refreshChats();
            if (payload.message.chat === selectedChat) {
              await reloadMessages();
              scrollToBottom();
            }
            // A group seen for the first time has no name yet; look it up in
            // the background so the list stops showing a raw number.
            if (!payload.message.from_me) resolveNames();
            break;
          case "retentionApplied":
            if (payload.removed > 0) {
              await refreshChats();
              await reloadMessages();
            }
            break;
        }
      });

      // Reuse a stored session automatically: pairing is only needed the very
      // first time, so the button should never be shown to a paired account.
      await connect();
    }

    setup();

    return () => {
      unlisten?.();
      window.removeEventListener("error", onError);
      window.removeEventListener("unhandledrejection", onRejection);
    };
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
      <p class="hint">Connecting…</p>
    {:else}
      <button class="primary" onclick={connect}>Start pairing</button>
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
              <span class="time">{formatTime(chat.last_message_at)}</span>
              <span class="preview">{chat.last_text}</span>
              {#if chat.unread_count > 0}
                <span class="badge">{chat.unread_count > 99 ? "99+" : chat.unread_count}</span>
              {/if}
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
        <header>{chats.find((c) => c.chat === selectedChat)?.display_name ?? bareJid(selectedChat)}</header>

        <div class="messages" bind:this={scroller} onscroll={onScroll}>
          {#each messages.slice().reverse() as message (message.id)}
            <div class="bubble" class:mine={message.from_me}>
              {#if !message.from_me}
                <span class="sender">{senderLabel(message)}</span>
              {/if}

              {#if message.revoked}
                <span class="revoked">This message was deleted</span>
              {:else}
                {#if message.reply_to_text}
                  <span class="quote">
                    <span class="quote-author">
                      {message.reply_to_sender ? senderName(message.reply_to_sender) : "Message"}
                    </span>
                    <span class="quote-text">{message.reply_to_text}</span>
                  </span>
                {/if}

                {#if message.media_kind === "image" && message.media_path}
                <img class="media" src={convertFileSrc(message.media_path)} alt={message.text} />
                {:else if message.media_kind && message.media_path}
                  <a class="file" href={convertFileSrc(message.media_path)} target="_blank">
                    {message.text || message.media_kind}
                  </a>
                {:else}
                  <span class="text">{message.text}</span>
                {/if}
              {/if}

              <span class="meta">
                {#if !message.revoked}
                  <button
                    class="reply-btn"
                    title="Reply"
                    onclick={() => (replyingTo = message)}>↩</button
                  >
                {/if}
                {formatTime(message.timestamp)}
                {#if message.from_me}
                  <span
                    class="ticks"
                    class:read={message.status === "read"}
                    title={message.status ?? "pending"}>{statusMark(message.status)}</span
                  >
                {/if}
              </span>
            </div>
          {/each}
        </div>

        {#if scrolledUp}
          <button class="jump" onclick={scrollToBottom}>Jump to latest ↓</button>
        {/if}

        {#if replyingTo}
          <div class="reply-preview">
            <span>Replying to {senderLabel(replyingTo)}: {replyingTo.text}</span>
            <button class="icon" onclick={() => (replyingTo = null)}>×</button>
          </div>
        {/if}

        {#if pending}
          <div class="attach-preview">
            {#if pending.isImage}
              <img src={pending.url} alt="Attachment preview" />
            {:else if pending.isVideo}
              <!-- svelte-ignore a11y_media_has_caption -->
              <video src={pending.url} controls></video>
            {:else}
              <span class="file-name">{pending.file.name}</span>
            {/if}
            <input
              class="caption"
              bind:value={caption}
              placeholder="Add a caption"
              autocomplete="off"
            />
            <button class="primary" onclick={sendPending}>Send</button>
            <button class="icon" title="Cancel" onclick={cancelPending}>×</button>
          </div>
        {/if}

        <form class="composer" onsubmit={(e) => (e.preventDefault(), send())}>
          <button
            type="button"
            class="icon attach"
            title="Attach a file"
            onclick={() => filePicker?.click()}>📎</button
          >
          <input
            class="file-input"
            type="file"
            bind:this={filePicker}
            onchange={attach}
          />
          <input
            bind:value={draft}
            placeholder="Type a message"
            autocomplete="off"
            onpaste={onPaste}
          />
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
  <div class="sheet-backdrop" role="presentation" onclick={() => (showSettings = false)}>
    <div class="sheet" role="dialog" aria-modal="true" aria-label="Settings">
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

      <label class="stack">
        <span>Media download folder</span>
        <input
          type="text"
          placeholder="App data folder"
          value={settings.media_dir ?? ""}
          oninput={(e) => (settings.media_dir = e.currentTarget.value || null)}
        />
      </label>

      <label class="check">
        <input type="checkbox" bind:checked={settings.accept_full_history} />
        <span>Download full history on next pairing</span>
      </label>

      <p class="hint">Retention and folder changes apply the next time Hermóðr starts.</p>

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
    grid-template-columns: 1fr auto auto;
    grid-template-areas: "name time badge" "preview preview preview";
    gap: 2px 8px;
    text-align: left;
    background: transparent;
    color: inherit;
    border: 0;
    padding: 10px 14px;
    cursor: pointer;
    border-bottom: 1px solid #1c1c1f;
    font: inherit;
    align-items: center;
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
  .badge {
    grid-area: badge;
    background: #22c55e;
    color: #052e16;
    font-size: 11px;
    font-weight: 700;
    border-radius: 999px;
    padding: 1px 7px;
    min-width: 18px;
    text-align: center;
  }
  .empty {
    padding: 16px 14px;
    color: #71717a;
  }
  .conversation {
    display: flex;
    flex-direction: column;
    min-height: 0;
    position: relative;
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
    /* Without this a flex item refuses to shrink below its content, so a large
       image stretches the bubble instead of being scaled down to fit it. */
    min-width: 0;
    align-self: flex-start;
    background: #27272a;
    border-radius: 8px;
    padding: 6px 10px;
    display: flex;
    flex-direction: column;
    gap: 3px;
    word-break: break-word;
    overflow: hidden;
  }
  .bubble.mine {
    align-self: flex-end;
    background: #14532d;
  }
  .sender {
    font-size: 11px;
    color: #86efac;
  }
  .revoked {
    font-style: italic;
    color: #71717a;
  }
  .quote {
    font-size: 12px;
    color: #a1a1aa;
    border-left: 2px solid #52525b;
    padding-left: 6px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 40ch;
  }
  .media {
    /* Cap both axes: width keeps it inside the bubble, height stops a tall
       photo from filling the viewport. */
    max-width: 100%;
    max-height: 320px;
    width: auto;
    height: auto;
    object-fit: contain;
    border-radius: 6px;
    display: block;
  }
  .file {
    color: #93c5fd;
  }
  .meta {
    font-size: 10px;
    color: #a1a1aa;
    align-self: flex-end;
    display: flex;
    align-items: center;
    gap: 4px;
  }
  .reply-btn {
    background: transparent;
    border: 0;
    color: inherit;
    cursor: pointer;
    font-size: 12px;
    opacity: 0;
    padding: 0 2px;
  }
  .bubble:hover .reply-btn {
    opacity: 0.8;
  }
  .placeholder {
    margin: auto;
    color: #71717a;
  }
  .jump {
    position: absolute;
    bottom: 74px;
    right: 18px;
    background: #3f3f46;
    color: inherit;
    border: 0;
    border-radius: 999px;
    padding: 6px 14px;
    cursor: pointer;
    font: inherit;
    box-shadow: 0 2px 8px rgba(0, 0, 0, 0.4);
  }
  .attach-preview {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 14px;
    background: #1c1c1f;
    border-top: 1px solid #27272a;
  }
  .attach-preview img,
  .attach-preview video {
    max-height: 72px;
    max-width: 120px;
    border-radius: 6px;
    display: block;
  }
  .attach-preview .caption {
    flex: 1;
    background: #111214;
    border: 1px solid #3f3f46;
    border-radius: 6px;
    padding: 6px 10px;
    color: inherit;
    font: inherit;
  }
  .file-name {
    color: #a1a1aa;
    font-size: 13px;
  }
  .quote-author {
    display: block;
    font-weight: 600;
    color: #a1a1aa;
  }
  .quote-text {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ticks {
    font-size: 10px;
    letter-spacing: -2px;
  }
  .ticks.read {
    color: #38bdf8;
  }
  .reply-preview {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    padding: 6px 14px;
    background: #1c1c1f;
    border-top: 1px solid #27272a;
    font-size: 12px;
    color: #a1a1aa;
  }
  .reply-preview span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .composer {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 14px;
    border-top: 1px solid #27272a;
  }
  .composer > input:not(.file-input) {
    flex: 1;
    background: #1c1c1f;
    border: 1px solid #3f3f46;
    border-radius: 6px;
    padding: 8px 10px;
    color: inherit;
    font: inherit;
  }
  .file-input {
    display: none;
  }
  .attach {
    font-size: 16px;
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
    width: min(460px, 90vw);
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
  .sheet label.stack {
    flex-direction: column;
    align-items: stretch;
    gap: 4px;
  }
  .sheet input[type="number"],
  .sheet input[type="text"] {
    background: #111214;
    border: 1px solid #3f3f46;
    border-radius: 4px;
    padding: 4px 8px;
    color: inherit;
    font: inherit;
  }
  .sheet input[type="number"] {
    width: 100px;
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
