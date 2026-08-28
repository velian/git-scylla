//! Where the last run's transcripts live, so `git-scylla log` can find them.
//! One file, overwritten each run, not a growing history.

use git_scylla_core::{Action, BatchId, Job};
use serde::{Deserialize, Serialize};

const FILE: &str = "last-run.json";

#[derive(Debug, Serialize, Deserialize)]
pub struct LastRun {
    pub batch: BatchId,
    pub action: Action,
    pub jobs: Vec<Job>,
}

/// Record a finished batch. A save failure only warns: it must not turn a
/// successful batch into a failed command.
pub fn save(run: &LastRun) {
    if let Err(e) = git_scylla_store::save_json(FILE, run) {
        tracing::warn!(%e, "could not save transcripts");
    }
}

pub fn load() -> Option<LastRun> {
    git_scylla_store::load_json(FILE)
}
