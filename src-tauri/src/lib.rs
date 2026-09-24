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

use hermodr_core::{ChatSummary, Retention, Service, ServiceConfig, ServiceEvent, StoredMessage};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

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
    accounts: Mutex<AccountsFile>,
}

/// One signed-in account.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Account {
    pub id: String,
    pub label: String,
}

/// The account list, persisted next to the app config.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct AccountsFile {
    accounts: Vec<Account>,
    active: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountsView {
    pub accounts: Vec<Account>,
    pub active: Option<String>,
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

/// Where an account's files live. The first account keeps the old layout so an
/// existing single-account install is adopted in place.
fn account_base(app: &AppHandle, id: &str) -> std::path::PathBuf {
    if id == "default" {
        data_dir(app)
    } else {
        data_dir(app).join("accounts").join(id)
    }
}

fn accounts_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("accounts.json")
}

fn save_accounts(app: &AppHandle, file: &AccountsFile) {
    let path = accounts_path(app);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(file) {
        let _ = std::fs::write(path, json);
    }
}

/// Loads the account list, adopting an existing single-account install the
/// first time.
fn load_accounts(app: &AppHandle) -> AccountsFile {
    if let Ok(json) = std::fs::read_to_string(accounts_path(app)) {
        if let Ok(file) = serde_json::from_str::<AccountsFile>(&json) {
            return file;
        }
    }
    let mut file = AccountsFile::default();
    if data_dir(app).join("session.db").exists() {
        file.accounts.push(Account {
            id: "default".into(),
            label: "WhatsApp".into(),
        });
        file.active = Some("default".into());
        save_accounts(app, &file);
    }
    file
}

