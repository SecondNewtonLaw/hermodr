//! Core of the Hermóðr native client.
//!
//! Hermóðr talks the WhatsApp multi-device protocol directly instead of
//! embedding WhatsApp Web. That removes the webview entirely, and — more
//! importantly — makes history sync a decision this program gets to make.

pub mod history;
pub mod service;
pub mod store;

pub use history::HistoryPolicy;
pub use service::{Service, ServiceConfig, ServiceEvent};
pub use store::{ChatSummary, MessageStore, Retention, StoredMessage};
