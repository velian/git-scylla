//! What a grid row is: a `RepoSnapshot` plus what the grid derives from it.

use git_scylla_core::{Badge, FetchStatus, RepoSnapshot};
use serde::Serialize;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RepoRow {
    #[serde(flatten)]
    pub snapshot: RepoSnapshot,
    pub badge: Badge,
    pub badge_label: String,
    pub stale: bool,
    pub badge_rank: u8,
    pub status: String,
    pub fetch_cell: FetchCell,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FetchCell {
    pub status: FetchStatus,
    pub problem: bool,
    pub detail: Option<String>,
}

impl FetchCell {
    fn from(snapshot: &RepoSnapshot) -> Self {
        let status = snapshot.fetch_status();
        let problem = status.is_problem();
        let detail = match &status {
            FetchStatus::Quarantined { reason } => Some(reason.clone()),
            FetchStatus::BackingOff { failures, .. } => {
                Some(format!("{failures} consecutive failures"))
            }
            _ => None,
        };
        Self { status, problem, detail }
    }
}

impl RepoRow {
    pub fn at(snapshot: RepoSnapshot, now: SystemTime, max_age: Duration) -> Self {
        let badge = snapshot.badge();
        Self {
            badge,
            badge_label: badge.to_string(),
            badge_rank: badge as u8,
            stale: snapshot.is_stale(now, max_age),
            status: snapshot.status_line(),
            fetch_cell: FetchCell::from(&snapshot),
            snapshot,
        }
    }
}

impl From<RepoSnapshot> for RepoRow {
    fn from(snapshot: RepoSnapshot) -> Self {
        Self::at(snapshot, SystemTime::now(), git_scylla_engine::Policy::default().max_snapshot_age)
    }
}
