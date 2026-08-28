//! A [`Probe`] whose answers are written down rather than read off a disk.
//!
//! Behind the `testkit` feature, beside the trait it implements, so a change to
//! [`Probe`] breaks it in the same `cargo check` that changed the trait. It is
//! not in `crates/testkit`: that crate is the specification the real probe is
//! judged against, and it does not depend on `git-scylla-probe` — making it do
//! so would point the specification at the thing it judges.
//!
//! What this buys is planning tests without a filesystem. Resolving a
//! `SyncDefault` needs to know that one repository calls its trunk `master`
//! and another calls it `main`; proving the engine handles both should not
//! require two `git init`s, two clones and a push.

use crate::git_cli::looks_like_revision;
use crate::{BoxFuture, Probe, ProbeRequest, RefAnswer, RefError, RefQuery, RefRequest};
use git_scylla_core::{RepoId, RepoSnapshot};
use git_scylla_discovery::RepoFound;
use std::path::{Path, PathBuf};

/// One repository as a test wants it to look.
///
/// Built by overriding what the test is about and leaving the rest, the same
/// bargain [`RepoSnapshot::stub`] offers — a fixture that decided things its
/// test had no opinion on is how a passing suite stops meaning anything.
#[derive(Debug, Clone)]
pub struct FakeRepo {
    path: PathBuf,
    git_dir: PathBuf,
    snapshot: RepoSnapshot,
    default_branch: Option<String>,
    tags: Vec<String>,
    refs: Vec<String>,
    unreadable: bool,
}

impl FakeRepo {
    /// A normal repository on `main`, with `main` as its default branch, no
    /// tags and no other refs.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            git_dir: path.join(".git"),
            snapshot: RepoSnapshot::stub(path.clone()),
            default_branch: Some("main".to_string()),
            tags: Vec::new(),
            refs: vec!["main".to_string()],
            unreadable: false,
            path,
        }
    }

    /// The trunk this repository reports. The case worth testing is a working
    /// set where this differs from repository to repository.
    pub fn default_branch(mut self, name: &str) -> Self {
        self.default_branch = Some(name.to_string());
        self.refs.push(name.to_string());
        self
    }

    /// No `origin/HEAD` and no `main`/`master` to fall back to — an answer of
    /// "no", not a failure to read.
    pub fn no_default_branch(mut self) -> Self {
        self.default_branch = None;
        self
    }

    pub fn tags(mut self, tags: &[&str]) -> Self {
        self.tags = tags.iter().map(|t| t.to_string()).collect();
        self
    }

    /// Refs a checkout could name. Remote-tracking entries are written the way
    /// they are stored — `origin/main` — and answer to the branch name alone,
    /// because that is what `git checkout main` does with them.
    pub fn refs(mut self, refs: &[&str]) -> Self {
        self.refs = refs.iter().map(|r| r.to_string()).collect();
        self
    }

    /// Every ref question about this repository fails, as an unreadable git
    /// directory does. The case that separates [`RefError`] from an answer of
    /// "no".
    pub fn unreadable(mut self) -> Self {
        self.unreadable = true;
        self
    }

    /// Edit the snapshot this repository probes to.
    pub fn snapshot(mut self, edit: impl FnOnce(&mut RepoSnapshot)) -> Self {
        edit(&mut self.snapshot);
        self
    }

    /// What discovery would have reported for this repository.
    pub fn found(&self) -> RepoFound {
        RepoFound {
            id: RepoId::from_canonical(self.path.clone()),
            path: self.path.clone(),
            kind: self.snapshot.kind.clone(),
            git_dir: self.git_dir.clone(),
        }
    }

    /// The request that asks this repository a [`RefQuery`].
    pub fn request(&self) -> RefRequest {
        RefRequest {
            git_dir: self.git_dir.clone(),
            remotes: self.snapshot.remotes.iter().map(|r| r.name.clone()).collect(),
        }
    }

    /// Does a registered ref answer to this name?
    ///
    /// Exact, or the DWIM form: a stored `origin/main` answers a question about
    /// `main`, because `git checkout main` with no local branch creates one
    /// from it. [`crate::has_ref`] does the same, and a fake that did not would
    /// pass tests the real probe fails.
    fn has(&self, rev: &str) -> bool {
        self.refs.iter().any(|r| r == rev || r.rsplit_once('/').is_some_and(|(_, b)| b == rev))
    }
}

/// A [`Probe`] backed by a list of [`FakeRepo`]s.
///
/// Asking about a repository that was never registered panics rather than
/// inventing a plausible clean row. A test that forgot one should say so
/// immediately, not pass for the wrong reason.
#[derive(Debug, Default)]
pub struct FakeProbe {
    repos: Vec<FakeRepo>,
}

impl FakeProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, repo: FakeRepo) -> Self {
        self.repos.push(repo);
        self
    }

    fn by_path(&self, path: &Path) -> &FakeRepo {
        self.repos
            .iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("FakeProbe: no repository registered at {}", path.display()))
    }

    fn by_git_dir(&self, git_dir: &Path) -> &FakeRepo {
        self.repos.iter().find(|r| r.git_dir == git_dir).unwrap_or_else(|| {
            panic!("FakeProbe: no repository with git dir {}", git_dir.display())
        })
    }
}

impl Probe for FakeProbe {
    fn probe<'a>(&'a self, req: ProbeRequest) -> BoxFuture<'a, RepoSnapshot> {
        let snap = self.by_path(&req.found.path).snapshot.clone();
        Box::pin(async move { snap })
    }

    fn refs<'a>(
        &'a self,
        repos: Vec<RefRequest>,
        query: RefQuery,
    ) -> BoxFuture<'a, Vec<Result<RefAnswer, RefError>>> {
        let answers = repos
            .iter()
            .map(|req| {
                let repo = self.by_git_dir(&req.git_dir);
                if repo.unreadable {
                    return Err(RefError::Unreadable {
                        path: req.git_dir.clone(),
                        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                    });
                }
                Ok(match &query {
                    RefQuery::DefaultBranch => {
                        RefAnswer::DefaultBranch(repo.default_branch.clone())
                    }
                    RefQuery::Tags => RefAnswer::Tags(repo.tags.clone()),
                    // The unanswerable-by-filesystem rule is part of the
                    // contract, not an implementation detail of the real probe:
                    // a caller that mishandles `None` must fail here too.
                    RefQuery::Exists { rev } => {
                        RefAnswer::Exists((!looks_like_revision(rev)).then(|| repo.has(rev)))
                    }
                })
            })
            .collect();
        Box::pin(async move { answers })
    }
}
