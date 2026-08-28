//! What a grid row is.
//!
//! `Badge` is derived by `RepoSnapshot::badge()`, the grid needs it for display
//! *and* for its default sort, and reimplementing that derivation in TypeScript
//! would be exactly the duplicate logic the generated bindings exist to
//! prevent.
//!
//! So the bridge projects rather than the frontend deriving. It lives here and
//! not in `crates/engine` because it is presentation: the engine publishes
//! facts, and which of them a grid shows is not its concern.

use git_scylla_core::{duration, Badge, FetchStatus, RepoSnapshot};
use serde::Serialize;
use std::time::{Duration, SystemTime};

/// A snapshot plus what the grid derives from it.
///
/// `#[serde(flatten)]` so the wire shape is one flat object — a row is a
/// repository with two extra columns, not a repository wrapped in something.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RepoRow {
    #[serde(flatten)]
    pub snapshot: RepoSnapshot,
    pub badge: Badge,
    /// The badge as a word, from `Badge`'s `Display`.
    ///
    /// Carried rather than derived on the other side, which had been
    /// lowercasing the variant name — a transliteration that holds only while
    /// every variant is a single word, and `InProgress` lowercases to
    /// `inprogress`. Phrasing a domain value is Rust's job here as everywhere
    /// else, and this is also the CSS class, so the two cannot disagree.
    pub badge_label: String,
    /// Not verified by the running process, or verified too long ago.
    ///
    /// Derived here rather than in the grid for the same reason `badge` is, and
    /// with an extra one: this is the *same* predicate the `SnapshotStale`
    /// precondition uses, so a row shown as current can never be one an action
    /// then refuses as stale. Two spellings of "stale" would let the tool
    /// contradict itself on screen.
    pub stale: bool,
    /// Sort priority, worst first.
    ///
    /// Sent as a number because the ordering lives in the declaration order of
    /// [`Badge`], which a TypeScript string union cannot express. A hand-kept
    /// array of badge names on the other side would be that ordering written
    /// down twice, and the second copy would be the one that rots.
    pub badge_rank: u8,
    /// The compact status column, from `RepoSnapshot::status_line`.
    ///
    /// The same string the CLI's STATUS column shows, because it is the same
    /// column. It was written twice — once here in Rust and once in
    /// `columns.ts` — and the two had already parted company over what a bare
    /// repository with an upstream should read as.
    pub status: String,
    pub fetch_cell: FetchCell,
}

/// The fetch column, phrased for a grid.
///
/// The *decision* is `RepoSnapshot::fetch_status`, shared with the CLI. Only
/// the phrasing is here, and it differs on purpose: this cell has a tooltip and
/// a button behind it, so it says the short thing and puts the reason in
/// `detail`, where the CLI has one line and must inline it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FetchCell {
    pub text: String,
    /// Something is wrong and the user can act on it — the grid offers
    /// "Fetch now" on exactly these.
    pub problem: bool,
    /// The tooltip: the quarantine's error, or the failure count.
    pub detail: Option<String>,
}

impl FetchCell {
    fn at(snapshot: &RepoSnapshot, now: SystemTime) -> Self {
        let status = snapshot.fetch_status();
        let problem = status.is_problem();
        let (text, detail) = match status {
            FetchStatus::NoRemote => ("no remote".to_string(), None),
            FetchStatus::Off => ("off".to_string(), None),
            FetchStatus::Quarantined { reason } => ("quarantined".to_string(), Some(reason)),
            FetchStatus::BackingOff { until, failures } => (
                // `duration_since` fails when the retry is already due, which
                // is a zero-length wait and not an error.
                format!(
                    "retrying {}",
                    duration::brief(until.duration_since(now).unwrap_or_default())
                ),
                Some(format!("{failures} consecutive failures")),
            ),
            // A stamp in the future means the clock moved. `since` reads that
            // as "just now", which is the honest answer and the one the CLI
            // gives.
            FetchStatus::Fetched { at } => {
                (duration::since(now.duration_since(at).unwrap_or(Duration::ZERO)), None)
            }
            FetchStatus::Never => ("never".to_string(), None),
        };
        Self { text, problem, detail }
    }
}

impl RepoRow {
    /// Project a snapshot, judging staleness against `now`.
    pub fn at(snapshot: RepoSnapshot, now: SystemTime, max_age: Duration) -> Self {
        let badge = snapshot.badge();
        Self {
            badge,
            badge_label: badge.to_string(),
            badge_rank: badge as u8,
            stale: snapshot.is_stale(now, max_age),
            status: snapshot.status_line(),
            fetch_cell: FetchCell::at(&snapshot, now),
            snapshot,
        }
    }
}

impl From<RepoSnapshot> for RepoRow {
    fn from(snapshot: RepoSnapshot) -> Self {
        // The engine's default bound, which is also its default policy: the two
        // are the same number because they are the same question.
        Self::at(snapshot, SystemTime::now(), git_scylla_engine::Policy::default().max_snapshot_age)
    }
}
