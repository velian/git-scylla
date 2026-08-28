//! The Tauri shell.
//!
//! **This crate adds no domain logic.** Every decision about what is eligible,
//! what runs, and in what order was made in `crates/engine`; the commands in
//! [`commands`] are thin wrappers that send a `Cmd` and await the reply. If a
//! rule needs writing here, it belongs in the engine.

pub mod commands;
pub mod config;
mod error;
pub mod events;
mod handoff;
pub mod menu;
pub mod row;
pub mod state;
pub mod watch;

pub use error::{BridgeError, ErrorKind};
pub use events::CHANNEL as EVENT_CHANNEL;
pub use menu::CHANNEL as MENU_CHANNEL;

/// The command surface, in one place.
///
/// A macro rather than a list written twice: `run` needs it and so does the
/// test that drives it, and two lists would let a command be shipped untested
/// or tested but unshipped.
#[macro_export]
macro_rules! command_handler {
    () => {
        tauri::generate_handler![
            $crate::commands::start_scan,
            $crate::commands::cancel_scan,
            $crate::commands::get_snapshot,
            $crate::commands::select_repos,
            $crate::commands::refresh_repo,
            $crate::commands::plan,
            $crate::commands::start_batch,
            $crate::commands::cancel_batch,
            $crate::commands::plan_undo,
            $crate::commands::start_undo,
            $crate::commands::job_log,
            $crate::commands::pick_root_dir,
            $crate::commands::get_config,
            $crate::commands::add_root,
            $crate::commands::remove_root,
            $crate::commands::open_full_disk_access_settings,
            $crate::commands::hand_off,
            $crate::commands::set_editor,
            $crate::commands::set_terminal,
            $crate::commands::resolved_terminal,
            $crate::commands::template_placeholders,
            $crate::commands::put_custom,
            $crate::commands::remove_custom,
            $crate::commands::acknowledge_custom,
            $crate::commands::fetch_now,
            $crate::commands::set_has_selection,
        ]
    };
}

// Aliased: there are two `Config` types in play — the engine's limits and
// policy, and the roots this application persists.
use git_scylla_engine::{Config as EngineConfig, Engine};
use state::App;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // The native folder picker for root management. No security-scoped
        // bookmarks: this is a non-sandboxed local build.
        .plugin(tauri_plugin_dialog::init())
        // Only to open the Full Disk Access pane of System Settings.
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            use tauri::Manager;
            // `Engine::start` spawns onto the ambient tokio runtime, so it has
            // to be constructed inside one. Tauri's async runtime is tokio;
            // `block_on` enters it.
            let engine = tauri::async_runtime::block_on(async {
                // The cache is the shell's: a warm launch shows rows before it
                // has a scan. The CLI leaves it off.
                Engine::start(EngineConfig {
                    cache: git_scylla_engine::CacheMode::ReadWrite,
                    ..EngineConfig::default()
                })
            });
            events::forward(app.handle().clone(), engine.handle());
            menu::install(app.handle())?;
            let state = App::new(engine, config::load());
            let (handle, watcher) = (state.engine.clone(), std::sync::Arc::clone(&state.watcher));
            app.manage(state);
            // The watcher itself starts with the first scan, which is where the
            // roots arrive; this only keeps its index current afterwards.
            watch::follow_scans(handle, watcher);
            Ok(())
        })
        .on_menu_event(menu::dispatch)
        .invoke_handler(command_handler!())
        .run(tauri::generate_context!())
        .expect("error while running git-scylla");
}
