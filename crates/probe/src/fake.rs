//! A [`Probe`] whose answers are written down rather than read off a disk.
//! Lets planning tests run without a filesystem

use crate::git_cli::looks_like_revision;
use crate::{BoxFuture, Probe, ProbeRequest, RefAnswer, RefError, RefQuery, RefRequest};
use git_scylla_core::{Remote, RepoId, RepoSnapshot};
use git_scylla_discovery::RepoFound;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

    pub fn default_branch(mut self, name: &str) -> Self {
        self.default_branch = Some(name.to_string());
        self.refs.push(name.to_string());
        self
    }

    pub fn no_default_branch(mut self) -> Self {
        self.default_branch = None;
        self
    }

    pub fn tags(mut self, tags: &[&str]) -> Self {
        self.tags = tags.iter().map(|t| t.to_string()).collect();
        self
    }

    pub fn refs(mut self, refs: &[&str]) -> Self {
        self.refs = refs.iter().map(|r| r.to_string()).collect();
        self
    }

    pub fn unreadable(mut self) -> Self {
        self.unreadable = true;
        self
    }

    pub fn snapshot(mut self, edit: impl FnOnce(&mut RepoSnapshot)) -> Self {
        edit(&mut self.snapshot);
        self
    }

    pub fn found(&self) -> RepoFound {
        RepoFound {
            id: RepoId::from_canonical(self.path.clone()),
            path: self.path.clone(),
            kind: self.snapshot.kind.clone(),
            git_dir: self.git_dir.clone(),
        }
    }

    pub fn request(&self) -> RefRequest {
        RefRequest {
            git_dir: self.git_dir.clone(),
            remotes: self.snapshot.remotes.iter().map(|r| r.name.clone()).collect(),
        }
    }

    fn has(&self, rev: &str) -> bool {
        self.refs.iter().any(|r| r == rev || r.rsplit_once('/').is_some_and(|(_, b)| b == rev))
    }
}

#[derive(Debug, Default)]
pub struct FakeProbe {
    repos: Vec<FakeRepo>,
    ref_delay: std::time::Duration,
}

impl FakeProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, repo: FakeRepo) -> Self {
        self.repos.push(repo);
        self
    }

    pub fn slow_refs(mut self, delay: std::time::Duration) -> Self {
        self.ref_delay = delay;
        self
    }

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
        let delay = self.ref_delay;
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            answers
        })
    }
}
