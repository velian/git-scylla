//! Turning a discovered repository into a [`RepoSnapshot`].
//!
//! One `git status` per repository does almost all of it; the remainder is
//! reading a handful of marker files out of the git directory.

mod config;
#[cfg(feature = "testkit")]
mod fake;
mod git_cli;
mod gitdir;
mod porcelain;

use git_scylla_core::{RepoDetail, RepoSnapshot};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Instant;

pub use config::{host_of_url, parse_remotes};
#[cfg(feature = "testkit")]
pub use fake::{FakeProbe, FakeRepo};
// default_branch, has_ref and tags stay pub(crate): they are how GitCliProbe
// answers a RefQuery, and callers go through `Probe::refs`.
pub use git_cli::GitCliProbe;
pub use gitdir::{detect_in_progress, resolve_common_dir};
pub use porcelain::{parse_porcelain_v2, PorcelainStatus};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct ProbeRequest {
    pub found: git_scylla_discovery::RepoFound,
    pub deadline: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefQuery {
    DefaultBranch,
    Tags,
    Exists { rev: String },
}

#[derive(Debug, Clone)]
pub struct RefRequest {
    pub per_worktree_dir: PathBuf,
    pub remotes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefAnswer {
    DefaultBranch(Option<String>),
    Tags(Vec<String>),
    Exists(Option<bool>),
}

#[derive(Debug, thiserror::Error)]
pub enum RefError {
    #[error("could not read {path}: {source}")]
    Unreadable { path: PathBuf, source: std::io::Error },
    #[error("{path} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error("the ref read did not finish: {0}")]
    Interrupted(String),
}

#[derive(Debug, thiserror::Error)]
pub enum DetailError {
    #[error("not implemented until a feature needs it")]
    NotImplemented,
    #[error("{0}")]
    Failed(String),
}

pub trait Probe: Send + Sync {
    fn probe<'a>(&'a self, req: ProbeRequest) -> BoxFuture<'a, RepoSnapshot>;

    fn refs<'a>(
        &'a self,
        repos: Vec<RefRequest>,
        query: RefQuery,
    ) -> BoxFuture<'a, Vec<Result<RefAnswer, RefError>>>;

    fn detail<'a>(&'a self, _req: ProbeRequest) -> BoxFuture<'a, Result<RepoDetail, DetailError>> {
        Box::pin(async { Err(DetailError::NotImplemented) })
    }
}
