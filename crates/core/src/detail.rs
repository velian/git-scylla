use crate::Oid;
use serde::{Deserialize, Serialize};

/// Repository data that does not belong in a 100-row grid.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDetail {
    /// From `refs/remotes/<remote>/HEAD`.
    pub default_branch: Option<String>,
    pub tags: Vec<Tag>,
    pub stash_entries: Vec<StashEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    pub target: Oid,
    /// An annotated tag is its own object; a lightweight tag points straight at
    /// a commit.
    pub annotated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashEntry {
    /// Position in the reflog: `0` is `stash@{0}`.
    pub index: u32,
    pub message: String,
}
