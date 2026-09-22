//! The client service: connection lifecycle, typed events, and storage.
//!
//! This is the layer the UI talks to. It owns the protocol [`Bot`], converts
//! library events into [`ServiceEvent`]s the UI can render, and persists
//! messages through the [`MessageStore`] so retention stays enforced.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use tokio::sync::broadcast;
use whatsapp_rust::{
    prelude::*, wacore::msg_secret::MsgSecretRetention, wacore::types::events::Event, CacheConfig,
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

        let store_for_events = store.clone();
        let events_for_events = events.clone();

        let bot = Bot::builder()
            .with_backend(SqliteStore::new(config.session_path.to_string_lossy().as_ref()).await?)
            .with_history_sync_admission(policy)
            .with_cache_config(cache_config_for(&config.retention))
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
