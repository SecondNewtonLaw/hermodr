//! The client service: connection lifecycle, typed events, and storage.
//!
//! This is the layer the UI talks to. It owns the protocol [`Bot`], converts
//! library events into [`ServiceEvent`]s the UI can render, and persists
//! messages through the [`MessageStore`] so retention stays enforced.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::Result;
use serde::Serialize;
use tokio::sync::broadcast;
use whatsapp_rust::{
    download::{Downloadable, MediaType},
    media::{self, AudioOptions, DocumentOptions, ImageOptions, VideoOptions},
    prelude::*,
    wacore::msg_secret::MsgSecretRetention,
    wacore::types::presence::ReceiptType,
    wacore::types::events::Event,
    wacore_binary::builder::NodeBuilder,
    CacheConfig,
    WAPatchName,
};

use crate::{
    history::HistoryPolicy,
    store::{MessageStore, Retention, StoredMessage},
};

/// Builds the cache configuration for a given retention window.
///
/// `msg_secrets` are the decryption keys kept so edits, reactions and poll
/// votes can still be applied to their parent message. The library's default
/// horizon is 30 days for text and 90 for polls, which on an account with
/// hundreds of thousands of messages grows the session database into the
/// hundreds of megabytes. Since Hermóðr only keeps messages for
/// [`Retention::max_age_hours`], keeping keys far beyond that window protects
/// add-ons for messages that no longer exist. The horizon is therefore capped
/// at the message window, with a floor of an hour so edits arriving slightly
/// after their parent are never lost.
fn cache_config_for(retention: &Retention) -> CacheConfig {
    let horizon = retention
        .max_age_hours
        .map(|hours| Duration::from_secs(u64::from(hours) * 3600))
        .unwrap_or_else(|| Duration::from_secs(30 * 86_400))
        .max(Duration::from_secs(3600));

    CacheConfig {
        msg_secret_retention: MsgSecretRetention {
            text: horizon,
            // Poll and bot secrets have no sender-side time window, but they
            // still only matter while the parent message is retained.
            poll_event: horizon,
            bot: horizon,
        },
        ..Default::default()
    }
}

/// Reclaims decryption secrets left behind by a longer retention horizon.
///
/// The library prunes `msg_secrets` by their stored `expires_at`, so rows
/// written before the horizon was shortened keep their original deadline and
/// the session database stays large. This drops secrets whose parent message is
/// already outside the retention window, and shortens the deadline on the rest
/// so they expire on our schedule.
///
/// Rows with no message timestamp, or with the "never expires" marker, are left
/// untouched: their age cannot be established, and discarding them could break
/// decryption for a message we still keep.
fn reclaim_oversized_secrets(session_path: &Path, retention: &Retention) -> Result<usize> {
    let Some(hours) = retention.max_age_hours else {
        return Ok(0);
    };
    if !session_path.exists() {
        return Ok(0);
    }

    let conn = rusqlite::Connection::open(session_path)?;
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'msg_secrets'",
        [],
        |row| row.get(0),
    )?;
    if has_table == 0 {
        return Ok(0);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let horizon = i64::from(hours) * 3600;
    let cutoff = now - horizon;

    let removed = conn.execute(
        "DELETE FROM msg_secrets WHERE message_ts > 0 AND message_ts < ?1",
        [cutoff],
    )?;
    conn.execute(
        "UPDATE msg_secrets SET expires_at = ?1
         WHERE message_ts > 0 AND expires_at > ?1",
        [now + horizon],
    )?;

    // SQLite reuses freed pages rather than returning them to the filesystem,
    // so the file stays large after a bulk delete and a later run would find
    // nothing left to remove. Compacting whenever a meaningful freelist has
    // built up covers both the profile that deletes now and the one that was
    // already pruned by an earlier build. This runs before the bot starts, so
    // there is no concurrent access to disturb.
    let free_pages: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    if removed > 0 || free_pages > 1_000 {
        conn.execute_batch("VACUUM")?;
    }

    Ok(removed)
}

/// Events the UI reacts to.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ServiceEvent {
    /// A pairing QR is ready to display.
    ///
    /// Every variant uses named fields: an internally tagged enum cannot
    /// represent a newtype variant holding a bare `String`, and serialization
    /// failure would silently drop the event.
    QrCode { code: String },
    Connected,
    Disconnected,
    /// A message was received or sent and stored.
    ///
    /// Boxed because `StoredMessage` is far larger than the other variants, and
    /// every clone of the enum is stored in the broadcast buffer.
    Message { message: Box<StoredMessage> },
    /// Message history was changed by retention, so the UI should refresh.
    RetentionApplied { removed: usize },
    /// Address-book names were learned, so cached chats and messages now hold
    /// stale display names and should be refetched.
    NamesUpdated { count: usize },
    /// The offline backlog is draining; `pending` is how many messages the
    /// server announced at the start of the drain.
    Syncing { pending: usize },
    /// The backlog finished draining.
    Synced,
}

/// A group member, as the mention autocomplete needs it.
#[derive(Debug, Clone, Serialize)]
pub struct Participant {
    /// JID to put in `mentioned_jid` and to mention in the text.
    pub jid: String,
    /// Display name, from the address book when known.
    pub name: String,
    /// Whether the member is a group admin.
    pub admin: bool,
    /// Phone number, when known.
    pub number: Option<String>,
    /// WhatsApp username, when the member has one.
    pub username: Option<String>,
}

/// One row of the chat/contact search.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub jid: String,
    pub name: String,
    /// The JID's user part, so the UI can show "number - name".
    pub number: String,
    /// `contact` or `group`.
    pub kind: String,
    /// Whether the name came from the address book.
    pub saved: bool,
    /// Whether the chat already has messages locally.
    pub has_messages: bool,
}

/// Everything the group info sidebar shows.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GroupInfo {
    pub subject: Option<String>,
    pub description: Option<String>,
    pub created_at: Option<u64>,
    pub participants: Vec<Participant>,
}

/// How the service should behave for one account.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Session database (protocol and crypto state).
    pub session_path: PathBuf,
    /// Message store database.
    pub messages_path: PathBuf,
    /// How much history to keep locally.
    pub retention: Retention,
    /// Whether to pull the deep history sync during pairing.
    pub accept_full_history: bool,
    /// Where downloaded media is written. `None` disables media downloads.
    pub media_dir: Option<PathBuf>,
}

