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
    media::{self, DocumentOptions, ImageOptions},
    prelude::*,
    wacore::msg_secret::MsgSecretRetention,
    wacore::types::events::Event,
    wacore_binary::builder::NodeBuilder,
    CacheConfig,
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
                move |_client| {
                    let events = events.clone();
                    let qr_state = qr_state.clone();
                    let connected_state = connected_state.clone();
                    async move {
                        connected_state.store(true, Ordering::SeqCst);
                        // The code is spent once paired.
                        *qr_state.lock().unwrap() = None;
                        let _ = events.send(ServiceEvent::Connected);
                    }
                }
            })
            .on_event_for(
                &[EventKind::Messages, EventKind::Disconnected],
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
                                for inbound in batch.messages.iter() {
                                    // The envelope carries the sender's display
                                    // name, which is the only name source
                                    // available without a contacts query.
                                    let push_name = inbound.info.push_name.to_string();
                                    let chat = inbound.info.source.chat.to_string();
                                    let sender = inbound.info.source.sender.to_string();
                                    if !push_name.is_empty() {
                                        let _ = store.set_name(&sender, &push_name);
                                        // A one-to-one chat is named after its
                                        // contact, so the same name applies.
                                        if !inbound.info.source.is_group {
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

                                    let Some(message) = incoming_message(
                                        inbound,
                                        client.as_deref(),
                                        media_dir.as_deref(),
                                    )
                                    .await
                                    else {
                                        continue;
                                    };
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
                            _ => {}
                        }
                    }
                },
            )
            .build()
            .await?;

        let client = bot.client();

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
    /// The sent message is stored and dispatched locally. WhatsApp does not echo
    /// a message back to the device that sent it, so without this the sender
    /// would not see their own message until the store was next reloaded.
    pub async fn send_text(&self, chat: &str, text: impl Into<String>) -> Result<()> {
        let to: Jid = chat.parse()?;
        let text = text.into();
        let result = self.client.send_text(to, text.clone()).await?;

        let message = StoredMessage {
            chat: chat.to_string(),
            id: result.message_id.clone(),
            sender: chat.to_string(),
            sender_name: None,
            timestamp: unix_now(),
            from_me: true,
            text,
            media_kind: None,
            media_path: None,
            reply_to_id: None,
            reply_to_text: None,
            // Not `true`: we cannot know whether the recipient has read it, and
            // claiming so shows a read marker that is not true.
            read: false,
            revoked: false,
        };
        self.store.upsert(&message)?;
        let _ = self.events.send(ServiceEvent::Message { message: Box::new(message) });
        Ok(())
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
    ) -> Result<()> {
        let to: Jid = chat.parse()?;
        // The quoted author must be the address without a device suffix: a
        // participant like `123:98@lid` is not resolvable by recipients, who
        // then attribute the quoted message to the sender of the reply.
        let sender: Jid = reply_to_sender.parse::<Jid>()?.to_non_ad();
        let text = text.into();

        use whatsapp_rust::wacore::proto_helpers::build_quote_context_with_info;
        let quoted = wa::Message::text(reply_to_text);
        let context =
            build_quote_context_with_info(reply_to_id, &sender, &to, &to, &quoted);

        use whatsapp_rust::wacore::proto_helpers::MessageBuilderExt;
        let message = wa::Message::text_with_context(text.clone(), context);
        let result = self.client.send_message(to, message).await?;

        let stored = StoredMessage {
            chat: chat.to_string(),
            id: result.message_id.clone(),
            sender: chat.to_string(),
            sender_name: None,
            timestamp: unix_now(),
            from_me: true,
            text,
            media_kind: None,
            media_path: None,
            reply_to_id: Some(reply_to_id.to_string()),
            reply_to_text: Some(reply_to_text.to_string()),
            read: false,
            revoked: false,
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

        let is_image = matches!(
            std::path::Path::new(&file_name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("jpg" | "jpeg" | "png" | "gif" | "webp")
        );

        let upload = self
            .client
            .upload(bytes.clone(), if is_image { MediaType::Image } else { MediaType::Document }, Default::default())
            .await?;

        let (message, kind) = if is_image {
            (
                media::image_message(
                    upload,
                    ImageOptions {
                        caption: caption.clone(),
                        ..Default::default()
                    },
                ),
                "image",
            )
        } else {
            (
                media::document_message(
                    upload,
                    DocumentOptions {
                        file_name: Some(file_name.clone()),
                        caption: caption.clone(),
                        ..Default::default()
                    },
                ),
                "document",
            )
        };

        let result = self.client.send_message(to, message).await?;

        // Keep our own copy so the sender sees what they sent.
        let mut stored_path = None;
        if let Some(dir) = &self.media_dir() {
            if std::fs::create_dir_all(dir).is_ok() {
                let dest = dir.join(format!("{}.{}", result.message_id, if is_image { "jpg" } else { "bin" }));
                if std::fs::write(&dest, &bytes).is_ok() {
                    stored_path = Some(dest.to_string_lossy().to_string());
                }
            }
        }

        let stored = StoredMessage {
            chat: chat.to_string(),
            id: result.message_id.clone(),
            sender: chat.to_string(),
            sender_name: None,
            timestamp: unix_now(),
            from_me: true,
            text: caption.unwrap_or_default(),
            media_kind: Some(kind.to_string()),
            media_path: stored_path,
            reply_to_id: None,
            reply_to_text: None,
            read: false,
            revoked: false,
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
fn quote_of(message: &wa::Message) -> Option<(String, String)> {
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
    let quoted = context.quoted_message.as_option()?;
    let text = quoted.text_content().unwrap_or("[media]").to_string();
    Some((id, text))
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
    let (reply_to_id, reply_to_text) = quote_of(&inbound.message)
        .map(|(id, text)| (Some(id), Some(text)))
        .unwrap_or((None, None));

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
        // Newly arrived, so unseen until the chat is opened.
        read: false,
        revoked: false,
    })
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
                read: false,
                revoked: false,
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
