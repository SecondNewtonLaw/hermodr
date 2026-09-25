const COMMANDS: &[&str] = &[
    "connection_state",
    "connect",
    "accounts",
    "add_account",
    "switch_account",
    "remove_account",
    "rename_account",
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
    "set_pinned",
    "search",
    "open_url",
    "unread_mentions",
    "load_older",
    "flush_media",
    "download_media",
    "set_chat_auto_download",
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
