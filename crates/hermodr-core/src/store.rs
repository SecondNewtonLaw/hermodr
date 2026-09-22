//! Message and chat storage.
//!
//! Hermóðr keeps its own history rather than relying on the protocol library,
//! which stores none. That makes retention ours to enforce: the [`Retention`]
//! policy bounds what is kept, so the store cannot grow without limit the way a
//! synced WhatsApp Web profile does.

use std::{path::Path, sync::Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

/// How much history to keep locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Drop messages older than this many hours. `None` keeps everything.
    pub max_age_hours: Option<u32>,
    /// Cap on stored messages per chat. `None` means no cap.
    pub max_messages_per_chat: Option<u32>,
}

impl Default for Retention {
    fn default() -> Self {
        // A small window by default: enough for current conversations without
        // re-creating the multi-gigabyte history the web client pulled in.
        Self {
            max_age_hours: Some(24),
            max_messages_per_chat: Some(500),
        }
    }
}

impl Retention {
    /// Keep everything, matching the default WhatsApp client behaviour.
    pub fn unlimited() -> Self {
        Self {
            max_age_hours: None,
            max_messages_per_chat: None,
        }
    }

    /// The oldest timestamp still inside the window, if one is set.
    fn oldest_allowed(&self) -> Option<i64> {
        self.max_age_hours.map(|hours| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            now - i64::from(hours) * 3600
        })
    }
}

/// A stored message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    pub chat: String,
    pub id: String,
    pub sender: String,
    pub timestamp: i64,
    pub from_me: bool,
    pub text: String,
}

/// A chat summary derived from stored messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatSummary {
    pub chat: String,
    pub last_message_at: i64,
    pub last_text: String,
    pub message_count: i64,
}

/// SQLite-backed message store.
pub struct MessageStore {
    conn: Mutex<Connection>,
    retention: Retention,
}

impl MessageStore {
    /// Opens (or creates) the store at `path`.
    pub fn open(path: &Path, retention: Retention) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening message store at {}", path.display()))?;

