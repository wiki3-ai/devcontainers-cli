//! Devcontainers.app — Tauri 2 desktop app library.
//!
//! Layering (mirrors `wiki3-ai/wiki3-app`):
//!   1. App shell        — [`run`] sets up windows and the Tauri builder.
//!   2. Host layer       — [`host`] (config, permissions, persistent state, menu).
//!   3. Container engine — the reusable [`devcontainer_core`] crate
//!      (runtime trait + Apple Containers / Podman / Docker impls,
//!      `devcontainer.json` translation, lifecycle orchestration).
//!   4. PTY              — [`pty`] (terminal hookup for the WebView).
//!   5. Commands         — [`commands`] (Tauri command surface).
//!   6. [`tauri_sink`]   — `EventSink` impl bridging the orchestrator to
//!      the Tauri event bus.

pub mod commands;
pub mod host;
pub mod pty;
pub mod tauri_sink;

use tracing_subscriber::EnvFilter;

/// Build and run the Tauri app. Called from `main.rs`.
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let host_state = host::HostState::load().unwrap_or_default();
    let runtime_registry = devcontainer_core::RuntimeRegistry::with_default_backends();
    let orchestrator = devcontainer_core::LifecycleOrchestrator::new();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(host_state)
        .manage(runtime_registry)
        .manage(orchestrator)
        .menu(host::menu::build_menu)
        .on_menu_event(host::menu::handle_menu_event)
        .invoke_handler(tauri::generate_handler![
            commands::workspace::list_workspaces,
            commands::workspace::add_workspace,
            commands::workspace::remove_workspace,
            commands::runtime::list_runtimes,
            commands::runtime::select_runtime,
            commands::runtime::list_containers,
            commands::runtime::container_start_by_id,
            commands::runtime::container_stop_by_id,
            commands::runtime::container_remove_by_id,
            commands::lifecycle::submit_parsed_devcontainer,
            commands::lifecycle::container_status,
            commands::lifecycle::container_up,
            commands::lifecycle::container_stop,
            commands::lifecycle::container_rebuild,
            commands::lifecycle::container_remove,
            commands::lifecycle::container_cancel,
            commands::fs::fs_is_file,
            commands::fs::fs_read_file,
            commands::fs::fs_write_file,
            commands::fs::fs_read_dir,
            commands::fs::fs_mkdirp,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Devcontainers.app");
}

// Plugin set is intentionally empty in this scaffold. Real plugins
// (e.g. `tauri-plugin-shell`, `tauri-plugin-fs`, `tauri-plugin-dialog`) are
// added in the PRs that wire the corresponding command surfaces.