impl ServiceConfig {
    /// Sensible defaults for a single account under `data_dir`.
    pub fn under(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            session_path: data_dir.join("session.db"),
            messages_path: data_dir.join("messages.db"),
            retention: Retention::default(),
            accept_full_history: false,
            media_dir: Some(data_dir.join("media")),
        }
    }
}

/// Fetches a group's subject over the `w:g2` namespace.
///
/// Group names are not carried on incoming messages, and the library exposes no
/// typed accessor for them in this version, so the query is issued directly.
async fn fetch_group_subject(client: &Client, group: &str) -> Option<String> {
    let jid: Jid = group.parse().ok()?;
    let node = NodeBuilder::new("iq")
        .attr("type", "get")
        .attr("xmlns", "w:g2")
        .attr("to", jid)
        .children([NodeBuilder::new("query")
            .attr("request", "interactive")
            .build()])
        .build();

    let response = client
        .send_iq_node(node, Some(Duration::from_secs(15)))
        .await
        .ok()?;

    // The subject lives on the <group> child of the response.
    let group_node = response.get().get_optional_child_by_tag(&["group"])?;
    group_node
        .attrs()
        .optional_string("subject")
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// A running account client.
///
/// Dropping this stops the background task and closes the stores.
pub struct Service {
    client: Arc<Client>,
    store: Arc<MessageStore>,
    events: broadcast::Sender<ServiceEvent>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    /// Where downloaded media is written; `None` disables media.
    media_dir: Option<PathBuf>,
    /// Latest pairing code, kept so a subscriber that attaches after the code
    /// was issued can still display it. The QR is emitted during startup, which
    /// a late subscriber would otherwise miss entirely.
    qr: Arc<Mutex<Option<String>>>,
    connected: Arc<AtomicBool>,
    /// Group metadata for this run, so opening a chat does not re-query the
    /// server and trip its rate limit. Group membership changes rarely enough
    /// that a session-lifetime cache is fine.
    group_cache: Mutex<std::collections::HashMap<String, GroupInfo>>,
    /// Every group the account is in, `(jid, subject)`, filled on first search.
    groups_cache: Mutex<Vec<(String, String)>>,
}

impl Service {
    /// Connects an account, pairing first if it has no session yet.
    ///
    /// Returns the service along with an event receiver that was registered
    /// before the connection attempt began. The pairing code is emitted during
    /// startup, so a receiver created afterwards would miss it; the returned one
    /// is guaranteed to see every event from the beginning.
    pub async fn start(config: ServiceConfig) -> Result<(Self, broadcast::Receiver<ServiceEvent>)> {
        let store = Arc::new(MessageStore::open(
            &config.messages_path,
            config.retention,
        )?);
        let (events, initial_rx) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        if let Ok(removed) = reclaim_oversized_secrets(&config.session_path, &config.retention) {
            if removed > 0 {
                println!("[service] reclaimed {removed} stale decryption secret(s)");
            }
        }

        let policy = if config.accept_full_history {
            HistoryPolicy::accept_everything()
        } else {
            HistoryPolicy::default()
        };

        let qr_state = Arc::new(Mutex::new(None));
        let connected_state = Arc::new(AtomicBool::new(false));
        // Whether the address book has already been replayed this run.
        let names_resynced = Arc::new(AtomicBool::new(false));
        // The client only exists once the bot is built, but the message handler
        // needs it to download media. A OnceLock bridges that ordering.
        let client_slot: Arc<std::sync::OnceLock<Arc<Client>>> =
            Arc::new(std::sync::OnceLock::new());

        let media_dir = config.media_dir.clone();
        let store_for_events = store.clone();
        let events_for_events = events.clone();
        let connected_for_events = connected_state.clone();
        let client_for_events = client_slot.clone();
        let media_dir_for_events = media_dir.clone();

        let bot = Bot::builder()
            .with_backend(SqliteStore::new(config.session_path.to_string_lossy().as_ref()).await?)
            .with_history_sync_admission(policy)
            .with_cache_config(cache_config_for(&config.retention))
            .on_qr_code({
                let events = events.clone();
                let qr_state = qr_state.clone();
                move |code, _timeout| {
                    let events = events.clone();
                    let qr_state = qr_state.clone();
                    async move {
                        *qr_state.lock().unwrap() = Some(code.clone());
                        let _ = events.send(ServiceEvent::QrCode { code });
                    }
                }
            })
            .on_connected({
                let events = events.clone();
                let qr_state = qr_state.clone();
                let connected_state = connected_state.clone();
                let names_resynced = names_resynced.clone();
                let store = store.clone();
                let session_path = config.session_path.clone();
                move |client| {
                    let events = events.clone();
                    let qr_state = qr_state.clone();
                    let connected_state = connected_state.clone();
                    let names_resynced = names_resynced.clone();
                    let store = store.clone();
                    let session_path = session_path.clone();
                    async move {
                        connected_state.store(true, Ordering::SeqCst);
                        // The code is spent once paired.
                        *qr_state.lock().unwrap() = None;
                        let _ = events.send(ServiceEvent::Connected);

                        // Saved contact names reach the client as app-state
                        // patches, and an already-paired session has none left
                        // to deliver. Replay the address book once per run so
                        // the names are learned.
                        if !names_resynced.swap(true, Ordering::SeqCst) {
                            let client = client.clone();
                            let events = events.clone();
                            let session_path = session_path.clone();
                            tokio::spawn(async move {
                                match client
                                    .resync_app_state_collection(WAPatchName::CriticalUnblockLow)
                                    .await
                                {
                                    Ok(_) => {
                                        backfill_lid_names(&session_path, &store);
                                        if let Ok(count) = store.saved_name_count() {
                                            println!(
                                                "[service] address book: {count} saved name(s)"
                                            );
                                            if count > 0 {
                                                let _ =
                                                    events.send(ServiceEvent::NamesUpdated { count });
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("[service] contact resync failed: {e}");
                                    }
                                }
                                // Pins live in a different collection.
                                if let Err(e) = client
                                    .resync_app_state_collection(WAPatchName::RegularLow)
                                    .await
                                {
                                    eprintln!("[service] pin resync failed: {e}");
                                }
                            });
                        }
                    }
                }
            })
            .on_event_for(
                &[
                    EventKind::Messages,
                    EventKind::Disconnected,
                    EventKind::Receipt,
                    EventKind::ServerAck,
                    EventKind::ContactUpdate,
                    EventKind::ContactRemoved,
                    EventKind::OfflineSyncPreview,
                    EventKind::OfflineSyncCompleted,
                    EventKind::PinUpdate,
                ],
                move |event, _client| {
                    let store = store_for_events.clone();
                    let events = events_for_events.clone();
                    let connected = connected_for_events.clone();
                    let client_for_events = client_for_events.clone();
                    let media_dir = media_dir_for_events.clone();
                    async move {
                        match event.as_ref() {
                            Event::Messages(batch) => {
                                let client = client_for_events.get().cloned();
                                // Our own addresses, so a mention can be
                                // recognised whichever form it uses.
                                let own: Vec<String> = client
                                    .as_deref()
                                    .map(|c| {
                                        [c.pn(), c.lid()]
                                            .into_iter()
                                            .flatten()
                                            .map(|j| j.to_non_ad().to_string())
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                for inbound in batch.messages.iter() {
                                    // The envelope carries the sender's display
                                    // name, which is the only name source
                                    // available without a contacts query.
                                    let push_name = inbound.info.push_name.to_string();
                                    let chat = inbound.info.source.chat.to_string();
                                    let sender = inbound.info.source.sender.to_string();
                                    let is_group = inbound.info.source.is_group
                                        || chat.ends_with("@g.us");
                                    let from_me = inbound.info.source.is_from_me;

                                    // Status updates are not a conversation; keep
                                    // them out of the store so they never show up
                                    // as a chat.
                                    if chat == "status@broadcast" {
                                        continue;
                                    }

                                    // Address-book names are keyed by phone
                                    // number, but an LID-addressed chat names
                                    // its sender with a LID, so the two never
                                    // match on their own. The source carries
                                    // the other form; copy the name across so
                                    // the saved one is what gets shown.
                                    if let Some(alt) =
                                        inbound.info.source.sender_alt.as_ref().map(|j| j.to_string())
                                    {
                                        let known = store.name_for(&alt).ok().flatten();
                                        let is_saved = known.is_some();
                                        // Fall back to the phone number, never
                                        // the unreadable LID.
                                        let name = known.unwrap_or_else(|| {
                                            alt.split('@').next().unwrap_or(&alt).to_string()
                                        });
                                        if is_saved {
                                            let _ = store.set_saved_name(&sender, &name);
                                            if !is_group && !from_me {
                                                let _ = store.set_saved_name(&chat, &name);
                                            }
                                        } else {
                                            let _ = store.set_name(&sender, &name);
                                            if !is_group && !from_me {
                                                let _ = store.set_name(&chat, &name);
                                            }
                                        }
                                    }

                                    if !push_name.is_empty() {
                                        // Push names never override a saved one.
                                        let _ = store.set_name(&sender, &push_name);
                                        // A participant's JID has no device suffix
                                        // while a message's sender does, so store
                                        // the bare form too or the group member
                                        // list cannot find the name.
                                        if let Some((user, server)) = sender.split_once('@') {
                                            let bare = format!(
                                                "{}@{}",
                                                user.split(':').next().unwrap_or(user),
                                                server
                                            );
                                            if bare != sender {
                                                let _ = store.set_name(&bare, &push_name);
                                            }
                                        }
                                        // A one-to-one chat is named after its
                                        // contact. A group is named by its
                                        // subject, and a message we sent must
                                        // never name a chat after us, which is
                                        // what turned a group into our own name.
                                        if !is_group && !from_me {
                                            let _ = store.set_name(&chat, &push_name);
                                        }
                                    }

                                    // A revoke is a protocol message naming the
                                    // original; mark it deleted rather than
                                    // dropping the notice, so the chat shows
                                    // that something was removed.
                                    if let Some(target) = revoke_target(&inbound.message) {
                                        if let Ok(true) = store.revoke(&chat, &target) {
                                            if let Ok(updated) =
                                                store.message(&chat, &target)
                                            {
                                                let _ = events.send(ServiceEvent::Message {
                                                    message: Box::new(updated),
                                                });
                                            }
                                        }
                                        continue;
                                    }

                                    let Some(mut message) = incoming_message(
                                        inbound,
                                        client.as_deref(),
                                        media_dir.as_deref(),
                                    )
                                    .await
                                    else {
                                        continue;
                                    };
                                    // The wire text names mentions by number;
                                    // store the display form so the conversation
                                    // shows the name.
                                    let mentions = mentioned_jids(&inbound.message);
                                    if !mentions.is_empty() {
                                        message.text =
                                            replace_mentions(&message.text, &mentions, &store);
                                    }
                                    // A quote carries no mentioned_jid, and some
                                    // senders omit it, so resolve bare @number
                                    // tokens too.
                                    message.text = resolve_mention_tokens(&message.text, &store);
                                    if let Some(reply) = message.reply_to_text.take() {
                                        message.reply_to_text =
                                            Some(resolve_mention_tokens(&reply, &store));
                                    }
                                    message.mentioned = mentions_me(&inbound.message, &own);
                                    let _ = store.upsert(&message);
                                    let _ = events.send(ServiceEvent::Message { message: Box::new(message) });
                                }
                                // Bound the store right after writes so the
                                // limit holds even if the process stops.
                                if let Ok(removed) = store.enforce_retention() {
                                    if removed > 0 {
                                        let _ = events
                                            .send(ServiceEvent::RetentionApplied { removed });
                                    }
                                }
                            }
                            Event::Disconnected(_) => {
                                connected.store(false, Ordering::SeqCst);
                                let _ = events.send(ServiceEvent::Disconnected);
                            }
                            // A receipt names the messages it refers to, so the
                            // outgoing row can move to delivered or read.
                            Event::Receipt(receipt) => {
                                let status = match receipt.r#type {
                                    ReceiptType::Read | ReceiptType::ReadSelf => "read",
                                    ReceiptType::Delivered => "delivered",
                                    _ => "sent",
                                };
                                let chat = receipt.source.chat.to_string();
                                for id in receipt.message_ids.iter() {
                                    if let Ok(true) =
                                        store.set_status(&chat, id.as_str(), status)
                                    {
                                        if let Ok(updated) = store.message(&chat, id.as_str()) {
                                            let _ = events.send(ServiceEvent::Message {
                                                message: Box::new(updated),
                                            });
                                        }
                                    }
                                }
                            }
                            // The server accepted our stanza, so it is at least sent.
                            Event::ServerAck(ack) => {
                                let accepted = ack.error.is_none();
                                if let (true, Some(chat)) = (accepted, ack.from.as_ref()) {
                                    let chat = chat.to_string();
                                    if let Ok(true) = store.set_status(&chat, &ack.id, "sent") {
                                        if let Ok(updated) = store.message(&chat, &ack.id) {
                                            let _ = events.send(ServiceEvent::Message {
                                                message: Box::new(updated),
                                            });
                                        }
                                    }
                                }
                            }
                            // The name the user saved for a contact comes from
                            // the address book and outranks the push name the
                            // contact set for themselves.
                            Event::ContactUpdate(update) => {
                                let name = update
                                    .action
                                    .full_name
                                    .as_deref()
                                    .or(update.action.first_name.as_deref());
                                if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
                                    let _ = store.set_saved_name(&update.jid.to_string(), name);
                                }
                            }
                            Event::ContactRemoved(removed) => {
                                let _ = store.clear_saved_name(&removed.jid.to_string());
                            }
                            // Progress for the initial catch-up, so the UI can
                            // show how much of the backlog is still arriving.
                            Event::OfflineSyncPreview(preview) => {
                                let pending = preview.messages.max(0) as usize;
                                if pending > 0 {
                                    let _ = events.send(ServiceEvent::Syncing { pending });
                                }
                            }
                            Event::OfflineSyncCompleted(_) => {
                                let _ = events.send(ServiceEvent::Synced);
                            }
                            // Chat pins are account state; mirror them so the
                            // list matches the phone.
                            Event::PinUpdate(pin) => {
                                let pinned = pin.action.pinned.unwrap_or(false);
                                let jid = pin.jid.to_non_ad().to_string();
                                let _ = store.set_pinned(&jid, pinned);
                            }
                            _ => {}
                        }
                    }
                },
            )
            .build()
            .await?;

        let client = bot.client();

        // Hand the client to the message handler, which needs it to download
        // media. Without this the slot stays empty and every attachment is
        // recorded with no file.
        let _ = client_slot.set(client.clone());

        // `run()` only returns on logout or shutdown, so it lives in its own task.
        tokio::spawn(async move {
            tokio::select! {
                _ = bot.run() => {}
                _ = shutdown_rx => {}
            }
        });

        Ok((
            Self {
                client,
                store,
                events,
                shutdown: shutdown_tx,
                media_dir,
                qr: qr_state,
                connected: connected_state,
                group_cache: Mutex::new(std::collections::HashMap::new()),
                groups_cache: Mutex::new(Vec::new()),
            },
            initial_rx,
        ))
    }

    /// Subscribes to service events.
    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.events.subscribe()
    }

    /// The current pairing code, if the account still needs pairing.
    ///
    /// Exposed separately from the event stream because the code is issued
    /// during startup, before a UI subscriber may have attached.
    pub fn current_qr(&self) -> Option<String> {
        self.qr.lock().unwrap().clone()
    }

    /// Whether the account is currently connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Resolves display names for chats that do not have one yet.
    ///
    /// Only groups need a query: a one-to-one chat is named after its contact,
    /// whose name arrives with the message itself. Returns how many were
    /// resolved, so the caller can refresh only when something changed.
    pub async fn resolve_missing_names(&self) -> Result<usize> {
        let mut resolved = 0;
        for chat in self.store.chats()? {
            if chat.display_name.is_some() || !chat.chat.ends_with("@g.us") {
                continue;
            }
            if let Some(subject) = fetch_group_subject(&self.client, &chat.chat).await {
                self.store.set_name(&chat.chat, &subject)?;
                resolved += 1;
            }
        }
        Ok(resolved)
    }

    /// Sends a text message to a chat.
    ///
    /// Members of a group chat, for mention autocomplete.
    pub async fn participants(&self, chat: &str) -> Result<Vec<Participant>> {
        Ok(self.group_info(chat).await?.participants)
    }

    /// Everything the group info sidebar needs.
    pub async fn group_info(&self, chat: &str) -> Result<GroupInfo> {
        if !chat.ends_with("@g.us") {
            return Ok(GroupInfo::default());
        }
        if let Some(info) = self.group_cache.lock().unwrap().get(chat).cloned() {
            return Ok(info);
        }
        let jid: Jid = chat.parse()?;
        let mut metadata = self
            .client
            .groups()
            .fetch_metadata(&jid)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        // The server often omits the phone number on LID participants, so fill
        // it from the client's LID/PN cache before naming them.
        self.client
            .groups()
            .resolve_participant_addresses(&mut metadata)
            .await;

        let mut seen = std::collections::HashSet::new();
        let mut participants = Vec::new();
        for member in &metadata.participants {
            let mention = member.jid.to_non_ad().to_string();
            if !seen.insert(mention.clone()) {
                continue;
            }
            let candidates = [
                member.phone_number.as_ref(),
                member.lid.as_ref(),
                Some(&member.jid),
            ];
            // Phone number without the server, so it reads as a number.
            let number = member
                .phone_number
                .as_ref()
                .map(|j| j.to_non_ad().to_string())
                .or_else(|| mention.ends_with("@s.whatsapp.net").then(|| mention.clone()))
                .map(|j| j.split('@').next().unwrap_or(&j).to_string());
            let username = member.username.as_ref().map(|u| u.to_string());
            // A name someone can read: saved/push name, then username, then the
            // phone number, and only last the LID.
            let name = candidates
                .into_iter()
                .flatten()
                .find_map(|j| self.store.name_for(&j.to_string()).ok().flatten())
                .or_else(|| username.clone())
                .or_else(|| number.clone())
                .unwrap_or_else(|| mention.split('@').next().unwrap_or(&mention).to_string());
            participants.push(Participant {
                jid: mention.clone(),
                name,
                admin: member.is_admin(),
                number,
                username,
            });
        }
        participants.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        let info = GroupInfo {
            subject: metadata.subject.clone(),
            description: metadata.description.clone(),
            created_at: metadata.creation_time,
            participants,
        };
        self.group_cache
            .lock()
            .unwrap()
            .insert(chat.to_string(), info.clone());
        Ok(info)
    }

    /// The sent message is stored and dispatched locally. WhatsApp does not echo
    /// a message back to the device that sent it, so without this the sender
    /// would not see their own message until the store was next reloaded.
    pub async fn send_text(
        &self,
        chat: &str,
        text: impl Into<String>,
        mentions: Vec<String>,
    ) -> Result<()> {
        let to: Jid = chat.parse()?;
        let text = text.into();
        // `@all` is a group mention, carried separately from member mentions.
        let mention_all = mentions.iter().any(|m| m == "@all");
        let mentioned: Vec<String> = mentions.iter().filter(|m| *m != "@all").cloned().collect();
        let result = if mentioned.is_empty() && !mention_all {
            self.client.send_text(to, text.clone()).await?
        } else {
            use whatsapp_rust::wacore::proto_helpers::MessageBuilderExt;
            let mut context = wa::ContextInfo {
                mentioned_jid: mentioned.clone(),
                ..Default::default()
            };
            if mention_all {
                context.group_mentions = vec![wa::GroupMention {
                    group_jid: Some(chat.to_string()),
                    group_subject: self.store.name_for(chat).ok().flatten(),
                }];
            }
            self.client
                .send_message(to, wa::Message::text_with_context(text.clone(), context))
                .await?
        };

        // The wire text names mentions by number; store the display form so the
        // conversation reads the same as the rest of the UI.
        let text = if mentioned.is_empty() {
            text
        } else {
            replace_mentions(&text, &mentioned, &self.store)
        };

        let message = StoredMessage {
            chat: chat.to_string(),
            id: result.message_id.clone(),
            sender: self.own_jid(),
            sender_name: None,
            timestamp: unix_now(),
            from_me: true,
            text,
            media_kind: None,
            media_path: None,
            reply_to_id: None,
            reply_to_text: None,
            reply_to_sender: None,
            // Not `true`: we cannot know whether the recipient has read it, and
            // claiming so shows a read marker that is not true.
            read: false,
            revoked: false,
            mentioned: false,
            preview_url: None,
            preview_title: None,
            preview_desc: None,
            preview_thumb: None,
            status: Some("pending".into()),
        };
        self.store.upsert(&message)?;
        let _ = self.events.send(ServiceEvent::Message { message: Box::new(message) });
        Ok(())
    }

    /// Our own JID, used as the sender of messages we send.
    fn own_jid(&self) -> String {
        self.client
            .pn()
            .or_else(|| self.client.lid())
            .map(|j| j.to_non_ad().to_string())
            .unwrap_or_default()
    }

    /// Chats, contacts and groups matching a query.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        use std::collections::HashSet;

        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }

        let local = self.store.chats()?;
        let local_jids: HashSet<String> = local.iter().map(|c| c.chat.clone()).collect();
        let mut results: Vec<SearchResult> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        // Local chats first: they have history and are what a search usually
        // means.
        for chat in &local {
            let number = user_part(&chat.chat);
            let name = chat.display_name.clone().unwrap_or_else(|| number.clone());
            if name.to_lowercase().contains(&needle) || number.contains(&needle) {
                seen.insert(chat.chat.clone());
                results.push(SearchResult {
                    kind: if chat.chat.ends_with("@g.us") { "group".to_string() } else { "contact".to_string() },
                    saved: self.store.name_is_saved(&chat.chat),
                    jid: chat.chat.clone(),
                    name,
                    number,
                    has_messages: true,
                });
            }
        }

        // Address book and learned names.
        for (jid, name, saved) in self.store.search_names(&needle, 50)? {
            if !seen.insert(jid.clone()) {
                continue;
            }
            results.push(SearchResult {
                kind: if jid.ends_with("@g.us") { "group".to_string() } else { "contact".to_string() },
                saved,
                jid: jid.clone(),
                name,
                number: user_part(&jid),
                has_messages: local_jids.contains(&jid),
            });
        }

        // Groups from the account, including ones with no local history.
        for (jid, subject) in self.group_overviews().await {
            if subject.to_lowercase().contains(&needle) && seen.insert(jid.clone()) {
                results.push(SearchResult {
                    jid: jid.clone(),
                    name: subject,
                    number: String::new(),
                    kind: "group".into(),
                    saved: false,
                    has_messages: local_jids.contains(&jid),
                });
            }
        }

        results.truncate(50);
        Ok(results)
    }

    /// Every group the account is in, fetched once and cached.
    async fn group_overviews(&self) -> Vec<(String, String)> {
        {
            let cache = self.groups_cache.lock().unwrap();
            if !cache.is_empty() {
                return cache.clone();
            }
        }
        match self.client.groups().list_participating().await {
            Ok(groups) => {
                let pairs: Vec<(String, String)> = groups
                    .into_iter()
                    .filter_map(|g| g.subject.map(|s| (g.id.to_string(), s)))
                    .collect();
                *self.groups_cache.lock().unwrap() = pairs.clone();
                pairs
            }
            Err(_) => Vec::new(),
        }
    }

    /// Pins or unpins a chat, mirroring it to the account.
    pub async fn set_pinned(&self, chat: &str, pinned: bool) -> Result<()> {
        let jid: Jid = chat.parse()?;
        self.store.set_pinned(&jid.to_non_ad().to_string(), pinned)?;
        let actions = self.client.chat_actions();
        let result = if pinned {
            actions.pin_chat(&jid).await
        } else {
            actions.unpin_chat(&jid).await
        };
        result.map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    /// Unread messages that mention us, oldest first.
    pub fn unread_mentions(&self, chat: &str) -> Result<Vec<String>> {
        self.store.unread_mentions(chat)
    }

    /// Where media is stored, if enabled.
    pub fn media_dir(&self) -> Option<PathBuf> {
        self.media_dir.clone()
    }

    /// Marks a chat's incoming messages as read. Returns how many changed.
    pub fn mark_read(&self, chat: &str) -> Result<usize> {
        self.store.mark_chat_read(chat)
    }

    /// Sends a text message quoting an earlier one.
    ///
    /// The quote is rebuilt from the stored message rather than the original
    /// protobuf, which we do not keep; the recipient renders the quoted text.
    pub async fn send_reply(
        &self,
        chat: &str,
        text: impl Into<String>,
        reply_to_id: &str,
        reply_to_sender: &str,
        reply_to_text: &str,
        mentions: Vec<String>,
    ) -> Result<()> {
        let to: Jid = chat.parse()?;
        // The quoted author must be the address without a device suffix: a
        // participant like `123:98@lid` is not resolvable by recipients, who
        // then attribute the quoted message to the sender of the reply.
        let sender: Jid = reply_to_sender.parse::<Jid>()?.to_non_ad();
        let text = text.into();

        use whatsapp_rust::wacore::proto_helpers::build_quote_context_with_info;
        let quoted = wa::Message::text(reply_to_text);
        let mention_all = mentions.iter().any(|m| m == "@all");
        let mentioned: Vec<String> = mentions.iter().filter(|m| *m != "@all").cloned().collect();
        let mut context = build_quote_context_with_info(reply_to_id, &sender, &to, &to, &quoted);
        if !mentioned.is_empty() {
            context.mentioned_jid = mentioned.clone();
        }
        if mention_all {
            context.group_mentions = vec![wa::GroupMention {
                group_jid: Some(chat.to_string()),
                group_subject: self.store.name_for(chat).ok().flatten(),
            }];
        }

        use whatsapp_rust::wacore::proto_helpers::MessageBuilderExt;
        let message = wa::Message::text_with_context(text.clone(), context);
        let result = self.client.send_message(to, message).await?;

        let text = if mentioned.is_empty() {
            text
        } else {
            replace_mentions(&text, &mentioned, &self.store)
        };

        let stored = StoredMessage {
            chat: chat.to_string(),
            id: result.message_id.clone(),
            sender: self.own_jid(),
            sender_name: None,
            timestamp: unix_now(),
            from_me: true,
            text,
            media_kind: None,
            media_path: None,
            reply_to_id: Some(reply_to_id.to_string()),
            reply_to_text: Some(reply_to_text.to_string()),
            reply_to_sender: Some(if sender.to_string() == self.own_jid() {
                "@me".to_string()
            } else {
                reply_to_sender.to_string()
            }),
            read: false,
            revoked: false,
            mentioned: false,
            preview_url: None,
            preview_title: None,
            preview_desc: None,
            preview_thumb: None,
            status: Some("pending".into()),
        };
        self.store.upsert(&stored)?;
        let _ = self.events.send(ServiceEvent::Message { message: Box::new(stored) });
        Ok(())
    }

    /// Sends a file as an image or document, chosen from its extension.
    ///
    /// Images are sent as images so they render inline; everything else goes as
    /// a document, which is what a file picker is usually for.
    pub async fn send_media(
        &self,
        chat: &str,
        file_name: &str,
        bytes: Vec<u8>,
        caption: Option<String>,
    ) -> Result<()> {
        let to: Jid = chat.parse()?;
        let file_name = file_name.to_string();
        let extension = std::path::Path::new(&file_name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();

        // The extension decides how the receiver renders the file, so a video
        // only arrives as a video (not a document) if it is sent as one.
        let (media_type, kind) = match extension.as_str() {
            "jpg" | "jpeg" | "png" | "gif" | "webp" => (MediaType::Image, "image"),
            "mp4" | "mov" | "m4v" | "webm" | "mkv" => (MediaType::Video, "video"),
            "ogg" | "opus" | "mp3" | "m4a" | "aac" | "wav" => (MediaType::Audio, "audio"),
            _ => (MediaType::Document, "document"),
        };

        let upload = self.client.upload(bytes.clone(), media_type, Default::default()).await?;

        let mimetype = mime_for(&extension).map(str::to_string);
        let message = match kind {
            "image" => media::image_message(
                upload,
                ImageOptions {
                    caption: caption.clone(),
                    mimetype,
                    ..Default::default()
                },
            ),
            "video" => media::video_message(
                upload,
                VideoOptions {
                    caption: caption.clone(),
                    mimetype,
                    ..Default::default()
                },
            ),
            "audio" => media::audio_message(
                upload,
                AudioOptions {
                    mimetype,
                    // An ogg/opus attachment is a voice note, which is how
                    // WhatsApp records and replays them.
                    ptt: Some(extension == "ogg"),
                    ..Default::default()
                },
            ),
            _ => media::document_message(
                upload,
                DocumentOptions {
                    file_name: Some(file_name.clone()),
                    caption: caption.clone(),
                    mimetype,
                    ..Default::default()
                },
            ),
        };

        let result = self.client.send_message(to, message).await?;

        // Keep our own copy so the sender sees what they sent.
        let mut stored_path = None;
        if let Some(dir) = &self.media_dir() {
            if std::fs::create_dir_all(dir).is_ok() {
                let name = if extension.is_empty() { "bin".to_string() } else { extension.clone() };
                let dest = dir.join(format!("{}.{}", result.message_id, name));
                if std::fs::write(&dest, &bytes).is_ok() {
                    stored_path = Some(dest.to_string_lossy().to_string());
                }
            }
        }

        let stored = StoredMessage {
            chat: chat.to_string(),
            id: result.message_id.clone(),
            sender: self.own_jid(),
            sender_name: None,
            timestamp: unix_now(),
            from_me: true,
            text: caption.unwrap_or_default(),
            media_kind: Some(kind.to_string()),
            media_path: stored_path,
            reply_to_id: None,
            reply_to_text: None,
            reply_to_sender: None,
            read: false,
            revoked: false,
            mentioned: false,
            preview_url: None,
            preview_title: None,
            preview_desc: None,
            preview_thumb: None,
            status: Some("pending".into()),
        };
        self.store.upsert(&stored)?;
        let _ = self.events.send(ServiceEvent::Message { message: Box::new(stored) });
        Ok(())
    }

    /// Stored messages for a chat, newest first.
    pub fn messages(&self, chat: &str, limit: u32) -> Result<Vec<StoredMessage>> {
        self.store.messages_for(chat, limit)
    }

    /// Chat summaries, most recently active first.
    pub fn chats(&self) -> Result<Vec<crate::store::ChatSummary>> {
        self.store.chats()
    }

    /// Stops the background task.
    pub fn shutdown(self) {
        let _ = self.shutdown.send(());
    }
}

/// The id of the message a revoke refers to, if this message is a revoke.
fn revoke_target(message: &wa::Message) -> Option<String> {
    use wa::message::protocol_message::Type;
    let protocol = message.get_base_message().protocol_message.as_option()?;
    if protocol.r#type != Some(Type::REVOKE) {
        return None;
    }
    let key = protocol.key.as_option()?;
    key.id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| id.to_string())
}

