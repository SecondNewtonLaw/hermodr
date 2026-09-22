const COMMANDS: &[&str] = &[
    "list_tabs",
    "add_tab",
    "select_tab",
    "close_tab",
    "rename_tab",
    "reorder_tabs",
    "set_zoom",
    "zoom_by",
    "reset_zoom",
    "get_settings",
    "set_settings",
    "open_settings",
    "shortcut",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
