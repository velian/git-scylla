//! What a grid row is: a `RepoSnapshot` plus what the grid derives from it.

use git_scylla_core::{duration, Badge, FetchStatus, RepoSnapshot};
use serde::Serialize;
use std::time::{Duration, SystemTime};

/// A snapshot plus what the grid derives from it. `#[serde(flatten)]` keeps
/// the wire shape one flat object: a row is a repository with extra columns.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RepoRow {
    #[serde(flatten)]
    pub snapshot: RepoSnapshot,
    pub badge: Badge,
    /// The badge as a word, from `Badge`'s `Display`. Also the CSS class.
    pub badge_label: String,
    /// Not verified by the running process, or verified too long ago. The
    /// same predicate the `SnapshotStale` precondition uses.
    pub stale: bool,
    /// Sort priority, worst first. A number because the ordering is
    /// [`Badge`]'s declaration order, which a TypeScript string union cannot
    /// express.
    pub badge_rank: u8,
    /// The compact status column, from `RepoSnapshot::status_line` — the
    /// same string the CLI's STATUS column shows.
    pub status: String,
    pub fetch_cell: FetchCell,
}

/// The fetch column, phrased for a grid. The decision is
/// `RepoSnapshot::fetch_status`, shared with the CLI; only the phrasing is
/// here.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FetchCell {
    pub text: String,
    /// The grid offers "Fetch now" on exactly these.
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
                format!(
                    "retrying {}",
                    duration::brief(until.duration_since(now).unwrap_or_default())
                ),
                Some(format!("{failures} consecutive failures")),
            ),
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
        Self::at(snapshot, SystemTime::now(), git_scylla_engine::Policy::default().max_snapshot_age)
    }
}
