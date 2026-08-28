//! Where the last run's transcripts live, so `git-scylla log` can find them.
//!
//! One file, overwritten each run. Not a history: the question this answers is
//! the immediate one — "the batch just finished, what happened to number 37".
//! A growing archive would be a different feature with different questions
//! (pruning, size, privacy) and nobody has asked for it.
//!
//! The state directory and the atomic write are `crates/store`'s; what stays
//! here is the shape of the file and what it is for.

use git_scylla_core::{Action, BatchId, Job};
use serde::{Deserialize, Serialize};

/// The file, under `$GIT_SCYLLA_STATE_DIR` or the application-support
/// directory.
const FILE: &str = "last-run.json";

#[derive(Debug, Serialize, Deserialize)]
pub struct LastRun {
    pub batch: BatchId,
    pub action: Action,
    pub jobs: Vec<Job>,
}

/// Record a finished batch. Failures are warnings: losing the transcript is
/// annoying, and turning a successful batch into a failed command over it would
/// be worse.
pub fn save(run: &LastRun) {
    if let Err(e) = git_scylla_store::save_json(FILE, run) {
        tracing::warn!(%e, "could not save transcripts");
    }
}

pub fn load() -> Option<LastRun> {
    git_scylla_store::load_json(FILE)
}
