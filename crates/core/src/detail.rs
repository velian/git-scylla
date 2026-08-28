use crate::Oid;
use serde::{Deserialize, Serialize};

/// Repository data that does not belong in a 100-row grid.
///
/// Fetched lazily, per repository, by an explicit caller. Nothing populates it
/// yet. It exists so the first feature needing tags or the remote's default
/// branch adds a `Probe` method, rather than three subprocesses to the path
/// that must finish in under a second for a hundred repositories.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDetail {
    /// From `refs/remotes/<remote>/HEAD` — `main` vs `master` is not uniform
    /// across a working set, so this is per repository and not configuration.
    pub default_branch: Option<String>,
    pub tags: Vec<Tag>,
    pub stash_entries: Vec<StashEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    pub target: Oid,
    /// An annotated tag is its own object; a lightweight tag points straight at
    /// a commit. The distinction matters to anything that creates or pushes one.
    pub annotated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashEntry {
    /// Position in the reflog: `0` is `stash@{0}`.
    pub index: u32,
    pub message: String,
}
