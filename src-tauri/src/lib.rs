use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    webview::{PermissionKind, PermissionResponse},
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, State, Webview, WebviewUrl,
    WindowEvent,
};

const CHROME_HEIGHT: f64 = 40.0;
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(90);
const SETTINGS_WIDTH: f64 = 420.0;
const SETTINGS_HEIGHT: f64 = 380.0;

#[derive(Clone, Serialize, Deserialize)]
struct Tab {
    id: String,
    name: String,
    #[serde(default = "default_zoom")]
    zoom: f64,
}

fn default_zoom() -> f64 {
    1.0
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct Settings {
    /// Percentage of available memory at which WebKit starts releasing caches.
    conservative_threshold: f64,
    /// Percentage at which WebKit aggressively reclaims memory.
    strict_threshold: f64,
    /// Percentage above which WebKit may kill the process.
    kill_threshold: f64,
    /// Reclaim memory from tabs that are not the active one.
    unload_inactive_tabs: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            conservative_threshold: 0.5,
            strict_threshold: 0.75,
            kill_threshold: 0.9,
            unload_inactive_tabs: true,
        }
    }
}

impl Settings {
    /// Clamps values into WebKit's accepted range.
    ///
    /// Thresholds are fractions between 0 and 1. Older builds stored percentages,
    /// and WebKit asserts (then ignores the whole setting) if given anything else,
    /// so anything out of range is reset to the default.
    fn validated(mut self) -> Self {
        let default = Settings::default();
        self.conservative_threshold = clamp_fraction(self.conservative_threshold, default.conservative_threshold);
        self.strict_threshold = clamp_fraction(self.strict_threshold, default.strict_threshold);
        self.kill_threshold = clamp_fraction(self.kill_threshold, default.kill_threshold);

        // WebKit requires conservative < strict < kill; it asserts (and drops the
        // whole configuration) otherwise.
        if self.conservative_threshold >= self.strict_threshold {
            self.conservative_threshold = default.conservative_threshold;
            self.strict_threshold = default.strict_threshold;
        }
        if self.strict_threshold >= self.kill_threshold {
            self.kill_threshold = default.kill_threshold;
        }
        self
    }
}

fn clamp_fraction(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value > 0.0 && value < 1.0 {
        value
    } else {
        fallback
    }
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    tabs: Vec<Tab>,
    selected: String,
    #[serde(default)]
    settings: Settings,
}

#[derive(Clone, Serialize)]
struct FrontendTab {
    id: String,
    name: String,
    zoom: f64,
}

#[derive(Clone, Serialize)]
struct FrontendState {
    tabs: Vec<FrontendTab>,
    selected: String,
    settings: Settings,
}

#[derive(Clone)]
struct Layout {
    views: Arc<Mutex<HashMap<String, Webview>>>,
    ui: Arc<Mutex<Option<Webview>>>,
    timer_scheduled: Arc<AtomicBool>,
}

impl Layout {
    fn relayout(&self, width: f64, height: f64) {
        if !(width.is_finite() && height.is_finite()) || width < 1.0 || height < 1.0 {
            return;
        }
        let content_height = (height - CHROME_HEIGHT).max(1.0);
        for webview in self.views.lock().unwrap().values() {
            let _ = webview.set_bounds(tauri::Rect {
                position: LogicalPosition::new(0.0, CHROME_HEIGHT).into(),
                size: LogicalSize::new(width, content_height).into(),
            });
        }
        if let Some(ui) = self.ui.lock().unwrap().as_ref() {
            let _ = ui.set_bounds(tauri::Rect {
                position: LogicalPosition::new(0.0, 0.0).into(),
                size: LogicalSize::new(width, CHROME_HEIGHT).into(),
            });
        }
    }

    /// Records the size to apply once resizing settles.
    ///
    /// The window handle is passed in so the final size is read from the window
    /// at apply time: during a tiled resize the event payload can lag behind the
    /// real size, and applying that stale value leaves the webviews out of sync
    /// with the window.
    fn request(&self, window: &tauri::Window) {
        if self.timer_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let layout = self.clone();
        let window = window.clone();
        std::thread::spawn(move || {
            std::thread::sleep(RESIZE_DEBOUNCE);
            layout.timer_scheduled.store(false, Ordering::SeqCst);
            let Ok(size) = window.inner_size() else { return };
            let scale = window.scale_factor().unwrap_or(1.0);
            let logical = size.to_logical::<f64>(scale);
            layout.relayout(logical.width, logical.height);
        });
    }
}