fn active_account(state: &AppState) -> Option<String> {
    state.accounts.lock().unwrap().active.clone()
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn config_for(app: &AppHandle, settings: &UiSettings, account: &str) -> ServiceConfig {
    let base = account_base(app, account);
    let default_media = base.join("media");
    ServiceConfig {
        session_path: base.join("session.db"),
        messages_path: base.join("messages.db"),
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

/// The event that tells a fallen-behind UI what the current state is.
fn resync_event(service: &Service) -> ServiceEvent {
    if service.is_connected() {
        ServiceEvent::Connected
    } else if let Some(code) = service.current_qr() {
        ServiceEvent::QrCode { code }
    } else {
        ServiceEvent::Disconnected
    }
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
/// Starts the service for an account, replacing any running one.
async fn start_service(app: &AppHandle, state: &AppState, account: &str) -> Result<(), String> {
    // Stop whatever is running first, so the old account disconnects.
    if let Some(existing) = state.service.lock().unwrap().take() {
        existing.shutdown();
    }

    let settings = state.settings.lock().unwrap().clone();
    let config = config_for(app, &settings, account);

    let (service, mut events) = Service::start(config)
        .await
        .map_err(|e| format!("failed to start service: {e}"))?;
    let service = Arc::new(service);

    // `events` was registered before the connection attempt, so the pairing code
    // cannot slip through the gap between starting and subscribing.
    let emitter = app.clone();
    let service_for_events = service.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let _ = emitter.emit(SERVICE_EVENT, &event);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                    // A slow consumer missed some events, and that can include
                    // `Connected`: an offline-sync burst is larger than any
                    // buffer. Re-announce the state so the UI catches up. The
                    // messages themselves are in the store to be refetched.
                    eprintln!("[hermodr] dropped {dropped} service event(s)");
                    let _ = emitter.emit(SERVICE_EVENT, &resync_event(&service_for_events));
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

/// Connects the active account, pairing by QR the first time.
#[tauri::command]
async fn connect(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if state.service.lock().unwrap().is_some() {
        return Ok(());
    }
    let account = match active_account(&state) {
        Some(account) => account,
        // First run: create the default account.
        None => {
            {
                let mut file = state.accounts.lock().unwrap();
                file.accounts.push(Account {
                    id: "default".into(),
                    label: "WhatsApp".into(),
                });
                file.active = Some("default".into());
            }
            save_accounts(&app, &state.accounts.lock().unwrap());
            "default".to_string()
        }
    };
    start_service(&app, &state, &account).await
}

/// The accounts and which one is active.
#[tauri::command]
fn accounts(state: State<'_, AppState>) -> AccountsView {
    let file = state.accounts.lock().unwrap();
    AccountsView {
        accounts: file.accounts.clone(),
        active: file.active.clone(),
    }
}

/// Adds an account and switches to it, which starts pairing.
#[tauri::command]
async fn add_account(
    app: AppHandle,
    state: State<'_, AppState>,
    label: Option<String>,
) -> Result<(), String> {
    let id = format!("acct-{}", now_millis());
    {
        let mut file = state.accounts.lock().unwrap();
        file.accounts.push(Account {
            id: id.clone(),
            label: label.unwrap_or_else(|| "WhatsApp".into()),
        });
        file.active = Some(id.clone());
    }
    save_accounts(&app, &state.accounts.lock().unwrap());
    start_service(&app, &state, &id).await
}

/// Switches to another account, disconnecting the current one.
#[tauri::command]
async fn switch_account(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    {
        let mut file = state.accounts.lock().unwrap();
        if !file.accounts.iter().any(|a| a.id == id) {
            return Err("unknown account".into());
        }
        file.active = Some(id.clone());
    }
    save_accounts(&app, &state.accounts.lock().unwrap());
    start_service(&app, &state, &id).await
}

/// Renames an account.
#[tauri::command]
fn rename_account(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    label: String,
) -> Result<(), String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Ok(());
    }
    {
        let mut file = state.accounts.lock().unwrap();
        match file.accounts.iter_mut().find(|a| a.id == id) {
            Some(account) => account.label = label,
            None => return Err("unknown account".into()),
        }
    }
    save_accounts(&app, &state.accounts.lock().unwrap());
    Ok(())
}

/// Removes an account and its data, switching to another if it was active.
#[tauri::command]
async fn remove_account(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    {
        let mut file = state.accounts.lock().unwrap();
        file.accounts.retain(|a| a.id != id);
        if file.active.as_deref() == Some(id.as_str()) {
            file.active = file.accounts.first().map(|a| a.id.clone());
        }
    }
    save_accounts(&app, &state.accounts.lock().unwrap());
    if let Some(existing) = state.service.lock().unwrap().take() {
        existing.shutdown();
    }
    if id != "default" {
        let _ = std::fs::remove_dir_all(account_base(&app, &id));
    }
    if let Some(next) = active_account(&state) {
        start_service(&app, &state, &next).await?;
    }
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
    mentions: Option<Vec<String>>,
) -> Result<(), String> {
    let service = state.service()?;
    service
        .send_reply(
            &chat,
            text,
            &reply_to_id,
            &reply_to_sender,
            &reply_to_text,
            mentions.unwrap_or_default(),
        )
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

/// Opens a downloaded media file with the desktop's default application.
///
/// The path is restricted to the configured media folder. The webview is the
/// least trusted part of the app, and it must not be able to ask the shell to
/// open arbitrary files.
#[tauri::command]
fn open_path(app: AppHandle, state: State<'_, AppState>, path: String) -> Result<(), String> {
    let configured = state
        .service
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|service| service.media_dir())
        .or_else(|| {
            let account = active_account(&state)?;
            let settings = state.settings.lock().unwrap().clone();
            config_for(&app, &settings, &account).media_dir
        });

    let dir = configured
        .and_then(|dir| std::fs::canonicalize(dir).ok())
        .ok_or("no media folder is configured")?;
    let target = std::fs::canonicalize(&path).map_err(|e| e.to_string())?;
    if !target.starts_with(&dir) {
        return Err("refusing to open a file outside the media folder".into());
    }

    #[cfg(target_os = "linux")]
    let spawned = std::process::Command::new("xdg-open").arg(&target).spawn();
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(&target).spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(&target)
        .spawn();

    spawned.map_err(|e| e.to_string())?;
    Ok(())
}

/// Media extensions the renderer may read.
const READABLE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "mp4", "mov", "m4v", "webm", "mkv", "ogg", "opus", "mp3",
    "m4a", "aac", "wav",
];

/// Reads a media file and returns it base64-encoded.
///
/// WebKitGTK's media pipeline cannot load the custom asset scheme, so audio and
/// video have to arrive as bytes and be turned into a blob URL by the page. The
/// extension allowlist keeps this from becoming a general file-read primitive,
/// which matters because a pasted file can live anywhere on disk.
#[tauri::command]
fn read_file(path: String) -> Result<String, String> {
    let extension = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !READABLE_EXTENSIONS.contains(&extension.as_str()) {
        return Err("unsupported file type".into());
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(BASE64.encode(bytes))
}

/// Sends a text message to a chat.
#[tauri::command]
async fn send_text(
    state: State<'_, AppState>,
    chat: String,
    text: String,
    mentions: Option<Vec<String>>,
) -> Result<(), String> {
    let service = state.service()?;
    service
        .send_text(&chat, text, mentions.unwrap_or_default())
        .await
        .map_err(|e| e.to_string())
}

/// Group members for mention autocomplete.
#[tauri::command]
async fn participants(
    state: State<'_, AppState>,
    chat: String,
) -> Result<Vec<hermodr_core::Participant>, String> {
    state
        .service()?
        .participants(&chat)
        .await
        .map_err(|e| e.to_string())
}

/// Group subject, description and members, for the info sidebar.
#[tauri::command]
async fn group_info(
    state: State<'_, AppState>,
    chat: String,
) -> Result<hermodr_core::GroupInfo, String> {
    state
        .service()?
        .group_info(&chat)
        .await
        .map_err(|e| e.to_string())
}

/// Opens an http(s) URL in the desktop's default browser.
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http(s) links are opened".into());
    }
    #[cfg(target_os = "linux")]
    let spawned = std::process::Command::new("xdg-open").arg(&url).spawn();
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(&url)
        .spawn();
    spawned.map_err(|e| e.to_string())?;
    Ok(())
}

/// Chats, contacts and groups matching a query.
#[tauri::command]
async fn search(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<hermodr_core::SearchResult>, String> {
    state.service()?.search(&query).await.map_err(|e| e.to_string())
}

/// Pins or unpins a chat, mirroring it to the account.
#[tauri::command]
async fn set_pinned(state: State<'_, AppState>, chat: String, pinned: bool) -> Result<(), String> {
    state
        .service()?
        .set_pinned(&chat, pinned)
        .await
        .map_err(|e| e.to_string())
}

/// Asks the phone for older messages in a chat.
#[tauri::command]
async fn load_older(
    state: State<'_, AppState>,
    chat: String,
    count: Option<i32>,
) -> Result<(), String> {
    state
        .service()?
        .load_older(&chat, count.unwrap_or(50))
        .await
        .map_err(|e| e.to_string())
}

/// Unread messages that mention us, oldest first.
#[tauri::command]
fn unread_mentions(state: State<'_, AppState>, chat: String) -> Result<Vec<String>, String> {
    state
        .service()?
        .unread_mentions(&chat)
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
                accounts: Mutex::new(load_accounts(app.handle())),
            });

            // Built here rather than from the config so clipboard access can be
            // turned on. WebKitGTK only hands pasted images to the page when
            // `javascript_can_access_clipboard` is set, and it does not deliver
            // them through the paste event's clipboardData.
            WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title("Hermóðr")
                .inner_size(1000.0, 720.0)
                .min_inner_size(480.0, 360.0)
                .enable_clipboard_access()
                .build()?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            connection_state,
            connect,
            accounts,
            add_account,
            switch_account,
            remove_account,
            rename_account,
            messages,
            chats,
            resolve_names,
            mark_read,
            send_reply,
            send_media,
            send_text,
            open_path,
            read_file,
            participants,
            group_info,
            set_pinned,
            unread_mentions,
            load_older,
            search,
            open_url,
            qr_svg,
            get_settings,
            set_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
