//! Phase 0 spike: prove what `whatsapp-rust` can do before committing to a rewrite.
//!
//! Goals, in order:
//!   1. Build against stable Rust (no nightly `simd` feature).
//!   2. Pair by QR against a real account.
//!   3. Refuse the initial history sync, then measure memory.
//!   4. Send and receive a text message.
//!
//! Run with: `cargo run -p hermodr-core --bin spike`
//!
//! Set `SPIKE_HISTORY=accept` to allow the initial history sync, so the
//! difference between the two modes can be measured.

use std::{env, time::Duration};

use anyhow::Result;
use whatsapp_rust::prelude::*;
use whatsapp_rust::wacore::types::events::Event;

/// Pure Rust, no Go toolchain. `whatsapp-rust` persists protocol/crypto state
/// only; message history is never stored unless we choose to.
fn session_url() -> String {
    env::args()
        .nth(1)
        .unwrap_or_else(|| "sqlite:spike.db".to_string())
}

/// Whether to accept the initial history sync.
///
/// Default is to skip it: history is exactly what made the v1 web client pull
/// the account's entire history into a WebKit heap.
fn should_skip_history() -> bool {
    !matches!(
        env::var("SPIKE_HISTORY").as_deref(),
        Ok("accept") | Ok("1") | Ok("true")
    )
}

/// Reads this process's resident set size, so memory is reported rather than assumed.
fn rss_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}

fn report_rss(phase: &str) {
    match rss_mb() {
        Some(mb) => println!("[spike] RSS after {phase}: {mb} MB"),
        None => println!("[spike] RSS after {phase}: unavailable"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let db = session_url();
    let skip = should_skip_history();

    println!("[spike] session: {db}");
    println!(
        "[spike] history sync: {}",
        if skip {
            "SKIPPED (default)"
        } else {
            "ACCEPTED (SPIKE_HISTORY set)"
        }
    );
    report_rss("startup");

    let mut builder = Bot::builder()
        .with_backend(SqliteStore::new(&db).await?)
        .on_qr_code(|code, _timeout| async move {
            println!("\n[spike] Scan to pair:\n{code}\n");
        })
        .on_connected(|_client| async {
            println!("[spike] connected");
            report_rss("connect");
        })
        .on_event_for(
            &[EventKind::Messages, EventKind::PairSuccess],
            |event, _client| async move {
                match event.as_ref() {
                    Event::Messages(batch) => {
                        for inbound in batch.messages.iter() {
                            let text = inbound
                                .message
                                .text_content()
                                .unwrap_or("<non-text>")
                                .to_string();
                            println!("[spike] message from {}: {text}", inbound.info.source.sender);
                        }
                    }
                    Event::PairSuccess(_) => println!("[spike] PAIRED successfully"),
                    _ => {}
                }
            },
        );

    if skip {
        builder = builder.skip_history_sync();
    }

    let bot = builder.build().await?;

    // Report growth while idle. With history refused these numbers should stay flat.
    tokio::spawn(async {
        for i in 1..=6 {
            tokio::time::sleep(Duration::from_secs(10)).await;
            report_rss(&format!("{}s idle", i * 10));
        }
    });

    bot.run().await;
    Ok(())
}
