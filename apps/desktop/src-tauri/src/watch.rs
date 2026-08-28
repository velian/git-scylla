//! Starting the filesystem watcher, and keeping its index current. See
//! `docs/README.md`.

use crate::state::{App, WatcherSlot};
use git_scylla_engine::{EngineHandle, Event};
use git_scylla_watch::{Invalidation, Watcher, DEBOUNCE};
use std::path::PathBuf;
use tauri::State;

/// Watch these roots, replacing whatever was being watched. A failure is
/// logged, not surfaced: the grid still works, driven by Refresh.
pub fn restart(app: &State<'_, App>, roots: &[PathBuf]) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Invalidation>(256);
    let watcher = match Watcher::start(roots, tx, DEBOUNCE) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(%e, "could not watch the roots; the grid will need Refresh");
            *app.watcher.lock().expect("watcher poisoned") = None;
            tell_engine_coverage(&app.engine, false);
            return;
        }
    };

    let engine = app.engine.clone();
    reindex_later(engine.clone(), &watcher);

    let forwarding = engine.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(what) = rx.recv().await {
            if forwarding.invalidate(what).await.is_err() {
                return;
            }
        }
    });

    // Dropping the previous watcher happens here, at the assignment, so there
    // is no window in which nothing is watched.
    *app.watcher.lock().expect("watcher poisoned") = Some(watcher);
    tell_engine_coverage(&app.engine, true);
}

/// Tell the engine whether a watcher is covering the roots. This decides
/// whether an old snapshot means "nothing changed" or "nobody has checked".
fn tell_engine_coverage(engine: &EngineHandle, covered: bool) {
    let engine = engine.clone();
    tauri::async_runtime::spawn(async move {
        let _ = engine.set_watched(covered).await;
    });
}

/// Rebuild the watcher's index whenever a scan settles. Subscribed once, for
/// the life of the application, since the watcher itself may be replaced.
pub fn follow_scans(engine: EngineHandle, watcher: WatcherSlot) {
    tauri::async_runtime::spawn(async move {
        let mut events = engine.subscribe();
        loop {
            match events.recv().await {
                Ok(Event::ScanDone { .. }) | Ok(Event::ReposRemoved(_)) => {
                    // Awaited before the `std::sync::Mutex` lock is taken.
                    let Ok(watched) = engine.watched().await else { return };
                    let held = watcher.lock().expect("watcher poisoned");
                    if let Some(w) = held.as_ref() {
                        w.reindex(watched);
                    }
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// Fill the index from what the engine holds right now.
fn reindex_later(engine: EngineHandle, watcher: &Watcher) {
    let index = watcher.index_handle();
    tauri::async_runtime::spawn(async move {
        if let Ok(watched) = engine.watched().await {
            index.replace(watched);
        }
    });
}
