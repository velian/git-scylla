//! Bridging the engine's broadcast channel to the webview.
//!
//! Batched on a 50 ms tick. One `app.emit` per repository probed would be
//! wasteful even at this scale, and — more to the point — React would re-render
//! per event rather than per frame.

use crate::row::RepoRow;
use git_scylla_core::{BatchId, BatchSummary, RepoId, RepoSnapshot};
use git_scylla_engine::{EngineHandle, Event};
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// The channel the frontend listens on.
pub const CHANNEL: &str = "engine://events";

/// What the webview receives.
///
/// Everything the engine publishes, except that snapshots arrive as **rows**:
/// the grid needs the derived `Badge` for display and for its default sort, and
/// deriving it in TypeScript would put a piece of the domain in the frontend.
///
/// A wrapper rather than a parallel copy of `Event`, so the enum's shape is
/// written down once and a passthrough arm cannot fall behind it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "type", content = "value")]
pub enum UiEvent {
    /// Projected from `Event::ReposUpserted`.
    Rows(Vec<RepoRow>),
    /// Projected from `Event::BatchDone`, for the same reason `Rows` is: the
    /// drawer's banner has to be the sentence the CLI prints, and re-assembling
    /// it from the counts on the other side would be a second shape free to
    /// drift from the first.
    BatchDone {
        id: BatchId,
        summary: BatchSummary,
        /// `31 ok, 3 failed, 13 skipped in 4.2s`, from
        /// [`BatchSummary::render`].
        line: String,
    },
    /// Everything else, verbatim.
    Engine(Event),
}

impl From<Event> for UiEvent {
    fn from(event: Event) -> Self {
        match event {
            Event::ReposUpserted(snaps) => {
                UiEvent::Rows(snaps.into_iter().map(RepoRow::from).collect())
            }
            Event::BatchDone { id, summary } => {
                UiEvent::BatchDone { id, summary, line: summary.render() }
            }
            other => UiEvent::Engine(other),
        }
    }
}

const TICK: Duration = Duration::from_millis(50);

/// Forward engine events to the webview until the engine stops.
pub fn forward(app: AppHandle, engine: EngineHandle) {
    tauri::async_runtime::spawn(async move {
        let mut events = engine.subscribe();
        let mut batch: Vec<Event> = Vec::new();
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                received = events.recv() => match received {
                    Ok(event) => batch.push(event),
                    // The receiver fell behind. Dropping events silently would
                    // leave the grid showing stale rows forever, so say so and
                    // let the frontend re-read the snapshot.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "event receiver lagged");
                        batch.push(Event::Lagged);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = ticker.tick() => {
                    if batch.is_empty() {
                        continue;
                    }
                    let payload: Vec<UiEvent> = coalesce(std::mem::take(&mut batch))
                        .into_iter()
                        .map(UiEvent::from)
                        .collect();
                    if app.emit(CHANNEL, &payload).is_err() {
                        // The window is gone.
                        break;
                    }
                }
            }
        }
    });
}

/// Collapse a tick's worth of events.
///
/// Only the two high-frequency kinds are merged, and both merge losslessly for
/// a consumer that just wants current state: a scan of a hundred repositories
/// produces a hundred `ReposUpserted` and a hundred `ScanProgress` in a couple
/// of ticks, and the frontend needs the last of each, not all of them.
/// Everything else is a discrete thing that happened and is passed through in
/// order.
fn coalesce(events: Vec<Event>) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::with_capacity(events.len());
    // Keyed by repository so the newest snapshot for each wins, and inserted at
    // the position of the first upsert so ordering against other events holds.
    let mut upserts: HashMap<RepoId, RepoSnapshot> = HashMap::new();
    let mut upsert_slot: Option<usize> = None;
    let mut progress_slot: HashMap<_, usize> = HashMap::new();

    for event in events {
        match event {
            Event::ReposUpserted(snaps) => {
                for snap in snaps {
                    upserts.insert(snap.id.clone(), snap);
                }
                if upsert_slot.is_none() {
                    upsert_slot = Some(out.len());
                    out.push(Event::ReposUpserted(Vec::new()));
                }
            }
            // Merging upserts into one slot works only while nothing between
            // them cares about the order. A removal does: a repository re-read,
            // removed, and re-read again would otherwise have its removal
            // ordered *after* both upserts and be dropped on the strength of
            // the middle fact. So the accumulated upserts are flushed ahead of
            // it and a fresh slot opens behind it.
            Event::ReposRemoved(ids) => {
                for id in &ids {
                    upserts.remove(id);
                }
                flush(&mut out, &mut upserts, &mut upsert_slot);
                out.push(Event::ReposRemoved(ids));
            }
            Event::ScanProgress { scan, .. } => match progress_slot.get(&scan) {
                Some(&i) => out[i] = event,
                None => {
                    progress_slot.insert(scan, out.len());
                    out.push(event);
                }
            },
            other => out.push(other),
        }
    }
    flush(&mut out, &mut upserts, &mut upsert_slot);
    out
}

/// Write the accumulated upserts into the slot they reserved, and close it.
fn flush(out: &mut [Event], upserts: &mut HashMap<RepoId, RepoSnapshot>, slot: &mut Option<usize>) {
    let Some(i) = slot.take() else { return };
    let mut snaps: Vec<RepoSnapshot> = std::mem::take(upserts).into_values().collect();
    // Deterministic order, so a re-render does not reshuffle rows the grid has
    // not been told to reorder.
    snaps.sort_by(|a, b| a.path.cmp(&b.path));
    out[i] = Event::ReposUpserted(snaps);
}
