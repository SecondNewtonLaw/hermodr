# Hermóðr

A native WhatsApp desktop client. It speaks the WhatsApp multi-device protocol
directly instead of embedding WhatsApp Web in a browser engine.

## Why

The obvious way to build a WhatsApp desktop client is to wrap WhatsApp Web in a
webview, which is what Altus (and most alternatives) do. That approach has a
cost: WhatsApp Web treats a desktop client like another browser tab, so it syncs
the account's **entire message history** into local storage and then keeps it in
memory. On a large account that means tens of gigabytes downloaded, multi-
gigabyte memory use, and a client that gets slower the more history you have.

Hermóðr takes the other path. Because it implements the protocol itself:

- **History sync is a decision this program makes.** The deep history sync is
  refused by default; recent messages, contacts and on-demand fetches still
  arrive. `accept_full_history` opts in to the old behaviour.
- **Message storage is ours.** History lives in a small SQLite database with a
  configurable retention window (24 hours and 500 messages per chat by default).
  Nothing unbounded accumulates.
- **No browser engine for WhatsApp.** No WebKit, no per-tab network processes,
  no compositing workarounds. The only webview is the one rendering this app's
  own UI.

## The name

**Hermóðr** (Old Norse `[ˈhermˌoːðz̠]`, anglicized *Hermod*, roughly
**"HAIR-moth"**): the `ð` is a voiced *th*, as in **"the"**, so the ending
sounds like "moth" said with a *th* rather than a hard *t*.

The name is Old Norse, from `herr` ("war, host") + `móðr` ("spirit, courage,
mood"), literally **"war-spirit"**. In Norse mythology Hermóðr is a son of
Odin and the brother of Baldr, and he is best known as *the messenger*: when
Baldr is killed, Hermóðr rides Odin's horse Sleipnir for nine nights down to
Hel to plead for his brother's return. A god whose job is to carry a message
from one realm to another is a fitting namesake for a chat client.

## Measured impact

Against a real account, comparing the old webview approach with this one:

| Metric | Altus (WhatsApp Web in a webview) | Hermóðr |
| --- | --- | --- |
| CPU, steady state | ~200% of one core, sustained | **2-4%** |
| Memory | ~2.2 GB, climbing to ~23 GB | **35 MB** |
| Memory under live traffic | 4.2 GB | **53 MB** |
| History downloaded at pairing | entire account (~20 GB) | none |
| Message history stored | 604k+ rows, 774 MB | bounded by retention |

The memory figures are Hermóðr's protocol/service process. The window adds a
small WebKit UI process on top; the point is that nothing history-sized ever
accumulates in either.

CPU was sampled with `pidstat` in 30-second windows. Altus held 130-220% of one
core the entire time and its RSS kept climbing toward the full 23 GB history, so
it never reaches a true idle. Hermóðr sat between 1.5% and 4.5%.

The session database also needed bounding: decryption secrets for edits and
reactions default to a 30-day horizon, which grew one profile to 97 MB across
399,620 rows. The horizon now tracks the message retention window, and existing
profiles are reclaimed on startup (**97 MB → 5.8 MB**).

## Architecture

```
crates/hermodr-core/    protocol client, storage, retention
  history.rs            which history-sync chunks to accept
  store.rs              SQLite message store + retention policy
  service.rs            connection lifecycle, typed event stream
src-tauri/              Tauri shell: commands and event forwarding
src/                    Svelte 5 UI (chat list, conversation, pairing)
```

`hermodr-core` is built on [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust),
a pure-Rust implementation of the WhatsApp multi-device protocol.

## Building

`whatsapp-rust` is pinned to a revision rather than the crates.io release,
because per-chunk history control (`HistorySyncAdmission`) exists only there.

**It is consumed from a local checkout**, expected at `../whatsapp-rust`:

```console
git clone https://github.com/oxidezap/whatsapp-rust
git -C whatsapp-rust checkout 9eb43b9bc2bc561cc34a672b4295528bf4c2d733
```

A git dependency is not used instead because that repository's own
`.cargo/config.toml` injects nightly-only rustflags which a git dependency would
inherit. As a path dependency those flags do not apply, and the pinned revision
builds on stable Rust.

Then:

```console
pnpm install
pnpm tauri dev
```

Requirements: Rust 1.94+ (stable), Node with pnpm, and the usual Tauri Linux
dependencies (WebKitGTK 4.1, GTK 3).

On Wayland, WebKitGTK's DMA-BUF renderer fails with `Gdk Error 71`. The app sets
`WEBKIT_DISABLE_DMABUF_RENDERER=1` itself, so no manual configuration is needed.

## Diagnostics

Two binaries exercise the core without the UI:

```console
cargo run -p hermodr-core --bin spike          # pair by QR, print a terminal QR
cargo run -p hermodr-core --bin service-check  # store messages, show retention
```

`spike` renders its pairing code as Unicode blocks, so it needs no image viewer.
Set `SPIKE_HISTORY=accept` to compare behaviour when the full history is allowed.

## Media

Downloaded media is written to the folder set in Settings, which defaults to the
app data directory. The asset protocol is scoped to whatever folder is
configured, so a custom location is served to the UI as well.

Attachments are read in the webview and sent base64-encoded, because a webview
cannot hand out a real filesystem path. That is fine for the images and
documents a picker is normally used for, but it is not a good fit for very large
files.

## Testing

```console
cargo test -p hermodr-core
```

## Status

Working:

- Pairing by QR, reusing the stored session on later launches
- Receiving and sending text, with quotes/replies
- Images and documents received inline, and sent from the composer
- Group and contact names, resolved from push names and group queries
- Unread counts per chat, and a read indicator on messages
- Replies in both directions, with the quoted author attributed correctly
- Deleted messages kept and shown as removed rather than vanishing
- Retention enforcement, and a configurable media folder

Not yet implemented: audio/video playback, group administration, calls,
notifications, multiple accounts, and the tray icon.