        // WAL keeps reads from blocking the writer, which matters because
        // messages arrive while the UI is querying.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                 chat      TEXT NOT NULL,
                 id        TEXT NOT NULL,
                 sender    TEXT NOT NULL,
                 timestamp INTEGER NOT NULL,
                 from_me   INTEGER NOT NULL,
                 text      TEXT NOT NULL,
                 PRIMARY KEY (chat, id)
             );
             CREATE INDEX IF NOT EXISTS idx_messages_chat_time
                 ON messages (chat, timestamp DESC);",
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
            retention,
        })
    }

    /// Records a message, replacing any existing row with the same id.
    pub fn upsert(&self, message: &StoredMessage) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (chat, id, sender, timestamp, from_me, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(chat, id) DO UPDATE SET
                 sender = excluded.sender,
                 timestamp = excluded.timestamp,
                 from_me = excluded.from_me,
                 text = excluded.text",
            params![
                message.chat,
                message.id,
                message.sender,
                message.timestamp,
                message.from_me as i32,
                message.text,
            ],
        )?;
        Ok(())
    }

    /// Messages in a chat, newest first.
    pub fn messages_for(&self, chat: &str, limit: u32) -> Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT chat, id, sender, timestamp, from_me, text
             FROM messages WHERE chat = ?1
             ORDER BY timestamp DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![chat, limit], |row| {
            Ok(StoredMessage {
                chat: row.get(0)?,
                id: row.get(1)?,
                sender: row.get(2)?,
                timestamp: row.get(3)?,
                from_me: row.get::<_, i32>(4)? != 0,
                text: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// One summary per chat, most recently active first.
    pub fn chats(&self) -> Result<Vec<ChatSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT chat,
                    MAX(timestamp) AS last_message_at,
                    COUNT(*) AS message_count
             FROM messages GROUP BY chat ORDER BY last_message_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        let mut summaries = Vec::new();
        for row in rows {
            let (chat, last_message_at, message_count) = row?;
            // The preview is fetched separately so the aggregate query stays simple.
            let last_text = conn
                .query_row(
                    "SELECT text FROM messages WHERE chat = ?1
                     ORDER BY timestamp DESC LIMIT 1",
                    params![chat],
                    |r| r.get::<_, String>(0),
                )
                .unwrap_or_default();
            summaries.push(ChatSummary {
                chat,
                last_message_at,
                last_text,
                message_count,
            });
        }
        Ok(summaries)
    }

    /// Applies the retention policy, returning how many messages were dropped.
    ///
    /// Called after writes rather than on a timer so the bound holds even if the
    /// process is interrupted.
    pub fn enforce_retention(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut removed = 0;

        if let Some(oldest) = self.retention.oldest_allowed() {
            removed += conn.execute(
                "DELETE FROM messages WHERE timestamp < ?1",
                params![oldest],
            )?;
        }

        if let Some(cap) = self.retention.max_messages_per_chat {
            // Rank within each chat and drop everything past the cap.
            removed += conn.execute(
                "DELETE FROM messages WHERE (chat, id) IN (
                     SELECT chat, id FROM (
                         SELECT chat, id,
                                ROW_NUMBER() OVER (
                                    PARTITION BY chat ORDER BY timestamp DESC
                                ) AS rank
                         FROM messages
                     ) WHERE rank > ?1
                 )",
                params![cap],
            )?;
        }

        Ok(removed)
    }

    /// Total stored messages, used by tests and diagnostics.
    pub fn count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn store(retention: Retention) -> MessageStore {
        // In-memory keeps tests independent and fast.
        let mut s = MessageStore::open(Path::new(":memory:"), retention).unwrap();
        s.conn.get_mut().unwrap().execute_batch(
            "DROP TABLE IF EXISTS messages;
             CREATE TABLE messages (
                 chat TEXT NOT NULL, id TEXT NOT NULL, sender TEXT NOT NULL,
                 timestamp INTEGER NOT NULL, from_me INTEGER NOT NULL,
                 text TEXT NOT NULL, PRIMARY KEY (chat, id));",
        ).unwrap();
        s
    }

    fn msg(chat: &str, id: &str, age_hours: i64, text: &str) -> StoredMessage {
        StoredMessage {
            chat: chat.into(),
            id: id.into(),
            sender: "them".into(),
            timestamp: now() - age_hours * 3600,
            from_me: false,
            text: text.into(),
        }
    }

    #[test]
    fn stores_and_reads_messages() {
        let s = store(Retention::unlimited());
        s.upsert(&msg("a@s", "1", 0, "hello")).unwrap();
        let got = s.messages_for("a@s", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "hello");
    }

    #[test]
    fn upsert_replaces_same_id() {
        let s = store(Retention::unlimited());
        s.upsert(&msg("a@s", "1", 0, "first")).unwrap();
        s.upsert(&msg("a@s", "1", 0, "edited")).unwrap();
        let got = s.messages_for("a@s", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "edited");
    }

    #[test]
    fn drops_messages_older_than_the_window() {
        let s = store(Retention {
            max_age_hours: Some(24),
            max_messages_per_chat: None,
        });
        s.upsert(&msg("a@s", "old", 48, "ancient")).unwrap();
        s.upsert(&msg("a@s", "new", 1, "recent")).unwrap();
        assert_eq!(s.enforce_retention().unwrap(), 1);
        let got = s.messages_for("a@s", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "recent");
    }

    #[test]
    fn caps_messages_per_chat() {
        let s = store(Retention {
            max_age_hours: None,
            max_messages_per_chat: Some(3),
        });
        for i in 0..10 {
            s.upsert(&msg("a@s", &i.to_string(), i, &format!("m{i}")))
                .unwrap();
        }
        s.enforce_retention().unwrap();
        assert_eq!(s.count().unwrap(), 3);
        // The newest survive.
        let got = s.messages_for("a@s", 10).unwrap();
        assert_eq!(got[0].text, "m0");
    }

    #[test]
    fn cap_applies_per_chat() {
        let s = store(Retention {
            max_age_hours: None,
            max_messages_per_chat: Some(2),
        });
        for i in 0..5 {
            s.upsert(&msg("a@s", &i.to_string(), i, "x")).unwrap();
            s.upsert(&msg("b@s", &i.to_string(), i, "y")).unwrap();
        }
        s.enforce_retention().unwrap();
        assert_eq!(s.messages_for("a@s", 99).unwrap().len(), 2);
        assert_eq!(s.messages_for("b@s", 99).unwrap().len(), 2);
    }

    #[test]
    fn summaries_are_newest_first() {
        let s = store(Retention::unlimited());
        s.upsert(&msg("old@s", "1", 10, "older")).unwrap();
        s.upsert(&msg("new@s", "1", 1, "newer")).unwrap();
        let chats = s.chats().unwrap();
        assert_eq!(chats[0].chat, "new@s");
        assert_eq!(chats[0].last_text, "newer");
        assert_eq!(chats[1].chat, "old@s");
    }

    #[test]
    fn unlimited_retention_keeps_everything() {
        let s = store(Retention::unlimited());
        for i in 0..50 {
            s.upsert(&msg("a@s", &i.to_string(), i * 100, "x")).unwrap();
        }
        assert_eq!(s.enforce_retention().unwrap(), 0);
        assert_eq!(s.count().unwrap(), 50);
    }
}