/// Extracts the quoted message id and text from an incoming message.
///
/// `context_info` lives on each inner message type rather than on `Message`
/// itself, so the carriers a reply can arrive on are checked in turn.
fn quote_of(message: &wa::Message) -> Option<(String, String, String)> {
    let base = message.get_base_message();
    let context = base
        .extended_text_message
        .as_option()
        .and_then(|m| m.context_info.as_option())
        .or_else(|| {
            base.image_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            base.video_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            base.audio_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            base.document_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })?;

    let id = context.stanza_id.as_ref()?.to_string();
    let author = context
        .participant
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let quoted = context.quoted_message.as_option()?;
    let text = quoted.text_content().unwrap_or("[media]").to_string();
    Some((id, author, text))
}

/// The user part of a JID, without the device suffix or server.
fn user_part(jid: &str) -> String {
    jid.split('@')
        .next()
        .unwrap_or(jid)
        .split(':')
        .next()
        .unwrap_or(jid)
        .to_string()
}

/// Seconds since the Unix epoch.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The media carried by a message, if any.
///
/// Returns the kind and a boxed download reference: the four media protos are
/// distinct types that each implement `Downloadable`.
fn detect_media(message: &wa::Message) -> Option<(&'static str, MediaType, Box<dyn Downloadable + Send + Sync>)> {
    if let Some(image) = message.image_message.as_option() {
        return Some(("image", MediaType::Image, Box::new(image.clone())));
    }
    if let Some(video) = message.video_message.as_option() {
        return Some(("video", MediaType::Video, Box::new(video.clone())));
    }
    if let Some(audio) = message.audio_message.as_option() {
        return Some(("audio", MediaType::Audio, Box::new(audio.clone())));
    }
    if let Some(document) = message.document_message.as_option() {
        return Some(("document", MediaType::Document, Box::new(document.clone())));
    }
    None
}

