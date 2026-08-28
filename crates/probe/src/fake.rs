//! A [`Probe`] whose answers are written down rather than read off a disk.
//!
//! Behind the `testkit` feature, beside the trait it implements, so a change
//! to [`Probe`] breaks it in the same `cargo check` that changed the trait.
//! Not in `crates/testkit`, since that crate does not depend on
//! `git-scylla-probe`.
//!
//! Lets planning tests run without a filesystem: a working set where one
//! repository calls its trunk `master` and another calls it `main` needs no
//! `git init`, clone, or push to construct.

use crate::git_cli::looks_like_revision;
use crate::{BoxFuture, Probe, ProbeRequest, RefAnswer, RefError, RefQuery, RefRequest};
use git_scylla_core::{Remote, RepoId, RepoSnapshot};
use git_scylla_discovery::RepoFound;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One repository as a test wants it to look.
///
/// Built by overriding what the test is about and leaving the rest at
/// [`RepoSnapshot::stub`]'s defaults.
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
    /// A normal repository on `main`, with `main` as its default branch, one
    /// remote called `origin`, no tags and no other refs.
    ///
    /// Two departures from [`RepoSnapshot::stub`]: `probed_at` is now, not
    /// the epoch, since an engine refuses to act on a stale snapshot; and
    /// there is a remote, since without one `SyncDefault` and a publishing
    /// `DevTag` skip with `NoRemote` before any ref question is asked.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut snapshot = RepoSnapshot::stub(path.clone());
        snapshot.probed_at = SystemTime::now();
        snapshot.remotes = vec![Remote { name: "origin".to_string(), host: None }];
        Self {
            git_dir: path.join(".git"),
            snapshot,
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
    /// Exact, or the DWIM form: a stored `origin/main` answers for `main`,
    /// matching what `git checkout main` does with no local branch.
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

    /// Create the empty `.git` directories discovery needs to find these
    /// repositories; everything discovery would have read out of one comes
    /// from this fake instead.
    pub fn scaffold(&self) -> std::io::Result<()> {
        for repo in &self.repos {
            std::fs::create_dir_all(&repo.git_dir)?;
        }
        Ok(())
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
                    // Mirrors the real probe's rule: revision syntax is
                    // unanswerable here too.
                    RefQuery::Exists { rev } => {
                        RefAnswer::Exists((!looks_like_revision(rev)).then(|| repo.has(rev)))
                    }
                })
            })
            .collect();
        Box::pin(async move { answers })
    }
}
