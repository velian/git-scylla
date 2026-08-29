//! The Tauri shell.

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
            $crate::commands::set_fetch_interval,
            $crate::commands::fetch_now,
            $crate::commands::set_has_selection,
        ]
    };
}

use git_scylla_engine::{Config as EngineConfig, Engine};
use state::App;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            use tauri::Manager;
            let config = config::load();
            let fetch = git_scylla_engine::FetchPolicy {
                interval: config
                    .fetch_interval_secs
                    .map(std::time::Duration::from_secs)
                    .unwrap_or(git_scylla_engine::FetchPolicy::default().interval),
                ..git_scylla_engine::FetchPolicy::default()
            };
            let engine = tauri::async_runtime::block_on(async {
                Engine::start(EngineConfig {
                    cache: git_scylla_engine::CacheMode::ReadWrite,
                    fetch,
                    ..EngineConfig::default()
                })
            });
            events::forward(app.handle().clone(), engine.handle());
            menu::install(app.handle())?;
            let state = App::new(engine, config);
            let (handle, watcher) = (state.engine.clone(), std::sync::Arc::clone(&state.watcher));
            app.manage(state);
            watch::follow_scans(handle, watcher);
            Ok(())
        })
        .on_menu_event(menu::dispatch)
        .invoke_handler(command_handler!())
        .run(tauri::generate_context!())
        .expect("error while running git-scylla");
}