/// Converts a protocol message into a storable one, downloading any media.
///
/// Returns `None` for messages that carry neither text nor media, so protocol
/// traffic does not fill the store with empty rows.
async fn incoming_message(
    inbound: &InboundMessage,
    client: Option<&Client>,
    media_dir: Option<&Path>,
) -> Option<StoredMessage> {
    let info = &inbound.info;
    let mut text = inbound.message.text_content().unwrap_or_default().to_string();

    let mut media_kind = None;
    let mut media_path = None;

    if let Some((kind, media_type, downloadable)) = detect_media(&inbound.message) {
        media_kind = Some(kind.to_string());

        // Download when a destination and a client are available. A failure
        // still records the message, so the text and metadata are not lost.
        if let (Some(client), Some(dir)) = (client, media_dir) {
            match client.download(downloadable.as_ref()).await {
                Ok(bytes) => {
                    if std::fs::create_dir_all(dir).is_ok() {
                        let path = dir.join(format!("{}.{}", info.id, extension_for(kind, media_type)));
                        if std::fs::write(&path, &bytes).is_ok() {
                            media_path = Some(path.to_string_lossy().to_string());
                        }
                    }
                }
                Err(e) => log::warn!("failed to download {} media: {e}", info.id),
            }
        }

        if text.is_empty() {
            text = format!("[{kind}]");
        }
    }

    if text.is_empty() && media_kind.is_none() {
        return None;
    }

    // A reply carries the quote in the message context. We do not keep the
    // original protobuf, so the text is copied out for display.
    let (reply_to_id, reply_to_text, reply_to_sender) = quote_of(&inbound.message)
        .map(|(id, sender, text)| {
            // Quoting our own message should read "You", not our phone number.
            let mine = client
                .map(|c| {
                    [c.pn(), c.lid()]
                        .into_iter()
                        .flatten()
                        .any(|j| j.to_non_ad().to_string() == sender)
                })
                .unwrap_or(false);
            let sender = if mine { "@me".to_string() } else { sender };
            (Some(id), Some(text), Some(sender))
        })
        .unwrap_or((None, None, None));

    // A link preview rides on the extended text message.
    let (preview_url, preview_title, preview_desc, preview_thumb) =
        link_preview(&inbound.message, media_dir, &info.id.to_string());

    Some(StoredMessage {
        chat: info.source.chat.to_string(),
        id: info.id.to_string(),
        sender: info.source.sender.to_string(),
        // Names are resolved separately and joined by the store on read.
        sender_name: None,
        timestamp: info.timestamp.timestamp(),
        from_me: info.source.is_from_me,
        text,
        media_kind,
        media_path,
        reply_to_id,
        reply_to_text,
        reply_to_sender,
        // Newly arrived, so unseen until the chat is opened.
        read: false,
        revoked: false,
        mentioned: false,
        preview_url,
        preview_title,
        preview_desc,
        preview_thumb,
        status: None,
    })
}