struct AppState {
    tabs: Arc<Mutex<Vec<Tab>>>,
    selected: Arc<Mutex<String>>,
    settings: Arc<Mutex<Settings>>,
    layout: Layout,
    store_path: PathBuf,
    seq: AtomicU64,
}

impl AppState {
    fn frontend(&self) -> FrontendState {
        FrontendState {
            tabs: self
                .tabs
                .lock()
                .unwrap()
                .iter()
                .map(|t| FrontendTab {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    zoom: t.zoom,
                })
                .collect(),
            selected: self.selected.lock().unwrap().clone(),
            settings: self.settings.lock().unwrap().clone(),
        }
    }

    fn show_only(&self, selected: &str) {
        let views = self.layout.views.lock().unwrap();
        for (id, webview) in views.iter() {
            let _ = if id == selected {
                webview.show()
            } else {
                webview.hide()
            };
        }
        // The chrome strip is not a tab, so the loop above would hide it.
        if let Some(ui) = self.layout.ui.lock().unwrap().as_ref() {
            let _ = ui.show();
        }
    }

    fn apply_zoom(&self, id: &str, zoom: f64) {
        if let Some(webview) = self.layout.views.lock().unwrap().get(id) {
            let _ = webview.set_zoom(zoom);
        }
    }

    fn save(&self) {
        let persisted = Persisted {
            tabs: self.tabs.lock().unwrap().clone(),
            selected: self.selected.lock().unwrap().clone(),
            settings: self.settings.lock().unwrap().clone(),
        };
        if let Some(parent) = self.store_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&persisted) {
            let _ = fs::write(&self.store_path, json);
        }
    }
}

fn next_id(seq: &AtomicU64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("t{:x}{:x}", nanos, seq.fetch_add(1, Ordering::Relaxed))
}

fn tabs_root(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("tabs")
}

fn tab_dir(app: &AppHandle, id: &str) -> PathBuf {
    tabs_root(app).join(id)
}

