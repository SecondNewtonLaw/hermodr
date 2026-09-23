const COMMANDS: &[&str] = &[
    "connection_state",
    "connect",
    "messages",
    "chats",
    "resolve_names",
    "mark_read",
    "send_reply",
    "send_media",
    "send_text",
    "open_path",
    "read_file",
    "participants",
    "group_info",
    "qr_svg",
    "get_settings",
    "set_settings",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