/// The link preview a message carries, with its thumbnail written next to the
/// other media so the UI can show it.
fn link_preview(
    message: &wa::Message,
    media_dir: Option<&std::path::Path>,
    id: &str,
) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
    use whatsapp_rust::wacore::proto_helpers::MessageExt;
    let Some(text) = message
        .get_base_message()
        .extended_text_message
        .as_option()
    else {
        return (None, None, None, None);
    };
    // `matched_text` is the URL as it appeared in the message.
    let Some(url) = text.matched_text.clone() else {
        return (None, None, None, None);
    };
    let thumb = text.jpeg_thumbnail.as_ref().and_then(|bytes| {
        let dir = media_dir?;
        std::fs::create_dir_all(dir).ok()?;
        let path = dir.join(format!("{id}_thumb.jpg"));
        std::fs::write(&path, bytes).ok()?;
        Some(path.to_string_lossy().to_string())
    });
    (Some(url), text.title.clone(), text.description.clone(), thumb)
}

/// File extension for a downloaded media item.
fn extension_for(kind: &str, media_type: MediaType) -> &'static str {
    match kind {
        "image" => "jpg",
        "video" => "mp4",
        "audio" => "ogg",
        "document" => "bin",
        _ => match media_type {
            MediaType::Image => "jpg",
            _ => "bin",
        },
    }
}

