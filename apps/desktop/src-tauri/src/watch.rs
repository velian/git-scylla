//! Starting the filesystem watcher, and keeping its index current.
//!
//! **No rules here.** Which repository a path belongs to, which paths matter
//! and what a debounce window collapses to are all `crates/watch`'s; whether a
//! reported repository may be re-probed is the engine's. What is left for the
//! shell is the two joins neither of them can make: the watcher has to be told
//! the roots, and it has to be told the working set once a scan settles.

use crate::state::{App, WatcherSlot};
use git_scylla_engine::{EngineHandle, Event};
use git_scylla_watch::{Invalidation, Watcher, DEBOUNCE};
use std::path::PathBuf;
use tauri::State;

/// Watch these roots, replacing whatever was being watched.
///
/// A failure is logged and not surfaced: the application is entirely usable
/// without a watcher, and an error banner on every launch for a degraded
/// refresh would cost more than it explains.
pub fn restart(app: &State<'_, App>, roots: &[PathBuf]) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Invalidation>(256);
    let watcher = match Watcher::start(roots, tx, DEBOUNCE) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(%e, "could not watch the roots; the grid will need Refresh");
            *app.watcher.lock().expect("watcher poisoned") = None;
            // Say so. Without coverage an old snapshot is one nobody has
            // checked, and the engine falls back to judging rows by age, which
            // is correct when there is no watcher.
            tell_engine_coverage(&app.engine, false);
            return;
        }
    };

    // Seed the index from what the engine already holds, so a rescan of roots
    // that were already scanned does not go blind until it settles.
    let engine = app.engine.clone();
    reindex_later(engine.clone(), &watcher);

    let forwarding = engine.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(what) = rx.recv().await {
            if forwarding.invalidate(what).await.is_err() {
                return; // the engine has stopped
            }
        }
    });

    // Dropping the previous watcher stops its subscription and its debounce
    // task; it happens here, at the assignment, and not before — so there is no
    // window in which nothing is watched.
    *app.watcher.lock().expect("watcher poisoned") = Some(watcher);
    tell_engine_coverage(&app.engine, true);
}

/// Tell the engine whether a watcher is covering the roots.
///
/// It changes what a snapshot's *age* means: with coverage, an old snapshot is
/// one nothing has changed; without it, one nobody has checked. The engine
/// cannot work this out for itself, because the watcher is the shell's.
fn tell_engine_coverage(engine: &EngineHandle, covered: bool) {
    let engine = engine.clone();
    tauri::async_runtime::spawn(async move {
        let _ = engine.set_watched(covered).await;
    });
}

/// Rebuild the watcher's index whenever a scan settles.
///
/// Subscribed once, for the life of the application, because a watcher can be
/// replaced under it and the subscription is about the engine rather than about
/// any particular watcher.
pub fn follow_scans(engine: EngineHandle, watcher: WatcherSlot) {
    tauri::async_runtime::spawn(async move {
        let mut events = engine.subscribe();
        loop {
            match events.recv().await {
                // A settled scan is the moment the working set is known. A
                // removal changes it too, and cheaply enough to just rebuild.
                Ok(Event::ScanDone { .. }) | Ok(Event::ReposRemoved(_)) => {
                    // Asked for *before* the lock is taken: `watcher` is behind
                    // a `std::sync::Mutex` and this is an await.
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
    // The watcher is about to be stored, so this cannot borrow it across the
    // await; the index is behind its own lock and is cheap to hand over.
    let index = watcher.index_handle();
    tauri::async_runtime::spawn(async move {
        if let Ok(watched) = engine.watched().await {
            index.replace(watched);
        }
    });
}
