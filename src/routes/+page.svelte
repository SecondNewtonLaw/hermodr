<script lang="ts">
  import { onMount, tick } from "svelte";
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import AudioPlayer from "$lib/AudioPlayer.svelte";

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
    media_thumb: string | null;
    reply_to_id: string | null;
    reply_to_text: string | null;
    reply_to_sender: string | null;
    reply_to_kind: string | null;
    reply_to_thumb: string | null;
    read: boolean;
    revoked: boolean;
    mentioned: boolean;
    preview_url: string | null;
    preview_title: string | null;
    preview_desc: string | null;
    preview_thumb: string | null;
    status: string | null;
  };
  type ChatSummary = {
    chat: string;
    display_name: string | null;
    last_message_at: number;
    last_text: string;
    last_from_me: boolean;
    last_sender_name: string | null;
    message_count: number;
    unread_count: number;
    mention_count: number;
    pinned: boolean;
  };
  type Account = { id: string; label: string };
  type SearchResult = {
    jid: string;
    name: string;
    number: string;
    kind: string;
    saved: boolean;
    has_messages: boolean;
  };
  type GroupInfo = {
    subject: string | null;
    description: string | null;
    created_at: number | null;
    participants: {
      jid: string;
      name: string;
      admin: boolean;
      number: string | null;
      username: string | null;
    }[];
  };
  type Retention = {
    max_age_hours: number | null;
    max_messages_per_chat: number | null;
  };
  type UiSettings = {
    retention: Retention;
    accept_full_history: boolean;
    auto_download_media: boolean;
    warn_missing_video_preview: boolean;
    media_dir: string | null;
  };
  type ConnectionState = { started: boolean; connected: boolean; qr: string | null };

  /** A file staged in the composer, before it is sent. */
  type PendingMedia = {
    id: number;
    file: File;
    url: string;
    kind: "image" | "video" | "other";
    caption: string;
  };

  type ServiceEvent =
    | { kind: "qrCode"; code: string }
    | { kind: "connected" }
    | { kind: "disconnected" }
    | { kind: "message"; message: StoredMessage }
    | { kind: "retentionApplied"; removed: number }
    | { kind: "namesUpdated"; count: number }
    | { kind: "syncing"; pending: number }
    | { kind: "synced" };

  let connected = $state(false);
  let connecting = $state(false);
  let started = $state(false);
  let qrSvg = $state<string | null>(null);
  let chats: ChatSummary[] = $state([]);
  let selectedChat = $state<string | null>(null);
  let messages: StoredMessage[] = $state([]);
  /** Offline-backlog progress: how many were announced and how many arrived. */
  let syncPending = $state(0);
  let syncSeen = $state(0);
  let syncPercent = $derived(
    syncPending > 0 ? Math.min(100, Math.round((syncSeen / syncPending) * 100)) : 0,
  );
  let draft = $state("");
  /** Per-chat composer text, so switching chats does not lose what was typed. */
  let drafts: Record<string, string> = $state({});
  let composerInput: HTMLTextAreaElement | undefined = $state();
  /** Group members for the @ autocomplete. */
  let participants: { jid: string; name: string }[] = $state([]);
  /** Open mention query, or null while the autocomplete is closed. */
  let mentionQuery = $state<string | null>(null);
  /** Unread mentions in the open chat, oldest first, for jump-to-mention. */
  let accountList: Account[] = $state([]);
  let activeAccount = $state<string | null>(null);
  let showAccounts = $state(false);
  /** A transient notice, such as a video sent without a preview. */
  let notice = $state<string | null>(null);
  let searchQuery = $state("");
  let searchResults: SearchResult[] = $state([]);
  let titleOverride = $state<string | null>(null);
  let mentionQueue: string[] = $state([]);
  /** Message briefly outlined after a jump, so it is easy to spot. */
  let highlightedId = $state<string | null>(null);
  let mentionCursor = $state(0);
  let mentionIndex = $state(0);
  /** Mentions picked from the autocomplete, used to convert the text on send. */
  let chosenMentions: { name: string; jid: string }[] = $state([]);
  let mentionMatches = $derived.by(() => {
    const query = mentionQuery;
    if (query === null) return [];
    const needle = query.toLowerCase();
    // `@all` is a group mention (respects mutes); `@all-override` also lists
    // every member, which notifies them even with the chat muted.
    const all = selectedChat?.endsWith("@g.us")
      ? [
          { jid: "@all", name: "all" },
          { jid: "@all-override", name: "all-override" },
        ]
      : [];
    return [...all, ...participants]
      .filter((p) => p.name.toLowerCase().includes(needle))
      .slice(0, 8);
  });
  let replyingTo: StoredMessage | null = $state(null);
  let settings: UiSettings = $state({
    retention: { max_age_hours: 24, max_messages_per_chat: 500 },
    accept_full_history: false,
    auto_download_media: true,
    warn_missing_video_preview: true,
    media_dir: null,
  });
  let showSettings = $state(false);
  let showGroupInfo = $state(false);
  let groupInfo: GroupInfo | null = $state(null);
  let groupInfoError = $state<string | null>(null);
  /** Sidebar widths, adjustable by dragging their edges. */
  let leftWidth = $state(300);
  let rightWidth = $state(320);
  let layoutColumns = $derived(
    showGroupInfo ? `${leftWidth}px 1fr ${rightWidth}px` : `${leftWidth}px 1fr`,
  );

  function startResize(side: "left" | "right", event: MouseEvent) {
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = side === "left" ? leftWidth : rightWidth;
    const onMove = (e: MouseEvent) => {
      const delta = e.clientX - startX;
      const next = side === "left" ? startWidth + delta : startWidth - delta;
      const clamped = Math.max(180, Math.min(640, next));
      if (side === "left") leftWidth = clamped;
      else rightWidth = clamped;
    };
    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }
  let error = $state<string | null>(null);

  let scroller: HTMLDivElement | undefined = $state();
  /** True while the user is reading older messages with new ones below. */
  let scrolledUp = $state(false);
  let filePicker: HTMLInputElement | undefined = $state();
  /** Files staged for review before they are sent, shown above the composer. */
  let pending: PendingMedia[] = $state([]);
  /** Id of the staged file whose preview/caption sheet is open. */
  let previewId = $state<number | null>(null);
  let pendingSeq = 0;
  let previewItem = $derived(pending.find((p) => p.id === previewId) ?? null);

  function bareJid(jid: string) {
    return jid.replace(/@.*$/, "");
  }
  function chatLabel(chat: ChatSummary) {
    return chat.display_name || bareJid(chat.chat);
  }
  /** Preview line: our own messages are prefixed "You", group peers by name. */
  function chatPreview(chat: ChatSummary) {
    if (chat.last_from_me) return `You: ${chat.last_text}`;
    if (chat.chat.endsWith("@g.us") && chat.last_sender_name) {
      return `${chat.last_sender_name}: ${chat.last_text}`;
    }
    return chat.last_text;
  }
  function senderLabel(message: StoredMessage) {
    return message.sender_name || bareJid(message.sender);
  }
  /** Resolves a JID to a known name, falling back to the bare address. */
  function senderName(jid: string) {
    const known = messages.find((m) => m.sender === jid && m.sender_name);
    return known?.sender_name || bareJid(jid);
  }
  /** Author shown on a quote; our own messages read "You". */
  function quoteAuthor(jid: string | null) {
    if (!jid) return "Message";
    return jid === "@me" ? "You" : senderName(jid);
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

  async function openChat(chat: string, jumpToMention = false, label: string | null = null) {
    selectedChat = chat;
    titleOverride = label;
    scrolledUp = false;
    participants = [];
    chosenMentions = [];
    mentionQuery = null;
    draft = drafts[chat] ?? "";
    showGroupInfo = false;
    groupInfo = null;
    // Captured before the chat is marked read, since that clears them.
    try {
      mentionQueue = await invoke<string[]>("unread_mentions", { chat });
    } catch {
      mentionQueue = [];
    }
    mentionCursor = 0;
    try {
      messages = await invoke<StoredMessage[]>("messages", { chat, limit: 200 });
      // Opening a conversation is what marks it seen.
      await invoke("mark_read", { chat });
      await refreshChats();
      scrollToBottom();
    } catch (e) {
      error = String(e);
    }
    // Group members power the @ autocomplete; a one-to-one chat returns none.
    try {
      participants = await invoke<{ jid: string; name: string }[]>("participants", { chat });
    } catch {
      participants = [];
    }
    // Opening a chat is the obvious moment to start typing.
    await tick();
    if (jumpToMention && mentionQueue.length > 0) {
      mentionCursor = 1;
      scrollToMessage(mentionQueue[0]);
    }
    composerInput?.focus();
  }

  /** Splits text into plain runs and http(s) links, for rendering. */
  function linkParts(text: string): { text: string; url?: string }[] {
    const pattern = /(https?:\/\/[^\s<>()\[\]{}"']+)/g;
    const parts: { text: string; url?: string }[] = [];
    let last = 0;
    for (const match of text.matchAll(pattern)) {
      const at = match.index ?? 0;
      if (at > last) parts.push({ text: text.slice(last, at) });
      parts.push({ text: match[0], url: match[0] });
      last = at + match[0].length;
    }
    if (last < text.length) parts.push({ text: text.slice(last) });
    return parts;
  }

  function hostOf(url: string) {
    try {
      return new URL(url).hostname;
    } catch {
      return url;
    }
  }

  async function openUrl(url: string) {
    try {
      await invoke("open_url", { url });
    } catch (e) {
      error = String(e);
    }
  }

  /** Deletes downloaded media, keeping the messages. */
  async function flushMedia() {
    try {
      const removed = await invoke<number>("flush_media");
      notice =
        removed > 0
          ? `Removed media from ${removed} message(s).`
          : "There was no downloaded media to remove.";
      await refreshChats();
      if (selectedChat) await reloadMessages();
    } catch (e) {
      error = String(e);
    }
  }

  /** Stops showing the video-without-preview warning. */
  async function muteNotice() {
    settings.warn_missing_video_preview = false;
    try {
      await invoke("set_settings", { settings });
    } catch (e) {
      error = String(e);
    }
    notice = null;
  }

  async function loadAccounts() {
    const view = await invoke<{ accounts: Account[]; active: string | null }>("accounts");
    accountList = view.accounts;
    activeAccount = view.active;
  }

  /** Clears everything tied to the current account before switching. */
  function resetUi() {
    chats = [];
    messages = [];
    selectedChat = null;
    draft = "";
    drafts = {};
    pending = [];
    replyingTo = null;
    participants = [];
    chosenMentions = [];
    mentionQueue = [];
    groupInfo = null;
    showGroupInfo = false;
  }

  async function switchTo(id: string) {
    if (id === activeAccount) return;
    try {
      resetUi();
      connected = false;
      await showQr(null);
      await invoke("switch_account", { id });
      await loadAccounts();
      await syncState();
    } catch (e) {
      error = String(e);
    }
  }

  async function renameAccount(id: string, label: string) {
    try {
      await invoke("rename_account", { id, label });
      await loadAccounts();
    } catch (e) {
      error = String(e);
    }
  }

  async function removeAccount(id: string) {
    if (!window.confirm("Remove this account and its local data?")) return;
    try {
      if (id === activeAccount) {
        resetUi();
        connected = false;
        await showQr(null);
      }
      await invoke("remove_account", { id });
      await loadAccounts();
      await syncState();
    } catch (e) {
      error = String(e);
    }
  }

  async function addAccount() {
    try {
      resetUi();
      connected = false;
      await showQr(null);
      await invoke("add_account", {});
      await loadAccounts();
      await syncState();
    } catch (e) {
      error = String(e);
    }
  }

  /** Asks the phone for older messages in the open chat. */
  async function loadOlder() {
    if (!selectedChat) return;
    try {
      await invoke("load_older", { chat: selectedChat, count: 50 });
    } catch (e) {
      error = String(e);
    }
  }

  /** Runs the chat/contact/group search. */
  async function runSearch() {
    const query = searchQuery.trim();
    if (!query) {
      searchResults = [];
      return;
    }
    try {
      searchResults = await invoke<SearchResult[]>("search", { query });
    } catch (e) {
      error = String(e);
    }
  }

  /** Opens a search result, even one with no local history. */
  function openFromSearch(result: SearchResult) {
    searchQuery = "";
    searchResults = [];
    openChat(result.jid, false, result.name);
  }

  /** Label for a media message with no caption, used in reply previews. */
  function replyPreviewText(message: StoredMessage) {
    // Media stores a "[image]" style placeholder when it has no caption.
    if (message.text && !message.text.startsWith("[")) return message.text;
    switch (message.media_kind) {
      case "image":
        return "Photo";
      case "video":
        return "Video";
      case "audio":
        return "Voice message";
      case "document":
        return "Document";
      default:
        return "";
    }
  }

  function replyIcon(kind: string | null) {
    if (kind === "audio") return "\u{1F3B5}";
    if (kind === "video") return "\u{1F3AC}";
    if (kind === "document") return "\u{1F4C4}";
    return "\u{1F4CE}";
  }

  /** Scrolls a message into view by its id, and highlights it briefly. */
  function scrollToMessage(id: string) {
    const element = scroller?.querySelector(`[data-id="${id}"]`);
    if (!element) return;
    element.scrollIntoView({ block: "center" });
    highlightedId = id;
    window.setTimeout(() => {
      if (highlightedId === id) highlightedId = null;
    }, 1600);
  }

  /** Jumps to the next unread mention, oldest to newest, wrapping around. */
  function jumpNextMention() {
    if (mentionQueue.length === 0) return;
    const id = mentionQueue[mentionCursor % mentionQueue.length];
    mentionCursor = (mentionCursor + 1) % mentionQueue.length;
    scrollToMessage(id);
  }

  /** Pins or unpins a chat, mirrored to the account. */
  async function togglePin(chat: ChatSummary, event: MouseEvent) {
    event.stopPropagation();
    try {
      await invoke("set_pinned", { chat: chat.chat, pinned: !chat.pinned });
      await refreshChats();
    } catch (e) {
      error = String(e);
    }
  }

  /** Opens the right sidebar with the group's subject, description and members. */
  async function openGroupInfo() {
    if (!selectedChat) return;
    showGroupInfo = true;
    groupInfo = null;
    groupInfoError = null;
    try {
      groupInfo = await invoke<GroupInfo>("group_info", { chat: selectedChat });
    } catch (e) {
      // The query can time out on a busy server; keep the panel open so the
      // failure is visible and retryable rather than looking like a dead click.
      groupInfoError = String(e);
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

  /** The `@…` token immediately before the caret, if the user is typing one. */
  function currentMentionQuery(): { query: string; start: number } | null {
    const input = composerInput;
    if (!input) return null;
    const caret = input.selectionStart ?? draft.length;
    const before = draft.slice(0, caret);
    const at = before.lastIndexOf("@");
    if (at === -1) return null;
    if (at > 0 && !/\s/.test(before[at - 1])) return null;
    const query = before.slice(at + 1);
    if (/\s/.test(query)) return null;
    return { query, start: at };
  }

  function onComposerInput(event: Event) {
    draft = (event.currentTarget as HTMLTextAreaElement).value;
    if (selectedChat) drafts[selectedChat] = draft;
    const token = currentMentionQuery();
    if (token && participants.length > 0) {
      mentionQuery = token.query;
      mentionIndex = 0;
    } else {
      mentionQuery = null;
    }
    autoGrow();
  }

  async function selectMention(person: { jid: string; name: string }) {
    const input = composerInput;
    const token = currentMentionQuery();
    if (!input || !token) return;
    const caret = input.selectionStart ?? draft.length;
    draft = draft.slice(0, token.start) + `@${person.name} ` + draft.slice(caret);
    mentionQuery = null;
    if (!chosenMentions.some((m) => m.jid === person.jid)) {
      chosenMentions = [...chosenMentions, { name: person.name, jid: person.jid }];
    }
    await tick();
    const position = token.start + person.name.length + 2;
    input.focus();
    input.setSelectionRange(position, position);
  }

  function onComposerKey(event: KeyboardEvent) {
    if (mentionQuery !== null && mentionMatches.length > 0) {
      if (event.key === "ArrowDown") {
        event.preventDefault();
        mentionIndex = (mentionIndex + 1) % mentionMatches.length;
        return;
      }
      if (event.key === "ArrowUp") {
        event.preventDefault();
        mentionIndex = (mentionIndex - 1 + mentionMatches.length) % mentionMatches.length;
        return;
      }
      if (event.key === "Enter" || event.key === "Tab") {
        event.preventDefault();
        void selectMention(mentionMatches[mentionIndex]);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        mentionQuery = null;
        return;
      }
    }
    // Enter sends; Shift+Enter keeps the newline the textarea just added.
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void send();
    }
  }

  /** Grows the composer with its content, up to a few lines. */
  function autoGrow() {
    const el = composerInput;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 140)}px`;
  }

  /** Turns the display text into wire text, naming mentions by number. */
  function mentionPayload() {
    let text = draft.trim();
    const jids: string[] = [];
    for (const mention of chosenMentions) {
      const token = `@${mention.name}`;
      if (!text.includes(token)) continue;
      if (mention.jid === "@all") {
        jids.push("@all");
        continue;
      }
      if (mention.jid === "@all-override") {
        // Everyone is also listed explicitly, which bypasses their mute.
        text = text.replace("@all-override", "@all");
        jids.push("@all");
        for (const person of participants) jids.push(person.jid);
        continue;
      }
      const user = mention.jid.split("@")[0].split(":")[0];
      text = text.replace(token, `@${user}`);
      jids.push(mention.jid);
    }
    return { text, jids };
  }

  async function send() {
    if (!draft.trim() || !selectedChat) return;
    const { text, jids } = mentionPayload();
    const reply = replyingTo;
    draft = "";
    if (selectedChat) delete drafts[selectedChat];
    replyingTo = null;
    chosenMentions = [];
    mentionQuery = null;
    await tick();
    autoGrow();
    composerInput?.focus();
    try {
      if (reply) {
        await invoke("send_reply", {
          chat: selectedChat,
          text,
          replyToId: reply.id,
          replyToSender: reply.sender,
          replyToText: reply.text,
          mentions: jids,
        });
      } else {
        await invoke("send_text", { chat: selectedChat, text, mentions: jids });
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
      const kind = file.type.startsWith("image/")
        ? "image"
        : file.type.startsWith("video/")
          ? "video"
          : "other";

      let url = "";
      if (kind === "image") url = await imagePreview(file);
      else if (kind === "video") url = URL.createObjectURL(file);

      pending = [...pending, { id: pendingSeq++, file, url, kind, caption: "" }];
    } catch (e) {
      // Staging must never take the chat down with it.
      error = `Could not preview that file: ${e}`;
    }
  }

  function attach(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const files = Array.from(input.files ?? []);
    input.value = "";
    for (const file of files) void stageFile(file);
  }

  /** Media extensions that can be staged from a pasted file path. */
  const PASTABLE = /\.(jpe?g|png|gif|webp|mp4|mov|m4v|webm|mkv|ogg|opus|mp3|m4a|aac|wav)$/i;

  function mimeForName(name: string) {
    const extension = name.slice(name.lastIndexOf(".") + 1).toLowerCase();
    const table: Record<string, string> = {
      jpg: "image/jpeg",
      jpeg: "image/jpeg",
      png: "image/png",
      gif: "image/gif",
      webp: "image/webp",
      mp4: "video/mp4",
      mov: "video/mp4",
      m4v: "video/mp4",
      webm: "video/webm",
      mkv: "video/x-matroska",
      ogg: "audio/ogg",
      opus: "audio/ogg",
      mp3: "audio/mpeg",
      m4a: "audio/mp4",
      aac: "audio/mp4",
      wav: "audio/wav",
    };
    return table[extension] ?? "application/octet-stream";
  }

  /**
   * Reads a pasted file, either as clipboard bytes or from a copied file path.
   *
   * WebKitGTK does not put clipboard images in the paste event's
   * `clipboardData`; only the async clipboard API reaches them. Copying a file
   * in the file manager usually exposes just a `text/uri-list`, so that path is
   * read back through the shell (restricted to media extensions) instead.
   */
  async function clipboardFile(): Promise<File | null> {
    try {
      const items = await navigator.clipboard?.read();
      for (const item of items ?? []) {
        const media = item.types.find(
          (t) => t.startsWith("image/") || t.startsWith("video/") || t.startsWith("audio/"),
        );
        if (media) {
          const blob = await item.getType(media);
          // send_media classifies by file extension, so a pasted item needs a
          // real one or a photo goes out as a document.
          const sub = media.split("/")[1]?.split(";")[0] || "bin";
          const extension = sub === "jpeg" ? "jpg" : sub;
          return new File([blob], `pasted.${extension}`, { type: media });
        }
        if (item.types.includes("text/uri-list")) {
          const text = await (await item.getType("text/uri-list")).text();
          const uri = text
            .split("\n")
            .map((line) => line.trim())
            .find((line) => line.length > 0);
          if (!uri?.startsWith("file://")) continue;
          const path = decodeURIComponent(new URL(uri).pathname);
          if (!PASTABLE.test(path)) continue;
          const data = await invoke<string>("read_file", { path });
          const bytes = Uint8Array.from(atob(data), (c) => c.charCodeAt(0));
          return new File([bytes], path.split("/").pop() ?? "pasted", { type: mimeForName(path) });
        }
      }
    } catch {
      // Nothing readable; the caller falls back to a hint.
    }
    return null;
  }

  /**
   * Stages pasted image bytes and blocks the default paste otherwise.
   *
   * Pasting a *file* (copying it in the file manager) puts a `text/uri-list` on
   * the clipboard rather than an image. Without `preventDefault` WebKit then
   * navigates the whole webview to that URI. That navigation is fatal: wry's
   * page-load handler does `webview.uri().unwrap()`, the URI is absent for such
   * a load, and the panic aborts the process. So anything file-like is
   * swallowed; only real image bytes are staged, and plain text stays native.
   */
  async function onPaste(event: ClipboardEvent) {
    const data = event.clipboardData;
    const item = data
      ? Array.from(data.items).find(
          (i) => i.type.startsWith("image/") || i.type.startsWith("video/"),
        )
      : undefined;
    const isUriList = data ? Array.from(data.types).includes("text/uri-list") : false;
    const isPlainText =
      !item && !isUriList && !!data && Array.from(data.types).includes("text/plain");
    if (isPlainText) return;
    if (item || isUriList || data) event.preventDefault();

    const file = item?.getAsFile() ?? (await clipboardFile());
    if (file) void stageFile(file);
    else if (isUriList) error = "Could not read that file. Try the 📎 button.";
  }

  /** Dropping files stages them; dropping anything else must not navigate. */
  function onDrop(event: DragEvent) {
    event.preventDefault();
    for (const file of Array.from(event.dataTransfer?.files ?? [])) void stageFile(file);
  }

  function removePending(id: number) {
    const item = pending.find((p) => p.id === id);
    if (item?.url.startsWith("blob:")) URL.revokeObjectURL(item.url);
    pending = pending.filter((p) => p.id !== id);
    if (previewId === id) previewId = null;
  }

  function clearPending() {
    for (const item of pending) {
      if (item.url.startsWith("blob:")) URL.revokeObjectURL(item.url);
    }
    pending = [];
    previewId = null;
  }

  async function sendPending() {
    if (!selectedChat || pending.length === 0) return;
    const items = [...pending];
    const reply = replyingTo;
    try {
      for (const item of items) {
        const buffer = new Uint8Array(await item.file.arrayBuffer());
        // Base64 keeps the payload a single IPC value. Fine for the images and
        // documents a picker is normally used for.
        let binary = "";
        for (let i = 0; i < buffer.length; i += 0x8000) {
          binary += String.fromCharCode(...buffer.subarray(i, i + 0x8000));
        }
        const warning = await invoke<string | null>("send_media", {
          chat: selectedChat,
          name: item.file.name,
          data: btoa(binary),
          caption: item.caption.trim() || null,
          replyToId: reply?.id ?? null,
          replyToSender: reply?.sender ?? null,
          replyToText: reply?.text ?? null,
        });
        if (warning && settings.warn_missing_video_preview) notice = warning;
      }
      clearPending();
      replyingTo = null;
      await reloadMessages();
      await refreshChats();
      scrollToBottom();
    } catch (e) {
      error = String(e);
    }
  }

  /** Opens a downloaded media file in the desktop's default application. */
  async function openMedia(path: string) {
    try {
      await invoke("open_path", { path });
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

    // Typing anywhere lands in the composer, so a chat can be answered without
    // clicking the field first.
    const onAnyKey = (event: KeyboardEvent) => {
      if (!selectedChat || !composerInput) return;
      if (event.ctrlKey || event.metaKey || event.altKey) return;
      const target = event.target as HTMLElement | null;
      if (
        target &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)
      ) {
        return;
      }
      if (event.key.length !== 1) return;
      composerInput.focus();
    };
    window.addEventListener("keydown", onAnyKey);

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
            if (syncPending > 0) syncSeen += 1;
            // Refresh the list first (cheap), then the conversation, so the
            // open chat updates immediately rather than after a network query.
            await refreshChats();
            if (payload.message.chat === selectedChat) {
              await reloadMessages();
              // An incoming message must not yank the view down while reading.
              if (payload.message.from_me) scrollToBottom();
              // Seen while open, but only if the window is actually focused.
              if (!payload.message.from_me && document.hasFocus()) {
                await invoke("mark_read", { chat: selectedChat });
                await refreshChats();
              }
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
          case "namesUpdated":
            // Address-book names arrived after the initial fetch, so the cached
            // display names are stale until both lists reload.
            await refreshChats();
            await reloadMessages();
            break;
          case "syncing":
            syncPending = payload.pending;
            syncSeen = 0;
            break;
          case "synced":
            // The backlog is in; refresh so the lists include everything the
            // burst delivered.
            await refreshChats();
            await reloadMessages();
            syncPending = 0;
            syncSeen = 0;
            break;
        }
      });

      // Reuse a stored session automatically: pairing is only needed the very
      // first time, so the button should never be shown to a paired account.
      await connect();
      await loadAccounts();
    }

    setup();

    return () => {
      unlisten?.();
      window.removeEventListener("error", onError);
      window.removeEventListener("unhandledrejection", onRejection);
      window.removeEventListener("keydown", onAnyKey);
    };
  });
</script>

<svelte:head><title>Hermóðr</title></svelte:head>

<!-- Window-level so a paste/drop anywhere cannot navigate the webview. -->
<svelte:window
  onpaste={onPaste}
  ondragover={(e) => e.preventDefault()}
  ondrop={onDrop}
/>

{#if error}
  <div class="error" role="alert">{error}</div>
{/if}

{#if syncPending > 0}
  <div class="sync-banner">
    <span class="sync-text">Loading messages… {syncPercent}%</span>
    <div class="sync-track">
      <div class="sync-bar" style="width: {syncPercent}%"></div>
    </div>
  </div>
{/if}

{#if !connected}
  <div class="pairing">
    <h1>Hermóðr</h1>
    <p class="lede">
      Link this device from WhatsApp &rsaquo; Linked devices &rsaquo; Link a device.
    </p>

    {#if accountList.length > 0}
      <div class="account-bar">
        {#each accountList as account (account.id)}
          <button
            class="account"
            class:active={account.id === activeAccount}
            title={account.label}
            onclick={() => switchTo(account.id)}>{account.label}</button>
        {/each}
        <button
          class="account add"
          title="Manage accounts"
          onclick={() => (showAccounts = true)}>⋯</button>
      </div>
    {/if}

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
  <div class="layout" style="grid-template-columns: {layoutColumns}">
    <aside class="chats">
      <header>
        <span>Chats</span>
        <span class="account-bar">
          {#each accountList as account (account.id)}
            <button
              class="account"
              class:active={account.id === activeAccount}
              title={account.label}
              onclick={() => switchTo(account.id)}>{account.label}</button>
          {/each}
          <button class="account add" title="Add account" onclick={addAccount}>+</button>
          <button class="account add" title="Manage accounts" onclick={() => (showAccounts = true)}>
            ⋯
          </button>
        </span>
        <button class="icon" title="Settings" onclick={() => (showSettings = !showSettings)}>
          ⚙
        </button>
      </header>
      <input
        class="search"
        placeholder="Search chats and contacts"
        bind:value={searchQuery}
        oninput={runSearch}
        autocomplete="off"
      />
      {#if searchQuery.trim()}
        <ul class="results">
          {#each searchResults as result (result.jid)}
            <li>
              <div
                class="chat-row"
                role="button"
                tabindex="0"
                onclick={() => openFromSearch(result)}
                onkeydown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    openFromSearch(result);
                  }
                }}>
                <span class="name">
                  {#if result.kind === "group" || result.saved}
                    {result.name}
                  {:else}
                    {result.number}{result.name && result.name !== result.number
                      ? ` - ${result.name}`
                      : ""}
                  {/if}
                </span>
                <span class="preview">
                  {result.kind}{result.has_messages ? "" : " · no messages yet"}
                </span>
              </div>
            </li>
          {/each}
        </ul>
      {:else}
      <ul>
        {#each chats as chat (chat.chat)}
          <li>
            <div
              class="chat-row"
              class:active={chat.chat === selectedChat}
              role="button"
              tabindex="0"
              onclick={() => openChat(chat.chat)}
              onkeydown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  openChat(chat.chat);
                }
              }}>
              <span class="name">{#if chat.pinned}<span class="pin">📌</span>{/if}{chatLabel(chat)}</span>
              <span class="time">{formatTime(chat.last_message_at)}</span>
              <span class="preview">{chatPreview(chat)}</span>
              <span class="badges">
                {#if chat.mention_count > 0}
                  <button
                    class="badge mention"
                    title="Jump to mention"
                    onclick={(e) => {
                      e.stopPropagation();
                      openChat(chat.chat, true);
                    }}>@</button>
                {/if}
                {#if chat.unread_count > 0}
                  <span class="badge">{chat.unread_count > 99 ? "99+" : chat.unread_count}</span>
                {/if}
                <button
                  class="badge pin-toggle"
                  title={chat.pinned ? "Unpin" : "Pin"}
                  onclick={(e) => togglePin(chat, e)}>📌</button>
              </span>
            </div>
          </li>
        {/each}
        {#if chats.length === 0}
          <li class="empty">No conversations yet.</li>
        {/if}
      </ul>
      {/if}
      <button type="button" class="resizer" aria-label="Resize chat list" onmousedown={(e) => startResize("left", e)}></button>
    </aside>

    <section class="conversation">
      {#if selectedChat}
        <header>
          {#if selectedChat.endsWith("@g.us")}
            <button class="chat-title" title="Group info" onclick={openGroupInfo}>
              {chats.find((c) => c.chat === selectedChat)?.display_name ?? bareJid(selectedChat)}
            </button>
          {:else}
            {chats.find((c) => c.chat === selectedChat)?.display_name ??
              titleOverride ??
              bareJid(selectedChat)}
          {/if}
          {#if mentionQueue.length > 0}
            <button class="icon jump-mention" title="Jump to mention" onclick={jumpNextMention}>
              @ {mentionCursor}/{mentionQueue.length}
            </button>
          {/if}
        </header>

        <div class="messages" bind:this={scroller} onscroll={onScroll}>
          {#if messages.length > 0}
            <button class="load-older" onclick={loadOlder}>Load older messages</button>
          {/if}
          {#each messages.slice().reverse() as message (message.id)}
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <div
              class="bubble"
              class:mine={message.from_me}
              class:highlighted={message.id === highlightedId}
              data-id={message.id}
              ondblclick={() => (replyingTo = message)}>
              {#if !message.from_me && selectedChat?.endsWith("@g.us")}
                <span class="sender">{senderLabel(message)}</span>
              {/if}

              {#if message.revoked}
                <span class="revoked">This message was deleted</span>
              {:else}
                {#if message.reply_to_text}
                  <button
                    type="button"
                    class="quote"
                    title="Go to message"
                    onclick={() => message.reply_to_id && scrollToMessage(message.reply_to_id)}>
                    {#if message.reply_to_kind === "image" && message.reply_to_thumb}
                      <img
                        class="quote-thumb"
                        src={convertFileSrc(message.reply_to_thumb)}
                        alt=""
                      />
                    {:else if message.reply_to_kind}
                      <span class="quote-icon">{replyIcon(message.reply_to_kind)}</span>
                    {/if}
                    <span class="quote-author">
                      {quoteAuthor(message.reply_to_sender)}
                    </span>
                    <span class="quote-text">{message.reply_to_text}</span>
                  </button>
                {/if}

                {#if message.media_kind === "image" && (message.media_path || message.media_thumb)}
                  <button
                    class="media-button"
                    title="Open in image viewer"
                    onclick={() => openMedia((message.media_path ?? message.media_thumb)!)}>
                    <img
                      class="media"
                      src={convertFileSrc((message.media_path ?? message.media_thumb)!)}
                      alt={message.text}
                    />
                  </button>
                {:else if (message.media_kind === "video" || message.media_kind === "gif") &&
                (message.media_path || message.media_thumb)}
                  <button
                    class="media-button video"
                    title={message.media_kind === "gif" ? "Open GIF" : "Open in video player"}
                    onclick={() => message.media_path && openMedia(message.media_path)}>
                    {#if message.media_thumb}
                      <img class="media" src={convertFileSrc(message.media_thumb)} alt="" />
                    {/if}
                    <span class="media-overlay">{message.media_kind === "gif" ? "GIF" : "▶"}</span>
                  </button>
                {:else if message.media_kind === "audio" && message.media_path}
                  <AudioPlayer path={message.media_path} />
                {:else if message.media_kind && (message.media_path || message.media_thumb)}
                  <button
                    class="file"
                    onclick={() => message.media_path && openMedia(message.media_path)}>
                    {message.text || message.media_kind}
                  </button>
                {:else}
                  <span class="text"
                    >{#each linkParts(message.text) as part}{#if part.url}<a
                          class="link"
                          href={part.url}
                          onclick={(e) => {
                            e.preventDefault();
                            openUrl(part.url!);
                          }}>{part.text}</a
                        >{:else}{part.text}{/if}{/each}</span
                  >
                {/if}

                {#if message.preview_url}
                  <button
                    class="preview-card"
                    title="Open link"
                    onclick={() => openUrl(message.preview_url!)}>
                    {#if message.preview_thumb}
                      <img
                        class="preview-thumb"
                        src={convertFileSrc(message.preview_thumb)}
                        alt=""
                      />
                    {/if}
                    <span class="preview-body">
                      {#if message.preview_title}
                        <span class="preview-title">{message.preview_title}</span>
                      {/if}
                      {#if message.preview_desc}
                        <span class="preview-desc">{message.preview_desc}</span>
                      {/if}
                      <span class="preview-host">{hostOf(message.preview_url)}</span>
                    </span>
                  </button>
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
            {#if replyingTo.media_kind === "image" && replyingTo.media_path}
              <img
                class="reply-thumb"
                src={convertFileSrc(replyingTo.media_path)}
                alt=""
              />
            {:else if replyingTo.media_kind}
              <span class="reply-icon">{replyIcon(replyingTo.media_kind)}</span>
            {/if}
            <span
              >Replying to {senderLabel(replyingTo)}: {replyPreviewText(replyingTo)}</span
            >
            <button class="icon" onclick={() => (replyingTo = null)}>×</button>
          </div>
        {/if}

        {#if pending.length > 0}
          <div class="pending">
            {#each pending as item (item.id)}
              <div class="pending-item">
                <button
                  class="pending-thumb"
                  title="Preview and caption"
                  onclick={() => (previewId = item.id)}>
                  {#if item.kind === "image"}
                    <img src={item.url} alt={item.file.name} />
                  {:else if item.kind === "video"}
                    <!-- svelte-ignore a11y_media_has_caption -->
                    <video src={item.url} preload="metadata" muted></video>
                    <span class="play-badge">▶</span>
                  {:else}
                    <span class="file-icon">📄</span>
                  {/if}
                </button>
                <span class="pending-name" title={item.file.name}>{item.file.name}</span>
                {#if item.caption}
                  <span class="pending-caption">{item.caption}</span>
                {/if}
                <button
                  class="icon remove"
                  title="Remove"
                  onclick={() => removePending(item.id)}>×</button
                >
              </div>
            {/each}
            <button class="primary" onclick={sendPending}>
              Send{pending.length > 1 ? ` ${pending.length}` : ""}
            </button>
          </div>
        {/if}

        {#if mentionQuery !== null && mentionMatches.length > 0}
          <div class="mentions">
            {#each mentionMatches as person, i (person.jid)}
              <button
                type="button"
                class="mention"
                class:active={i === mentionIndex}
                onclick={() => selectMention(person)}
                onmouseenter={() => (mentionIndex = i)}>
                {person.name}
              </button>
            {/each}
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
            multiple
            bind:this={filePicker}
            onchange={attach}
          />
          <textarea
            bind:this={composerInput}
            value={draft}
            oninput={onComposerInput}
            onkeydown={onComposerKey}
            rows="1"
            placeholder="Type a message"
          ></textarea>
          <button class="primary" type="submit" disabled={!draft.trim()}>Send</button>
        </form>
      {:else}
        <div class="placeholder">Select a conversation</div>
      {/if}
    </section>

    {#if showGroupInfo}
      <aside class="group-info">
        <button type="button" class="resizer" aria-label="Resize group info" onmousedown={(e) => startResize("right", e)}></button>
        <header>
          <span>Group info</span>
          <button class="icon" title="Close" onclick={() => (showGroupInfo = false)}>×</button>
        </header>
        {#if groupInfo}
          <div class="group-body">
            <h2>{groupInfo.subject ?? "Group"}</h2>
            {#if groupInfo.description}
              <p class="group-desc">{groupInfo.description}</p>
            {/if}
            {#if groupInfo.created_at}
              <p class="hint">
                Created {new Date(groupInfo.created_at * 1000).toLocaleDateString()}
              </p>
            {/if}
            <h3>{groupInfo.participants.length} members</h3>
            <ul>
              {#each groupInfo.participants as person (person.jid)}
                <li>
                  <span class="member-name">
                    {person.number ?? person.name}{person.name && person.number && person.name !== person.number ? ` - ${person.name}` : ""}
                    {#if person.admin}<span class="admin">admin</span>{/if}
                  </span>
                  {#if person.username}
                    <span class="member-meta">@{person.username}</span>
                  {/if}
                </li>
              {/each}
            </ul>
          </div>
        {:else}
          <div class="group-body">
            {#if groupInfoError}
              <p class="hint">Could not load group info: {groupInfoError}</p>
              <button class="primary" onclick={openGroupInfo}>Retry</button>
            {:else}
              <p class="hint">Loading…</p>
            {/if}
          </div>
        {/if}
      </aside>
    {/if}
  </div>
{/if}

{#if previewItem}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div
    class="sheet-backdrop"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) previewId = null;
    }}>
    <div
      class="sheet preview-sheet"
      role="dialog"
      aria-modal="true"
      aria-label="Attachment preview">
      {#if previewItem.kind === "image"}
        <img class="preview-large" src={previewItem.url} alt={previewItem.file.name} />
      {:else if previewItem.kind === "video"}
        <!-- svelte-ignore a11y_media_has_caption -->
        <video class="preview-large" src={previewItem.url} controls></video>
      {:else}
        <span class="file-icon large">📄</span>
      {/if}
      <span class="pending-name">{previewItem.file.name}</span>
      <input
        class="caption"
        value={previewItem.caption}
        oninput={(e) => previewItem && (previewItem.caption = e.currentTarget.value)}
        placeholder="Add a caption"
        autocomplete="off"
      />
      <button class="primary" onclick={() => (previewId = null)}>Done</button>
    </div>
  </div>
{/if}

{#if notice}
  <div class="notice">
    <span>{notice}</span>
    <button class="link" onclick={muteNotice}>Do not warn again</button>
    <button class="icon" title="Dismiss" onclick={() => (notice = null)}>×</button>
  </div>
{/if}

{#if showAccounts}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div
    class="sheet-backdrop"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) showAccounts = false;
    }}>
    <div class="sheet" role="dialog" aria-modal="true" aria-label="Accounts">
      <h2>Accounts</h2>
      {#each accountList as account (account.id)}
        <div class="account-row">
          <input
            type="text"
            value={account.label}
            onchange={(e) => renameAccount(account.id, e.currentTarget.value)}
          />
          <button class="icon" title="Remove" onclick={() => removeAccount(account.id)}>×</button>
        </div>
      {/each}
      <button class="primary" onclick={addAccount}>Add account</button>
      <button class="primary" onclick={() => (showAccounts = false)}>Done</button>
    </div>
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

      <label class="check">
        <input type="checkbox" bind:checked={settings.auto_download_media} />
        <span>Download media automatically</span>
      </label>

      <label class="check">
        <input type="checkbox" bind:checked={settings.warn_missing_video_preview} />
        <span>Warn when a video is sent without a preview</span>
      </label>

      <p class="hint">Retention and folder changes apply the next time Hermóðr starts.</p>

      <div class="actions">
        <button onclick={flushMedia}>Flush media</button>
      </div>

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
    overflow: hidden;
  }
  .chats,
  .group-info {
    position: relative;
  }
  .resizer {
    border: 0;
    background: transparent;
    padding: 0;
    position: absolute;
    top: 0;
    bottom: 0;
    width: 6px;
    cursor: col-resize;
    z-index: 5;
  }
  .chats .resizer,
  .group-info .resizer {
    display: block;
    width: 6px;
    padding: 0;
    border: 0;
    background: transparent;
  }
  .chats .resizer {
    right: -3px;
  }
  .group-info .resizer {
    left: -3px;
  }
  .chat-title {
    background: transparent;
    border: 0;
    color: inherit;
    font: inherit;
    font-weight: 600;
    padding: 0;
    cursor: pointer;
    text-align: left;
  }
  .group-info {
    position: relative;
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-left: 1px solid #27272a;
  }
  .group-info header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 12px 14px;
    font-weight: 600;
    border-bottom: 1px solid #27272a;
  }
  .group-body {
    padding: 14px;
    overflow-y: auto;
  }
  .group-body h2 {
    margin: 0 0 8px;
    font-size: 16px;
  }
  .group-body h3 {
    margin: 16px 0 6px;
    font-size: 13px;
    color: #a1a1aa;
  }
  .group-desc {
    margin: 0 0 8px;
    color: #d4d4d8;
    white-space: pre-wrap;
  }
  .group-body ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .group-body li {
    padding: 6px 0;
    border-bottom: 1px solid #1c1c1f;
  }
  .admin {
    color: #86efac;
    font-size: 11px;
  }
  .member-name {
    display: block;
  }
  .member-meta {
    display: block;
    color: #71717a;
    font-size: 11px;
  }
  .link {
    color: #93c5fd;
    cursor: pointer;
  }
  .preview-card {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    text-align: left;
    background: #1c1c1f;
    border: 0;
    border-radius: 6px;
    padding: 8px;
    margin-top: 4px;
    color: inherit;
    font: inherit;
    cursor: pointer;
    max-width: 100%;
  }
  .preview-thumb {
    width: 64px;
    height: 64px;
    object-fit: cover;
    border-radius: 4px;
    flex: none;
  }
  .preview-body {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .preview-title {
    font-weight: 500;
  }
  .preview-desc {
    font-size: 12px;
    color: #a1a1aa;
    overflow: hidden;
    display: -webkit-box;
    line-clamp: 2;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
  }
  .preview-host {
    font-size: 11px;
    color: #71717a;
  }
  /* Keep the newlines the sender typed, and wrap long tokens. */
  .text {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .chats {
    position: relative;
    overflow: hidden;
    border-right: 1px solid #27272a;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .chats header,
  .conversation header {
    min-width: 0;
    overflow: hidden;
    padding: 12px 14px;
    font-weight: 600;
    border-bottom: 1px solid #27272a;
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .account-row {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-bottom: 8px;
  }
  .account-row input {
    flex: 1;
  }
  .account-bar {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 4px;
    overflow: hidden;
  }
  /* Only the header bar fills the space between the title and the gear. */
  .chats header .account-bar {
    flex: 1;
  }
  .account {
    background: #1c1c1f;
    border: 1px solid #27272a;
    color: #a1a1aa;
    font: inherit;
    font-size: 11px;
    padding: 3px 10px;
    border-radius: 999px;
    cursor: pointer;
    max-width: 110px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .account.active {
    background: #22c55e;
    border-color: #22c55e;
    color: #052e16;
    font-weight: 600;
  }
  .notice {
    position: fixed;
    left: 50%;
    bottom: 18px;
    transform: translateX(-50%);
    z-index: 200;
    display: flex;
    align-items: center;
    gap: 10px;
    max-width: 80vw;
    padding: 8px 14px;
    background: #3f3f46;
    border-radius: 8px;
    font-size: 13px;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.5);
  }
  .notice .link {
    background: transparent;
    border: 0;
    color: #93c5fd;
    font: inherit;
    cursor: pointer;
    white-space: nowrap;
  }
  .search {
    margin: 8px 14px;
    background: #1c1c1f;
    border: 1px solid #3f3f46;
    border-radius: 6px;
    padding: 6px 10px;
    color: inherit;
    font: inherit;
  }
  .results .preview {
    color: #71717a;
  }
  .chats ul {
    list-style: none;
    margin: 0;
    padding: 0;
    overflow-y: auto;
    overflow-x: hidden;
    flex: 1;
  }
  .chat-row {
    width: 100%;
    min-width: 0;
    overflow: hidden;
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
  .chat-row:hover {
    background: #1c1c1f;
  }
  .chat-row.active {
    background: #27272a;
  }
  .badges {
    grid-area: badge;
    display: flex;
    align-items: center;
    gap: 4px;
  }
  .pin {
    margin-right: 2px;
  }
  .badge.mention {
    background: #2563eb;
    color: #fff;
    border: 0;
    cursor: pointer;
    font-weight: 700;
  }
  .badge.pin-toggle {
    background: transparent;
    border: 0;
    color: inherit;
    min-width: 0;
    padding: 0;
    cursor: pointer;
    opacity: 0;
    font-size: 11px;
  }
  .chat-row:hover .pin-toggle {
    opacity: 0.7;
  }
  .jump-mention {
    font-size: 12px;
  }
  .name {
    grid-area: name;
    min-width: 0;
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
    min-width: 0;
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
    min-width: 0;
    position: relative;
  }
  .load-older {
    align-self: center;
    background: #27272a;
    border: 0;
    border-radius: 999px;
    color: #d4d4d8;
    font: inherit;
    font-size: 12px;
    padding: 4px 12px;
    cursor: pointer;
  }
  .messages {
    flex: 1;
    overflow-y: auto;
    overflow-x: hidden;
    min-width: 0;
    padding: 14px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .sync-banner {
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    z-index: 100;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 14px;
    background: #1c1c1f;
    border-bottom: 1px solid #27272a;
  }
  .sync-text {
    font-size: 11px;
    color: #a1a1aa;
    white-space: nowrap;
  }
  .sync-track {
    flex: 1;
    height: 4px;
    border-radius: 2px;
    background: #27272a;
    overflow: hidden;
  }
  .sync-bar {
    height: 100%;
    background: #22c55e;
    transition: width 0.2s ease;
  }
  .bubble {
    max-width: 70%;
    /* Without this a flex item refuses to shrink below its content, so a large
       image stretches the bubble instead of being scaled down to fit it. */
    min-width: 0;
    /* The message list is a flex column, so bubbles shrink by default. With a
       few hundred of them they squash to one line and `overflow: hidden` clips
       the text away, which looks like every message collapsing. */
    flex-shrink: 0;
    align-self: flex-start;
    background: #27272a;
    border-radius: 8px;
    padding: 6px 10px;
    display: flex;
    flex-direction: column;
    gap: 3px;
    word-break: break-word;
    overflow-wrap: anywhere;
    overflow: hidden;
  }
  .bubble.highlighted {
    outline: 2px solid #22c55e;
    outline-offset: 1px;
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
    display: flex;
    align-items: center;
    gap: 6px;
    background: transparent;
    border: 0;
    border-left: 2px solid #52525b;
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
    font-size: 12px;
    color: #a1a1aa;
    border-left: 2px solid #52525b;
    padding-left: 6px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
    max-width: 100%;
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
    background: transparent;
    border: 0;
    padding: 0;
    font: inherit;
    color: #93c5fd;
    cursor: pointer;
    text-align: left;
  }
  /* Media opens in the system viewer, so the whole preview is the button. */
  .media-button {
    position: relative;
    display: block;
    padding: 0;
    border: 0;
    background: transparent;
    cursor: zoom-in;
    line-height: 0;
  }
  .media-button.video {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 6px;
    padding: 14px 16px;
    cursor: pointer;
    background: #111214;
    border-radius: 6px;
    min-width: 180px;
    min-height: 100px;
  }
  .media-overlay {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #e4e4e7;
    font-size: 26px;
    font-weight: 700;
    letter-spacing: 1px;
    text-shadow: 0 1px 6px rgba(0, 0, 0, 0.8);
    pointer-events: none;
  }
  .play-badge {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #e4e4e7;
    font-size: 28px;
    text-shadow: 0 1px 6px rgba(0, 0, 0, 0.8);
    pointer-events: none;
  }
  .file-icon {
    font-size: 28px;
  }
  .file-icon.large {
    font-size: 56px;
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
  .pending {
    display: flex;
    align-items: flex-end;
    flex-wrap: wrap;
    gap: 12px;
    padding: 8px 14px;
    background: #1c1c1f;
    border-top: 1px solid #27272a;
  }
  .pending-item {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 120px;
  }
  .pending-thumb {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    width: 120px;
    height: 72px;
    overflow: hidden;
    border: 1px solid #3f3f46;
    border-radius: 6px;
    background: #111214;
    color: #e4e4e7;
    cursor: pointer;
  }
  .pending-thumb img,
  .pending-thumb video {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .pending-name {
    font-size: 11px;
    color: #a1a1aa;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pending-caption {
    font-size: 11px;
    color: #86efac;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pending .remove {
    position: absolute;
    top: -6px;
    right: -6px;
    width: 18px;
    height: 18px;
    border-radius: 999px;
    background: #3f3f46;
    color: #e4e4e7;
    font-size: 12px;
    line-height: 1;
  }
  .caption {
    background: #111214;
    border: 1px solid #3f3f46;
    border-radius: 6px;
    padding: 8px 10px;
    color: inherit;
    font: inherit;
  }
  .preview-sheet {
    width: min(680px, 80vw);
  }
  .preview-large {
    max-width: 100%;
    max-height: 60vh;
    border-radius: 8px;
    object-fit: contain;
  }
  .quote-author {
    display: block;
    font-weight: 600;
    color: #a1a1aa;
  }
  .quote-thumb {
    width: 28px;
    height: 28px;
    object-fit: cover;
    border-radius: 4px;
    flex: none;
  }
  .quote-icon {
    font-size: 14px;
    flex: none;
  }
  .quote-text {
    flex: 1;
    min-width: 0;
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
  .reply-thumb {
    width: 32px;
    height: 32px;
    object-fit: cover;
    border-radius: 4px;
    flex: none;
  }
  .reply-icon {
    font-size: 16px;
    flex: none;
  }
  .mentions {
    display: flex;
    flex-direction: column;
    max-height: 180px;
    overflow-y: auto;
    background: #1c1c1f;
    border-top: 1px solid #27272a;
  }
  .mention {
    text-align: left;
    background: transparent;
    border: 0;
    color: inherit;
    font: inherit;
    padding: 7px 14px;
    cursor: pointer;
  }
  .mention.active,
  .mention:hover {
    background: #27272a;
  }
  .composer {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 14px;
    border-top: 1px solid #27272a;
  }
  .composer > input:not(.file-input),
  .composer > textarea {
    flex: 1;
    background: #1c1c1f;
    border: 1px solid #3f3f46;
    border-radius: 6px;
    padding: 8px 10px;
    color: inherit;
    font: inherit;
  }
  .composer > textarea {
    resize: none;
    line-height: 1.35;
    max-height: 140px;
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
