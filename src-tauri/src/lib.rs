//! Tauri shell for Hermóðr.
//!
//! The window only renders our own UI; WhatsApp is spoken natively by
//! [`hermodr_core`]. There is no webview pointed at a remote site, so none of
//! the history, memory, or compositing problems of that approach apply.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

use hermodr_core::{ChatSummary, Retention, Service, ServiceConfig, StoredMessage};
use tauri::{AppHandle, Emitter, Manager, State};

/// Event name the frontend listens on for service updates.
const SERVICE_EVENT: &str = "service-event";

/// Settings the UI can change.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UiSettings {
    pub retention: Retention,
    /// Whether to pull the account's entire history during pairing.
    pub accept_full_history: bool,
    /// Where downloaded media is stored. Empty disables downloads.
    pub media_dir: Option<String>,
}

struct AppState {
    service: Mutex<Option<Arc<Service>>>,
    settings: Mutex<UiSettings>,
}

impl AppState {
    fn service(&self) -> Result<Arc<Service>, String> {
        self.service
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "not connected yet".to_string())
    }
}

/// Where this account's data lives.
fn data_dir(app: &AppHandle) -> std::path::PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn config_for(app: &AppHandle, settings: &UiSettings) -> ServiceConfig {
    let default_media = data_dir(app).join("media");
    ServiceConfig {
        session_path: data_dir(app).join("session.db"),
        messages_path: data_dir(app).join("messages.db"),
        retention: settings.retention,
        accept_full_history: settings.accept_full_history,
        // An unset or empty setting falls back to the app data directory.
        media_dir: settings
            .media_dir
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or(Some(default_media)),
    }
}

/// Snapshot of the connection state, for the UI's initial render.
///
/// The pairing code is issued during startup, before the event listener is
/// attached, so a subscriber can miss it. Returning the current values lets the
/// UI recover instead of showing a blank pairing screen forever.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConnectionState {
    pub started: bool,
    pub connected: bool,
    pub qr: Option<String>,
}

#[tauri::command]
fn connection_state(state: State<'_, AppState>) -> ConnectionState {
    let service = state.service.lock().unwrap().clone();
    match service {
        Some(service) => ConnectionState {
            started: true,
            connected: service.is_connected(),
            qr: service.current_qr(),
        },
        None => ConnectionState {
            started: false,
            connected: false,
            qr: None,
        },
    }
}

/// Connects the account, pairing by QR the first time.
///
/// Returns once the service is running; the QR code and connection state arrive
/// as [`SERVICE_EVENT`] messages so the UI can render them as they happen.
#[tauri::command]
async fn connect(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if state.service.lock().unwrap().is_some() {
        return Ok(());
    }

    let settings = state.settings.lock().unwrap().clone();
    let config = config_for(&app, &settings);

    let (service, mut events) = Service::start(config)
        .await
        .map_err(|e| format!("failed to start service: {e}"))?;
    let service = Arc::new(service);

    // `events` was registered before the connection attempt, so the pairing code
    // cannot slip through the gap between starting and subscribing.
    let emitter = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let _ = emitter.emit(SERVICE_EVENT, &event);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                    // A slow consumer missed some events. Messages are in the
                    // store regardless, so the UI can recover by refetching.
                    eprintln!("[hermodr] dropped {dropped} service event(s)");
                }
                Err(_) => break,
            }
        }
    });

    // A media folder outside the app data directory still has to be readable by
    // the UI, so the asset scope is widened to whatever was configured.
    if let Some(dir) = service.media_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = app.asset_protocol_scope().allow_directory(&dir, true);
    }

    *state.service.lock().unwrap() = Some(service);

    Ok(())
}

/// Stored messages for a chat, newest first.
#[tauri::command]
fn messages(
    state: State<'_, AppState>,
    chat: String,
    limit: Option<u32>,
) -> Result<Vec<StoredMessage>, String> {
    state
        .service()?
        .messages(&chat, limit.unwrap_or(200))
        .map_err(|e| e.to_string())
}

/// Chat summaries, most recently active first.
#[tauri::command]
fn chats(state: State<'_, AppState>) -> Result<Vec<ChatSummary>, String> {
    state.service()?.chats().map_err(|e| e.to_string())
}

/// Resolves display names for chats that still show a raw number.
///
/// Returns how many were resolved; the UI refreshes when that is non-zero.
#[tauri::command]
async fn resolve_names(state: State<'_, AppState>) -> Result<usize, String> {
    let service = state.service()?;
    service.resolve_missing_names().await.map_err(|e| e.to_string())
}

/// Marks a chat as read. Returns how many messages were newly marked.
#[tauri::command]
fn mark_read(state: State<'_, AppState>, chat: String) -> Result<usize, String> {
    state.service()?.mark_read(&chat).map_err(|e| e.to_string())
}

/// Sends a text message quoting an earlier one.
#[tauri::command]
async fn send_reply(
    state: State<'_, AppState>,
    chat: String,
    text: String,
    reply_to_id: String,
    reply_to_sender: String,
    reply_to_text: String,
) -> Result<(), String> {
    let service = state.service()?;
    service
        .send_reply(&chat, text, &reply_to_id, &reply_to_sender, &reply_to_text)
        .await
        .map_err(|e| e.to_string())
}

/// Sends an attachment as an image or document.
///
/// The file arrives base64-encoded because the webview cannot hand out a real
/// filesystem path, and the plugin that could is not usable alongside the
/// pinned Tauri checkout.
#[tauri::command]
async fn send_media(
    state: State<'_, AppState>,
    chat: String,
    name: String,
    data: String,
    caption: Option<String>,
) -> Result<(), String> {
    let bytes = BASE64.decode(data.as_bytes()).map_err(|e| e.to_string())?;
    let service = state.service()?;
    service
        .send_media(&chat, &name, bytes, caption)
        .await
        .map_err(|e| e.to_string())
}

/// Sends a text message to a chat.
#[tauri::command]
async fn send_text(state: State<'_, AppState>, chat: String, text: String) -> Result<(), String> {
    let service = state.service()?;
    service
        .send_text(&chat, text)
        .await
        .map_err(|e| e.to_string())
}

/// Renders a pairing code as SVG for the UI to display.
#[tauri::command]
fn qr_svg(value: String) -> Result<String, String> {
    hermodr_core::qr_svg(&value).map_err(|e| e.to_string())
}

/// Current settings.
#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> UiSettings {
    state.settings.lock().unwrap().clone()
}

/// Updates settings. Takes effect on the next connection.
#[tauri::command]
fn set_settings(state: State<'_, AppState>, settings: UiSettings) {
    *state.settings.lock().unwrap() = settings;
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // WebKitGTK's DMA-BUF renderer fails to create GBM buffers under Wayland
    // (Hyprland), aborting with "Gdk Error 71". This affects our own UI webview
    // as much as it did the old one.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    tauri::Builder::default()
        .setup(|app| {
            app.manage(AppState {
                service: Mutex::new(None),
                settings: Mutex::new(UiSettings::default()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connection_state,
            connect,
            messages,
            chats,
            resolve_names,
            mark_read,
            send_reply,
            send_media,
            send_text,
            qr_svg,
            get_settings,
            set_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
