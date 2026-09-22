//! The client service: connection lifecycle, typed events, and storage.
//!
//! This is the layer the UI talks to. It owns the protocol [`Bot`], converts
//! library events into [`ServiceEvent`]s the UI can render, and persists
//! messages through the [`MessageStore`] so retention stays enforced.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use tokio::sync::broadcast;
use whatsapp_rust::{prelude::*, wacore::types::events::Event};

use crate::{
    history::HistoryPolicy,
    store::{MessageStore, Retention, StoredMessage},
};

/// Events the UI reacts to.
#[derive(Debug, Clone)]
pub enum ServiceEvent {
    /// A pairing QR is ready to display.
    QrCode(String),
    Connected,
    Disconnected,
    /// A message was received and stored.
    Message(StoredMessage),
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
        }
    }
}

/// A running account client.
///
/// Dropping this stops the background task and closes the stores.
pub struct Service {
    client: Arc<Client>,
    store: Arc<MessageStore>,
    events: broadcast::Sender<ServiceEvent>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl Service {
    /// Connects an account, pairing first if it has no session yet.
    pub async fn start(config: ServiceConfig) -> Result<Self> {
        let store = Arc::new(MessageStore::open(
            &config.messages_path,
            config.retention,
        )?);
        let (events, _) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let policy = if config.accept_full_history {
            HistoryPolicy::accept_everything()
        } else {
            HistoryPolicy::default()
        };

        let store_for_events = store.clone();
        let events_for_events = events.clone();

        let bot = Bot::builder()
            .with_backend(SqliteStore::new(config.session_path.to_string_lossy().as_ref()).await?)
            .with_history_sync_admission(policy)
            .on_qr_code({
                let events = events.clone();
                move |code, _timeout| {
                    let events = events.clone();
                    async move {
                        let _ = events.send(ServiceEvent::QrCode(code));
                    }
                }
            })
            .on_connected({
                let events = events.clone();
                move |_client| {
                    let events = events.clone();
                    async move {
                        let _ = events.send(ServiceEvent::Connected);
                    }
                }
            })
            .on_event_for(
                &[EventKind::Messages, EventKind::Disconnected],
                move |event, _client| {
                    let store = store_for_events.clone();
                    let events = events_for_events.clone();
                    async move {
                        match event.as_ref() {
                            Event::Messages(batch) => {
                                for inbound in batch.messages.iter() {
                                    let Some(message) = to_stored(inbound) else {
                                        continue;
                                    };
                                    let _ = store.upsert(&message);
                                    let _ = events.send(ServiceEvent::Message(message));
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

        Ok(Self {
            client,
            store,
            events,
            shutdown: shutdown_tx,
        })
    }

    /// Subscribes to service events.
    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.events.subscribe()
    }

    /// Sends a text message to a chat.
    pub async fn send_text(&self, chat: &str, text: impl Into<String>) -> Result<()> {
        let to: Jid = chat.parse()?;
        self.client.send_text(to, text).await?;
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

/// Converts a protocol message into a storable one.
///
/// Returns `None` for messages with no text body: protocol traffic and media
/// without a caption would otherwise fill the store with empty rows.
fn to_stored(inbound: &InboundMessage) -> Option<StoredMessage> {
    let text = inbound.message.text_content()?.to_string();
    let info = &inbound.info;
    Some(StoredMessage {
        chat: info.source.chat.to_string(),
        id: info.id.to_string(),
        sender: info.source.sender.to_string(),
        timestamp: info.timestamp.timestamp(),
        from_me: info.source.is_from_me,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
