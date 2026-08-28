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

/// What to probe.
#[derive(Debug, Clone)]
pub struct ProbeRequest {
    pub found: git_scylla_discovery::RepoFound,
    pub deadline: Instant,
}

/// What a caller wants to know about the refs of every repository in a plan.
///
/// One query per call, not one per repository — the question is the same for
/// every repository in the plan. Per-repository input lives in [`RefRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefQuery {
    /// Which branch this repository treats as its trunk.
    DefaultBranch,
    /// Every tag name this repository has.
    Tags,
    /// Is there a ref the user could check out by this name?
    Exists { rev: String },
}

/// One repository's side of a [`RefQuery`].
#[derive(Debug, Clone)]
pub struct RefRequest {
    pub git_dir: PathBuf,
    /// This repository's remote names, in configured order. Only
    /// [`RefQuery::DefaultBranch`] reads this.
    pub remotes: Vec<String>,
}

/// One repository's answer, in the same shape as the [`RefQuery`] that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefAnswer {
    /// `None` when neither `origin/HEAD` nor a `main`/`master` fallback was
    /// found — a real answer of "no trunk", distinct from [`RefError`].
    DefaultBranch(Option<String>),
    Tags(Vec<String>),
    /// `None` when the name carries revision syntax — `main~3`, a raw object
    /// id — and so cannot be answered from the filesystem. The caller lets
    /// the job try rather than refusing it.
    Exists(Option<bool>),
}

/// Why a repository could not be asked at all, as opposed to answering "no".
#[derive(Debug, thiserror::Error)]
pub enum RefError {
    #[error("could not read {path}: {source}")]
    Unreadable { path: PathBuf, source: std::io::Error },
    #[error("{path} is not a directory")]
    NotADirectory { path: PathBuf },
    /// The read panicked or the runtime dropped it before it finished.
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

/// The engine's only I/O seam: everything it knows about repository state
/// comes through here.
///
/// Boxed futures rather than `async fn`, so `Arc<dyn Probe>` stays object-safe.
/// `GitCliProbe` is the sole production implementation.
pub trait Probe: Send + Sync {
    /// Probe a repository.
    ///
    /// Infallible by construction: every failure — timeout, non-zero exit,
    /// git not on `PATH` — comes back as a snapshot carrying a
    /// [`git_scylla_core::ProbeOutcome`], never an `Err`.
    fn probe<'a>(&'a self, req: ProbeRequest) -> BoxFuture<'a, RepoSnapshot>;

    /// Answer one ref question for a whole plan's worth of repositories.
    ///
    /// Batched: one call takes an entire directory walk off the caller's task
    /// rather than one per repository. One `Result` per request, in the same
    /// order as `repos`.
    fn refs<'a>(
        &'a self,
        repos: Vec<RefRequest>,
        query: RefQuery,
    ) -> BoxFuture<'a, Vec<Result<RefAnswer, RefError>>>;

    /// Per-repository data too expensive to fetch for every row of a scan.
    /// Unimplemented until a caller needs it.
    fn detail<'a>(&'a self, _req: ProbeRequest) -> BoxFuture<'a, Result<RepoDetail, DetailError>> {
        Box::pin(async { Err(DetailError::NotImplemented) })
    }
}
