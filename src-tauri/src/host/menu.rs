//! Native macOS menu — File / View / Window / Help.
//!
//! Mirrors the menu structure of `wiki3-ai/wiki3-app`'s `menu.rs`. The menu
//! is built when the Tauri app launches; events are routed back into the
//! frontend via the standard Tauri `MenuEvent` mechanism so the dashboard
//! can react.

use tauri::{
    menu::{Menu, MenuBuilder, MenuEvent, MenuItemBuilder, SubmenuBuilder},
    AppHandle, Emitter, Manager, Runtime,
};

/// IDs used for the items routed back to the frontend.
pub mod ids {
    pub const OPEN_FOLDER: &str = "open-folder";
    pub const NEW_WORKSPACE: &str = "new-workspace";
    pub const TOGGLE_DASHBOARD: &str = "toggle-dashboard";
    pub const SHOW_HELP: &str = "show-help";
}

pub fn build_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let file = SubmenuBuilder::new(app, "File")
        .item(
            &MenuItemBuilder::with_id(ids::OPEN_FOLDER, "Open Folder…")
                .accelerator("CmdOrCtrl+O")
                .build(app)?,
        )
        .item(
            &MenuItemBuilder::with_id(ids::NEW_WORKSPACE, "New Workspace…")
                .accelerator("CmdOrCtrl+N")
                .build(app)?,
        )
        .separator()
        .quit()
        .build()?;

    let view = SubmenuBuilder::new(app, "View")
        .item(
            &MenuItemBuilder::with_id(ids::TOGGLE_DASHBOARD, "Toggle Dashboard")
                .accelerator("CmdOrCtrl+0")
                .build(app)?,
        )
        .separator()
        .fullscreen()
        .build()?;

    let window = SubmenuBuilder::new(app, "Window")
        .minimize()
        .close_window()
        .build()?;

    let help = SubmenuBuilder::new(app, "Help")
        .item(&MenuItemBuilder::with_id(ids::SHOW_HELP, "Devcontainers Help").build(app)?)
        .build()?;

    MenuBuilder::new(app)
        .items(&[&file, &view, &window, &help])
        .build()
}

pub fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    let id = event.id().0.as_str();
    match id {
        ids::TOGGLE_DASHBOARD => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = if window.is_visible().unwrap_or(false) {
                    window.hide()
                } else {
                    window.show().and_then(|_| window.set_focus())
                };
            }
        }
        _ => {
            // Forward to the frontend; the dashboard listens for `menu://<id>`.
            let _ = app.emit(&format!("menu://{id}"), ());
        }
    }
}
