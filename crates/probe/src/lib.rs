//! Turning a discovered repository into a [`RepoSnapshot`].
//!
//! One `git status` per repository does almost all of it; the remainder is
//! reading a handful of marker files out of the git directory.

mod config;
mod git_cli;
mod gitdir;
mod porcelain;

use git_scylla_core::{RepoDetail, RepoSnapshot};
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

pub use config::{host_of_url, parse_remotes};
pub use git_cli::{default_branch, has_ref, tags, GitCliProbe};
pub use gitdir::{detect_in_progress, resolve_common_dir};
pub use porcelain::{parse_porcelain_v2, PorcelainStatus};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What to probe.
#[derive(Debug, Clone)]
pub struct ProbeRequest {
    pub found: git_scylla_discovery::RepoFound,
    pub deadline: Instant,
}

#[derive(Debug, thiserror::Error)]
pub enum DetailError {
    #[error("not implemented until a feature needs it")]
    NotImplemented,
    #[error("{0}")]
    Failed(String),
}

/// The seam that keeps the domain testable without a filesystem.
///
/// Boxed futures rather than `async fn` in the trait, because the engine wants
/// `Arc<dyn Probe>` and `async fn` in traits is not dyn-compatible. There is
/// exactly one production implementation.
pub trait Probe: Send + Sync {
    /// Probe a repository.
    ///
    /// Infallible by construction: every failure — timeout, non-zero exit, git
    /// not on `PATH` — comes back as a snapshot carrying a
    /// [`git_scylla_core::ProbeOutcome`]. A caller cannot accidentally drop a
    /// repository by ignoring an error, and a failed probe can never be
    /// rendered as "clean".
    fn probe<'a>(&'a self, req: ProbeRequest) -> BoxFuture<'a, RepoSnapshot>;

    /// The cold half of the model: per-repository data too expensive for a
    /// hundred-row scan.
    ///
    /// Nothing calls this yet. It is declared now so the first caller adds a
    /// body here rather than three fields to `RepoSnapshot` and three
    /// subprocesses to the scan.
    fn detail<'a>(&'a self, _req: ProbeRequest) -> BoxFuture<'a, Result<RepoDetail, DetailError>> {
        Box::pin(async { Err(DetailError::NotImplemented) })
    }
}