/// Removes data directories that no longer belong to a known tab.
///
/// Mirrors Altus' partition pruning: old runs leave full WhatsApp profiles
/// (hundreds of MB each) that would otherwise never be reclaimed.
fn prune_orphan_tabs(app: &AppHandle, live_ids: &[String]) -> usize {
    let root = tabs_root(app);
    let Ok(entries) = fs::read_dir(&root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if live_ids.iter().any(|id| id == &name) {
            continue;
        }
        if fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Applies WebKit's memory pressure thresholds so idle tabs release caches.
///
/// WebKit keeps decoded message data resident otherwise, which on a large
/// WhatsApp history means gigabytes that are never handed back. Must run before
/// any webview is created to take effect.
fn apply_memory_pressure(settings: &Settings) -> bool {
    use webkit2gtk::{MemoryPressureSettings, WebsiteDataManager};

    let mut pressure = MemoryPressureSettings::new();
    // Order matters: each setter is validated against the *other* thresholds
    // still holding their defaults, so a low value applied first trips the
    // assertion. Setting the highest threshold first always satisfies it.
    pressure.set_kill_threshold(settings.kill_threshold);
    pressure.set_strict_threshold(settings.strict_threshold);
    pressure.set_conservative_threshold(settings.conservative_threshold);
    WebsiteDataManager::set_memory_pressure_settings(&mut pressure);
    true
}

/// Opens a URL in the user's default browser.
///
/// Deliberately avoids a plugin dependency: `xdg-open` is the standard Linux
/// entry point and is what the desktop session already uses.
fn open_external(url: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
}

fn create_webview(app: &AppHandle, window: &tauri::Window, tab: &Tab) -> tauri::Result<Webview> {
    let dir = tab_dir(app, &tab.id);
    let _ = fs::create_dir_all(&dir);

    let size = window.inner_size()?;
    let logical = size.to_logical::<f64>(window.scale_factor()?);
    let content_height = (logical.height - CHROME_HEIGHT).max(1.0);

    let label = tab.id.clone();

    window.add_child(
        tauri::webview::WebviewBuilder::new(
            &tab.id,
            WebviewUrl::External("https://web.whatsapp.com".parse().unwrap()),
        )
        .data_directory(dir)
        .enable_clipboard_access()
        // Without an explicit background the webview paints its default light
        // colour before WebKit composites, which shows as a flash on resize or
        // when the window regains focus.
        .background_color(tauri::utils::config::Color(24, 24, 27, 255))
        .on_permission_request(|_, kind| match kind {
            PermissionKind::Notifications | PermissionKind::Microphone | PermissionKind::Camera => {
                PermissionResponse::Allow
            }
            _ => PermissionResponse::Default,
        })
        // WhatsApp stays in the webview; every other link goes to the system
        // browser, matching desktop-client behaviour.
        .on_navigation(move |url| {
            let host = url.host_str().unwrap_or_default();
            let is_whatsapp = host == "web.whatsapp.com"
                || host.ends_with(".whatsapp.com")
                || host == "whatsapp.com"
                || host == "wa.me";
            if is_whatsapp {
                return true;
            }
            if url.scheme() == "http" || url.scheme() == "https" {
                open_external(url.as_str());
            }
            false
        })
        // Links with target="_blank" (and window.open) would otherwise be
        // silently dropped or spawn a bare webview window.
        .on_new_window(move |url, _features| {
            let host = url.host_str().unwrap_or_default();
            let is_whatsapp =
                host == "web.whatsapp.com" || host.ends_with(".whatsapp.com") || host == "wa.me";
            if !is_whatsapp && (url.scheme() == "http" || url.scheme() == "https") {
                open_external(url.as_str());
            }
            tauri::webview::NewWindowResponse::Deny
        })
        .initialization_script(WHATSAPP_INIT_SCRIPT)
        .initialization_script(&format!(
            "window.__HERMODR_TAB_ID__ = {};",
            serde_json::to_string(&label).unwrap()
        )),
        LogicalPosition::new(0.0, CHROME_HEIGHT),
        LogicalSize::new(logical.width, content_height),
    )
}

/// Runs in the WhatsApp page. Handles the things the web app does not do on a
/// desktop webview, most importantly pasting images from the system clipboard.
const WHATSAPP_INIT_SCRIPT: &str = r#"
(function () {
  if (window.__hermodrInit) return;
  window.__hermodrInit = true;

  // WhatsApp only exposes paste handling through its own paste event; WebKitGTK
  // does not deliver clipboard images to it, so we read the clipboard and
  // synthesise a paste on the composer ourselves.
  async function pasteImage() {
    if (!navigator.clipboard || !navigator.clipboard.read) return false;
    let items;
    try {
      items = await navigator.clipboard.read();
    } catch (e) {
      return false;
    }
    for (const item of items) {
      const type = item.types.find((t) => t.startsWith('image/'));
      if (!type) continue;
      const blob = await item.getType(type);
      const file = new File([blob], 'pasted-image.png', { type });
      const data = new DataTransfer();
      data.items.add(file);

      const input = document.querySelector('#main footer div[contenteditable]');
      if (!input) return false;
      const event = new ClipboardEvent('paste', {
        bubbles: true,
        cancelable: true,
        clipboardData: data,
      });
      input.focus();
      input.dispatchEvent(event);
      return true;
    }
    return false;
  }

  document.addEventListener(
    'keydown',
    function (event) {
      if (!(event.ctrlKey || event.metaKey)) return;
      const key = event.key;

      if (key === 'v' && !event.shiftKey) {
        // Only intercept when there is no text on the clipboard worth pasting;
        // let the browser handle ordinary text paste itself.
        pasteImage();
        return;
      }

      // Shortcuts cannot be seen by the chrome webview while WhatsApp has
      // focus, so forward them out. Only a whitelist crosses the boundary.
      if (key === '=' || key === '+' || key === '-' || key === '0' ||
          key === 't' || key === 'w' || key === ',') {
        event.preventDefault();
        event.stopPropagation();
        try {
          window.__TAURI_INTERNALS__.invoke('shortcut', { key: key });
        } catch (e) {}
      }
    },
    true
  );
})();
"#;

fn broadcast(app: &AppHandle, state: &AppState) {
    let _ = app.emit_to("ui", "tabs-changed", state.frontend());
}

#[tauri::command]
fn list_tabs(state: State<'_, AppState>) -> FrontendState {
    state.frontend()
}

#[tauri::command]
fn add_tab(app: AppHandle, window: tauri::Window, state: State<'_, AppState>) -> Result<(), String> {
    let id = next_id(&state.seq);
    let name = format!("Account {}", state.tabs.lock().unwrap().len() + 1);
    let tab = Tab {
        id: id.clone(),
        name,
        zoom: 1.0,
    };
    let webview = create_webview(&app, &window, &tab).map_err(|e| e.to_string())?;
    state.layout.views.lock().unwrap().insert(id.clone(), webview);
    state.tabs.lock().unwrap().push(tab);
    *state.selected.lock().unwrap() = id.clone();
    state.show_only(&id);

    let size = window.inner_size().map_err(|e| e.to_string())?;
    let logical = size.to_logical::<f64>(window.scale_factor().unwrap_or(1.0));
    state.layout.relayout(logical.width, logical.height);

    state.save();
    broadcast(&app, &state);
    Ok(())
}

#[tauri::command]
fn select_tab(app: AppHandle, state: State<'_, AppState>, id: String) {
    if !state.tabs.lock().unwrap().iter().any(|t| t.id == id) {
        return;
    }
    *state.selected.lock().unwrap() = id.clone();
    state.show_only(&id);
    let zoom = state
        .tabs
        .lock()
        .unwrap()
        .iter()
        .find(|t| t.id == id)
        .map(|t| t.zoom)
        .unwrap_or(1.0);
    state.apply_zoom(&id, zoom);
    state.save();
    broadcast(&app, &state);
}

#[tauri::command]
fn close_tab(app: AppHandle, window: tauri::Window, state: State<'_, AppState>, id: String) {
    if let Some(webview) = state.layout.views.lock().unwrap().remove(&id) {
        let _ = webview.close();
    }
    state.tabs.lock().unwrap().retain(|t| t.id != id);
    let _ = fs::remove_dir_all(tab_dir(&app, &id));

    if state.tabs.lock().unwrap().is_empty() {
        let _ = add_tab(app, window, state);
        return;
    }

    if *state.selected.lock().unwrap() == id {
        let next = state.tabs.lock().unwrap()[0].id.clone();
        *state.selected.lock().unwrap() = next.clone();
        state.show_only(&next);
    }
    state.save();
    broadcast(&app, &state);
}

#[tauri::command]
fn rename_tab(app: AppHandle, state: State<'_, AppState>, id: String, name: String) {
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }
    if let Some(tab) = state.tabs.lock().unwrap().iter_mut().find(|t| t.id == id) {
        tab.name = name;
    }
    state.save();
    broadcast(&app, &state);
}

#[tauri::command]
fn reorder_tabs(app: AppHandle, state: State<'_, AppState>, ids: Vec<String>) {
    let mut tabs = state.tabs.lock().unwrap();
    tabs.sort_by_key(|t| ids.iter().position(|id| id == &t.id).unwrap_or(usize::MAX));
    drop(tabs);
    state.save();
    broadcast(&app, &state);
}

#[tauri::command]
fn set_zoom(app: AppHandle, state: State<'_, AppState>, id: String, zoom: f64) {
    let zoom = zoom.clamp(0.5, 3.0);
    if let Some(tab) = state.tabs.lock().unwrap().iter_mut().find(|t| t.id == id) {
        tab.zoom = zoom;
    }
    if *state.selected.lock().unwrap() == id {
        state.apply_zoom(&id, zoom);
    }
    state.save();
    broadcast(&app, &state);
}

/// Adjusts the zoom of a specific tab and returns the new value.
#[tauri::command]
fn zoom_by(app: AppHandle, state: State<'_, AppState>, id: String, delta: f64) -> f64 {
    let current = state
        .tabs
        .lock()
        .unwrap()
        .iter()
        .find(|t| t.id == id)
        .map(|t| t.zoom)
        .unwrap_or(1.0);
    let next = (current + delta).clamp(0.5, 3.0);
    set_zoom(app, state, id, next);
    next
}

#[tauri::command]
fn reset_zoom(app: AppHandle, state: State<'_, AppState>, id: String) {
    set_zoom(app, state, id, 1.0);
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

/// Opens (or focuses) the settings window.
///
/// The dialog cannot live in the chrome strip: that webview is only 40px tall,
/// so anything it renders outside its bounds is clipped behind the WhatsApp
/// webview. It runs as a transient of the main window, which is what makes the
/// window manager keep it stacked above its parent rather than behind it.
#[tauri::command]
fn open_settings(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return Ok(());
    }

    let mut builder = tauri::webview::WebviewWindowBuilder::new(
        &app,
        "settings",
        // Route path rather than `settings.html`: the dev server only serves the
        // route, and the static build falls back to it as well.
        WebviewUrl::App("settings".into()),
    )
    .title("Hermóðr Settings")
    .inner_size(SETTINGS_WIDTH, SETTINGS_HEIGHT)
    .resizable(false)
    .always_on_top(true);

    if let Some(main) = app.get_webview_window("main") {
        builder = builder.transient_for(&main).map_err(|e| e.to_string())?;
        // Centre over the main window rather than the screen.
        if let (Ok(pos), Ok(size)) = (main.outer_position(), main.outer_size()) {
            let x = pos.x + (size.width as i32 - SETTINGS_WIDTH as i32) / 2;
            let y = pos.y + (size.height as i32 - SETTINGS_HEIGHT as i32) / 2;
            builder = builder.position(x as f64, y as f64);
        }
    } else {
        builder = builder.center();
    }

    builder.build().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_settings(app: AppHandle, state: State<'_, AppState>, settings: Settings) {
    let settings = settings.validated();
    *state.settings.lock().unwrap() = settings.clone();
    apply_memory_pressure(&settings);
    state.save();
    broadcast(&app, &state);
}

/// Handles a keyboard shortcut forwarded from a WhatsApp webview.
///
/// The remote page has no other IPC access; this command is the single bridge
/// and it only acts on a fixed set of keys.
#[tauri::command]
fn shortcut(app: AppHandle, state: State<'_, AppState>, key: String) -> Result<(), String> {
    let selected = state.selected.lock().unwrap().clone();
    match key.as_str() {
        "=" | "+" => {
            let next = zoom_delta(&state, &selected, 0.1);
            let _ = app.emit_to("ui", "zoom-changed", next);
        }
        "-" => {
            let next = zoom_delta(&state, &selected, -0.1);
            let _ = app.emit_to("ui", "zoom-changed", next);
        }
        "0" => {
            set_zoom_value(&state, &selected, 1.0);
            let _ = app.emit_to("ui", "zoom-changed", 1.0);
        }
        "t" => {
            if let Some(window) = app.get_window("main") {
                let _ = add_tab(app.clone(), window, state);
            }
        }
        "w" => {
            if let Some(window) = app.get_window("main") {
                close_tab(app.clone(), window, state, selected);
            }
        }
        "," => {
            let _ = open_settings(app.clone());
        }
        _ => {}
    }
    Ok(())
}

#[tauri::command]
fn zoom_delta(state: &AppState, id: &str, delta: f64) -> f64 {
    let current = state
        .tabs
        .lock()
        .unwrap()
        .iter()
        .find(|t| t.id == id)
        .map(|t| t.zoom)
        .unwrap_or(1.0);
    let next = (current + delta).clamp(0.5, 3.0);
    set_zoom_value(state, id, next);
    next
}

#[tauri::command]
fn set_zoom_value(state: &AppState, id: &str, zoom: f64) {
    let zoom = zoom.clamp(0.5, 3.0);
    if let Some(tab) = state.tabs.lock().unwrap().iter_mut().find(|t| t.id == id) {
        tab.zoom = zoom;
    }
    if *state.selected.lock().unwrap() == id {
        state.apply_zoom(id, zoom);
    }
    state.save();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // WebKitGTK's DMA-BUF renderer fails to create GBM buffers under Wayland
    // (Hyprland), aborting with "Gdk Error 71". Fall back to the compositing path.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    tauri::Builder::default()
        .setup(|app| {
            let window = tauri::window::WindowBuilder::new(app, "main")
                .title("Hermóðr")
                .inner_size(1100.0, 800.0)
                .build()?;

            let store_path = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("tabs.json");
            let persisted: Option<Persisted> = fs::read_to_string(&store_path)
                .ok()
                .and_then(|json| serde_json::from_str(&json).ok());

            let state = AppState {
                tabs: Arc::new(Mutex::new(
                    persisted.as_ref().map(|p| p.tabs.clone()).unwrap_or_default(),
                )),
                selected: Arc::new(Mutex::new(
                    persisted
                        .as_ref()
                        .map(|p| p.selected.clone())
                        .unwrap_or_default(),
                )),
                settings: Arc::new(Mutex::new(
                    persisted
                        .as_ref()
                        .map(|p| p.settings.clone())
                        .unwrap_or_default()
                        .validated(),
                )),
                layout: Layout {
                    views: Arc::new(Mutex::new(HashMap::new())),
                    ui: Arc::new(Mutex::new(None)),
                    timer_scheduled: Arc::new(AtomicBool::new(false)),
                },
                store_path,
                seq: AtomicU64::new(0),
            };

            let settings = state.settings.lock().unwrap().clone();
            apply_memory_pressure(&settings);

            let live_ids: Vec<String> = state.tabs.lock().unwrap().iter().map(|t| t.id.clone()).collect();
            let removed = prune_orphan_tabs(app.handle(), &live_ids);
            if removed > 0 {
                eprintln!("[hermodr] pruned {removed} orphaned tab profile(s)");
            }

            let window_size = window.inner_size()?;
            let window_logical = window_size.to_logical::<f64>(window.scale_factor()?);

            // Content webviews are created before the chrome strip: GtkFixed draws
            // children in insertion order, so the strip ends up on top.
            if state.tabs.lock().unwrap().is_empty() {
                let id = next_id(&state.seq);
                let tab = Tab {
                    id: id.clone(),
                    name: "Account 1".into(),
                    zoom: 1.0,
                };
                let webview = create_webview(app.handle(), &window, &tab)?;
                state.layout.views.lock().unwrap().insert(id.clone(), webview);
                state.tabs.lock().unwrap().push(tab);
                *state.selected.lock().unwrap() = id;
            } else {
                let tabs = state.tabs.lock().unwrap().clone();
                for tab in &tabs {
                    let webview = create_webview(app.handle(), &window, tab)?;
                    state.layout.views.lock().unwrap().insert(tab.id.clone(), webview);
                }
                let selected = state.selected.lock().unwrap().clone();
                let selected = if tabs.iter().any(|t| t.id == selected) {
                    selected
                } else {
                    tabs[0].id.clone()
                };
                *state.selected.lock().unwrap() = selected;
            }

            let ui = window.add_child(
                tauri::webview::WebviewBuilder::new("ui", WebviewUrl::App("index.html".into()))
                    .background_color(tauri::utils::config::Color(24, 24, 27, 255))
                    // The strip is local-only and never navigates; giving it its own
                    // data directory would spawn an extra WebKit network process for
                    // no benefit.
                    .initialization_script(
                        "window.addEventListener('DOMContentLoaded', function () {
                            document.documentElement.style.background = '#18181b';
                        });",
                    ),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(window_logical.width, CHROME_HEIGHT),
            )?;
            *state.layout.ui.lock().unwrap() = Some(ui);

            let selected = state.selected.lock().unwrap().clone();
            state.show_only(&selected);
            let zoom = state
                .tabs
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.id == selected)
                .map(|t| t.zoom)
                .unwrap_or(1.0);
            state.apply_zoom(&selected, zoom);
            state.save();

            let size = window.inner_size()?;
            let logical = size.to_logical::<f64>(window.scale_factor()?);
            state.layout.relayout(logical.width, logical.height);

            let layout = state.layout.clone();
            let win = window.clone();
            window.on_window_event(move |event| match event {
                WindowEvent::Resized(_) => {
                    layout.request(&win);
                }
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = win.hide();
                }
                _ => {}
            });

            let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            let mut tray = TrayIconBuilder::new().menu(&menu);
            if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon);
            }
            tray.on_menu_event(|app, event| match event.id.as_ref() {
                "show" => {
                    if let Some(window) = app.get_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
                "quit" => app.exit(0),
                _ => {}
            })
            .build(app)?;

            app.manage(state);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_tabs,
            add_tab,
            select_tab,
            close_tab,
            rename_tab,
            reorder_tabs,
            set_zoom,
            zoom_by,
            reset_zoom,
            get_settings,
            set_settings,
            open_settings,
            shortcut
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