/// Copies address-book names onto the LID form of the same address.
///
/// The address book is keyed by phone number, while messages in an
/// LID-addressed chat carry the LID. The library's own mapping table bridges
/// the two, so names already learned apply to existing history instead of only
/// to messages that arrive after this point.
fn backfill_lid_names(session_path: &std::path::Path, store: &MessageStore) {
    use std::collections::HashMap;

    let Ok(saved) = store.saved_names() else {
        return;
    };
    let by_phone: HashMap<&str, &str> = saved
        .iter()
        .filter_map(|(jid, name)| {
            jid.strip_suffix("@s.whatsapp.net")
                .map(|phone| (phone, name.as_str()))
        })
        .collect();
    if by_phone.is_empty() {
        return;
    }

    let Ok(conn) = rusqlite::Connection::open_with_flags(
        session_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return;
    };
    let mut by_lid: HashMap<String, String> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT lid, phone_number FROM lid_pn_mapping") {
        if let Ok(rows) = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for (lid, phone) in rows.flatten() {
                by_lid.insert(lid, phone);
            }
        }
    }

    for address in store.known_addresses().unwrap_or_default() {
        let Some((user, server)) = address.split_once('@') else {
            continue;
        };
        if server != "lid" {
            continue;
        }
        let bare = user.split(':').next().unwrap_or(user);
        let Some(phone) = by_lid.get(bare) else {
            continue;
        };
        match by_phone.get(phone.as_str()) {
            Some(name) => {
                let _ = store.set_saved_name(&address, name);
                let _ = store.set_saved_name(&format!("{bare}@lid"), name);
            }
            // Without a saved name, show the phone number instead of the LID,
            // which nobody can read.
            None => {
                let _ = store.set_name(&address, phone);
                let _ = store.set_name(&format!("{bare}@lid"), phone);
            }
        }
    }
}

