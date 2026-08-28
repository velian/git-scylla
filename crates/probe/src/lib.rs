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

/// What a caller wants to know about the refs of every repository in a plan.
///
/// One query per call, not one per repository: the question comes from the
/// *action*, which the user chose once, so a plan asks the same thing of all
/// forty. What differs per repository is in [`RefRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefQuery {
    /// Which branch this repository treats as its trunk.
    ///
    /// `main` here and `master` there is exactly why this cannot be answered
    /// from a [`RepoSnapshot`], and why `Action::SyncDefault` carries an
    /// `Option` until it has been asked.
    DefaultBranch,
    /// Every tag name this repository has.
    Tags,
    /// Is there a ref the user could check out by this name?
    Exists { rev: String },
}

/// One repository's side of a [`RefQuery`].
///
/// Carries `remotes` because [`RefQuery::DefaultBranch`] needs this
/// repository's own remote names in their configured order, and no other
/// variant reads it. A field that one variant uses is a smaller lie than a
/// query that has to be rebuilt per repository — the query is what the user
/// asked, and asking it forty times would say the opposite.
#[derive(Debug, Clone)]
pub struct RefRequest {
    pub git_dir: PathBuf,
    pub remotes: Vec<String>,
}

/// One repository's answer, in the same shape as the [`RefQuery`] that asked.
///
/// The variants cannot disagree with the query in practice — one adapter
/// answers one question — but the type cannot say so without generics, and
/// generics would cost `dyn` compatibility, which the engine needs more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefAnswer {
    /// `None` when neither `origin/HEAD` nor a `main`/`master` fallback was
    /// found. A real answer of "this repository has no trunk I can name",
    /// distinct from [`RefError`].
    DefaultBranch(Option<String>),
    Tags(Vec<String>),
    /// `None` when the name carries revision syntax and so cannot be answered
    /// from the filesystem at all — see [`has_ref`]. The caller lets the job
    /// try rather than refusing it.
    Exists(Option<bool>),
}

/// Why a repository could not be asked at all.
///
/// Deliberately separate from an answer of "no". A directory that cannot be
/// read is a repository whose default branch is *unknown*, and reporting that
/// as `NoDefaultBranch` puts a sentence in a plan — "this repository has no
/// default branch" — that may simply be false about a repository that has one.
#[derive(Debug, thiserror::Error)]
pub enum RefError {
    #[error("could not read {path}: {source}")]
    Unreadable { path: PathBuf, source: std::io::Error },
    #[error("{path} is not a directory")]
    NotADirectory { path: PathBuf },
    /// The read did not finish — it panicked, or the runtime dropped it.
    ///
    /// An error rather than a propagated panic: these reads run on behalf of an
    /// actor that owns every map in the engine, and taking that task down over
    /// one unreadable repository would lose the working set.
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

    /// Answer one ref question for a whole plan's worth of repositories.
    ///
    /// **This is the seam's other half.** Planning a `SyncDefault`, a `DevTag`
    /// or a `Checkout` needs facts that live in `refs/` and cannot be answered
    /// from a [`RepoSnapshot`]; asking them through the trait is what makes
    /// "the probe is the only I/O seam the engine has" true rather than nearly
    /// true.
    ///
    /// Batched, and that is the point. Per-repository calls would put a
    /// directory walk per row on the caller's task; one call lets an adapter
    /// take all of them off it at once.
    ///
    /// One `Result` per request, in the same order. Deliberately not a map: the
    /// caller already holds the ids it built the requests from, and a returned
    /// map invites the shape where a repository is in neither the answers nor
    /// the errors.
    ///
    /// No default body. A ref question that silently answered "nothing found"
    /// would be a plan quietly skipping every repository, which is the failure
    /// this seam exists to make impossible.
    fn refs<'a>(
        &'a self,
        repos: Vec<RefRequest>,
        query: RefQuery,
    ) -> BoxFuture<'a, Vec<Result<RefAnswer, RefError>>>;

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