/// The JIDs a message mentions, from whichever message type carries them.
fn message_context(message: &wa::Message) -> Option<&wa::ContextInfo> {
    use whatsapp_rust::wacore::proto_helpers::MessageExt;
    let base = message.get_base_message();
    base.extended_text_message
        .as_option()
        .and_then(|m| m.context_info.as_option())
        .or_else(|| {
            base.image_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            base.video_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            base.audio_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            base.document_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
}

/// The JIDs a message mentions.
fn mentioned_jids(message: &wa::Message) -> Vec<String> {
    message_context(message)
        .map(|c| c.mentioned_jid.clone())
        .unwrap_or_default()
}

/// Whether a message mentions us: directly, or everyone through @all.
fn mentions_me(message: &wa::Message, own: &[String]) -> bool {
    let Some(context) = message_context(message) else {
        return false;
    };
    if !context.group_mentions.is_empty() {
        return true;
    }
    context.mentioned_jid.iter().any(|mention| {
        let bare = mention.split(':').next().unwrap_or(mention);
        own.iter().any(|me| me == mention || me == bare)
    })
}

/// Rewrites `@<number>` mention tokens into the name we know for that JID.
///
/// The wire identifies a mention by its number; the UI shows the name, so the
/// stored text is the display form.
fn replace_mentions(text: &str, mentions: &[String], store: &MessageStore) -> String {
    let mut out = text.to_string();
    for jid in mentions {
        let user = jid.split('@').next().unwrap_or(jid);
        let user = user.split(':').next().unwrap_or(user);
        let bare = jid.split(':').next().unwrap_or(jid);
        let name = store
            .name_for(jid)
            .ok()
            .flatten()
            .or_else(|| store.name_for(bare).ok().flatten())
            .unwrap_or_else(|| user.to_string());
        out = out.replace(&format!("@{user}"), &format!("@{name}"));
    }
    out
}

/// Rewrites `@<digits>` tokens to a known name.
///
/// Used where no `mentioned_jid` is available: quoted text, and messages from
/// senders that omit it. The token is a bare number, so both the phone-number
/// and the LID form are tried.
fn resolve_mention_tokens(text: &str, store: &MessageStore) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start {
                let number = &text[start..end];
                let name = store
                    .name_for(&format!("{number}@s.whatsapp.net"))
                    .ok()
                    .flatten()
                    .or_else(|| store.name_for(&format!("{number}@lid")).ok().flatten());
                if let Some(name) = name {
                    out.push('@');
                    out.push_str(&name);
                    i = end;
                    continue;
                }
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// MIME type for an outgoing attachment, from its file extension.
fn mime_for(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" | "mov" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "ogg" | "opus" => "audio/ogg; codecs=opus",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_for_the_ui() {
        // Regression guard: these are emitted with `app.emit`, which fails
        // silently for a shape serde cannot represent.
        for event in [
            ServiceEvent::QrCode { code: "2@abc".into() },
            ServiceEvent::Connected,
            ServiceEvent::Disconnected,
            ServiceEvent::RetentionApplied { removed: 3 },
            ServiceEvent::NamesUpdated { count: 2 },
            ServiceEvent::Syncing { pending: 5 },
            ServiceEvent::Synced,
        ] {
            let json = serde_json::to_string(&event).expect("event must serialize");
            assert!(json.contains("\"kind\""), "missing tag: {json}");
        }

        let message = ServiceEvent::Message {
            message: Box::new(StoredMessage {
                chat: "a@s".into(),
                id: "1".into(),
                sender: "b@s".into(),
                sender_name: None,
                timestamp: 0,
                from_me: false,
                text: "hi".into(),
                media_kind: None,
                media_path: None,
                reply_to_id: None,
                reply_to_text: None,
                reply_to_sender: None,
                read: false,
                revoked: false,
                mentioned: false,
                preview_url: None,
                preview_title: None,
                preview_desc: None,
                preview_thumb: None,
                status: None,
            }),
        };
        let json = serde_json::to_string(&message).expect("message event must serialize");
        assert!(json.contains("\"message\""), "missing payload: {json}");
    }

    #[test]
    fn default_config_targets_its_data_dir() {
        let c = ServiceConfig::under("/tmp/example");
        assert_eq!(c.session_path, PathBuf::from("/tmp/example/session.db"));
        assert_eq!(c.messages_path, PathBuf::from("/tmp/example/messages.db"));
        assert_eq!(c.retention, Retention::default());
        assert!(!c.accept_full_history);
    }

    #[test]
    fn default_retention_is_bounded() {
        // The whole point of the rewrite: the default must not be unbounded.
        let r = Retention::default();
        assert!(r.max_age_hours.is_some());
        assert!(r.max_messages_per_chat.is_some());
    }

    #[test]
    fn secret_horizon_tracks_the_message_window() {
        // Keys must not outlive the messages they belong to, or the session
        // database grows far beyond the history we actually keep.
        let config = cache_config_for(&Retention {
            max_age_hours: Some(24),
            max_messages_per_chat: None,
        });
        let day = Duration::from_secs(24 * 3600);
        assert!(config.msg_secret_retention.text < day * 2);
        assert!(config.msg_secret_retention.poll_event < day * 2);
    }

    #[test]
    fn secret_horizon_has_a_floor() {
        // An edit can arrive shortly after its parent, so the horizon must not
        // collapse to zero for a very short retention window.
        let config = cache_config_for(&Retention {
            max_age_hours: Some(0),
            max_messages_per_chat: None,
        });
        assert!(config.msg_secret_retention.text >= Duration::from_secs(3600));
    }

    #[test]
    fn unlimited_retention_falls_back_to_the_library_default() {
        let config = cache_config_for(&Retention::unlimited());
        assert_eq!(
            config.msg_secret_retention.text,
            Duration::from_secs(30 * 86_400)
        );
    }
}
