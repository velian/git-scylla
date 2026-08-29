use crate::expect::{Expect, FetchExpect, RefExpect, UpstreamExpect};
use crate::git::{Git, GitError};
use git_scylla_core::{Head, InProgress, Oid, RepoId, RepoKind, WorkTree};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Fixture {
    pub name: String,
    pub path: PathBuf,
    pub expect: Expect,
    pub refs: RefExpect,
    pub nested_only: bool,
}

pub struct FixtureSet {
    pub dir: PathBuf,
    pub scan_root: PathBuf,
    pub fixtures: Vec<Fixture>,
}

impl FixtureSet {
    pub fn build(dir: &Path) -> Result<Self, GitError> {
        Builder::new(dir)?.build()
    }

    pub fn get(&self, name: &str) -> Option<&Fixture> {
        self.fixtures.iter().find(|f| f.name == name)
    }

    pub fn discoverable(&self) -> impl Iterator<Item = &Fixture> {
        self.fixtures.iter().filter(|f| !f.nested_only)
    }
}

struct Builder {
    dir: PathBuf,
    repos: PathBuf,
    origins: PathBuf,
    scratch: PathBuf,
    g: Git,
    out: Vec<Fixture>,
}

impl Builder {
    fn new(dir: &Path) -> Result<Self, GitError> {
        let mkdir = |p: &Path| -> Result<(), GitError> {
            std::fs::create_dir_all(p).map_err(|e| GitError {
                args: vec!["mkdir".into()],
                cwd: p.to_path_buf(),
                code: -1,
                stderr: e.to_string(),
            })
        };
        mkdir(dir)?;
        let dir = dir.canonicalize().map_err(|e| GitError {
            args: vec!["canonicalize".into()],
            cwd: dir.to_path_buf(),
            code: -1,
            stderr: e.to_string(),
        })?;
        let (repos, origins, scratch, home) =
            (dir.join("repos"), dir.join("origins"), dir.join("scratch"), dir.join("home"));
        for p in [&repos, &origins, &scratch, &home] {
            mkdir(p)?;
        }
        Ok(Self { dir: dir.clone(), repos, origins, scratch, g: Git::new(home), out: Vec::new() })
    }

    fn init(&self, name: &str) -> Result<PathBuf, GitError> {
        self.g.run(&self.repos, &["init", name])?;
        Ok(self.repos.join(name))
    }

    fn write(&self, repo: &Path, file: &str, contents: &str) -> Result<(), GitError> {
        let path = repo.join(file);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, contents).map_err(|e| GitError {
            args: vec!["write".into(), file.into()],
            cwd: repo.to_path_buf(),
            code: -1,
            stderr: e.to_string(),
        })
    }

    fn commit(&self, repo: &Path, file: &str, contents: &str, msg: &str) -> Result<(), GitError> {
        self.write(repo, file, contents)?;
        self.g.run(repo, &["add", file])?;
        self.g.run(repo, &["commit", "-m", msg])?;
        Ok(())
    }

    fn head_oid(&self, repo: &Path) -> Result<Oid, GitError> {
        let s = self.g.run(repo, &["rev-parse", "HEAD"])?;
        Oid::parse(&s).map_err(|e| GitError {
            args: vec!["rev-parse".into()],
            cwd: repo.to_path_buf(),
            code: -1,
            stderr: e.to_string(),
        })
    }

    fn origin_with_commit(&self, name: &str) -> Result<PathBuf, GitError> {
        let bare = format!("{name}.git");
        self.g.run(&self.origins, &["init", "--bare", &bare])?;
        let origin = self.origins.join(&bare);
        let seed = self.scratch.join(name);
        self.g.run(&self.scratch, &["clone", path_str(&origin), name])?;
        self.commit(&seed, "shared.txt", "one\n", "c1")?;
        self.g.run(&seed, &["push", "-u", "origin", "main"])?;
        Ok(origin)
    }
    fn advance_origin(&self, name: &str, n: u32) -> Result<(), GitError> {
        let seed = self.scratch.join(name);
        self.commit(&seed, "shared.txt", &format!("one\nremote {n}\n"), &format!("remote c{n}"))?;
        self.g.run(&seed, &["push", "origin", "main"])?;
        Ok(())
    }

    fn clone_from(&self, origin: &Path, name: &str) -> Result<PathBuf, GitError> {
        self.g.run(&self.repos, &["clone", path_str(origin), name])?;
        Ok(self.repos.join(name))
    }

    fn push(&mut self, name: &str, path: PathBuf, expect: Expect) {
        let refs = RefExpect::default();
        self.out.push(Fixture { name: name.to_string(), path, expect, refs, nested_only: false });
    }

    fn push_nested(&mut self, name: &str, path: PathBuf, expect: Expect) {
        let refs = RefExpect::default();
        self.out.push(Fixture { name: name.to_string(), path, expect, refs, nested_only: true });
    }

    fn refs(&mut self, name: &str, refs: RefExpect) {
        let f = self.out.iter_mut().find(|f| f.name == name).expect("fixture pushed before refs");
        f.refs = refs;
    }

    fn build(mut self) -> Result<FixtureSet, GitError> {
        self.shapes()?;
        self.worktree_and_submodule()?;
        self.upstreams()?;
        self.worktree_states()?;
        self.in_progress()?;
        Ok(FixtureSet { dir: self.dir, scan_root: self.repos, fixtures: self.out })
    }
    fn shapes(&mut self) -> Result<(), GitError> {
        let p = self.init("clean")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.push("clean", p, Expect::default());

        let p = self.init("unborn")?;
        self.push("unborn", p, Expect { head: Head::Unborn("main".into()), ..Default::default() });
        self.refs("unborn", RefExpect::default().no_default_branch());

        self.g.run(&self.repos, &["init", "--bare", "bare.git"])?;
        self.push(
            "bare",
            self.repos.join("bare.git"),
            Expect {
                kind: RepoKind::Bare,
                head: Head::Unborn("main".into()),
                ..Default::default()
            },
        );
        self.refs("bare", RefExpect::default().no_default_branch());

        self.g.run(&self.repos, &["init", "--bare", "bare-packed.git"])?;
        let packed = self.repos.join("bare-packed.git");
        let seed = self.scratch.join("bare-packed");
        self.g.run(&self.scratch, &["clone", path_str(&packed), "bare-packed"])?;
        self.commit(&seed, "a.txt", "a\n", "c1")?;
        self.g.run(&seed, &["push", "origin", "main"])?;
        self.g.run(&packed, &["pack-refs", "--all"])?;
        self.push(
            "bare-packed",
            packed,
            Expect {
                kind: RepoKind::Bare,
                head: Head::Branch("main".into()),
                ..Default::default()
            },
        );

        let outer = self.init("nested-outer")?;
        self.commit(&outer, "a.txt", "a\n", "c1")?;
        let inner = outer.join("vendor/inner");
        std::fs::create_dir_all(&inner).ok();
        self.g.run(&outer.join("vendor"), &["init", "inner"])?;
        self.commit(&inner, "b.txt", "b\n", "c1")?;
        self.push(
            "nested-outer",
            outer,
            Expect { work: WorkTree { untracked: 1, ..Default::default() }, ..Default::default() },
        );
        self.push_nested("nested-inner", inner, Expect::default());
        Ok(())
    }

    fn worktree_and_submodule(&mut self) -> Result<(), GitError> {
        let main = self.init("worktree-main")?;
        self.commit(&main, "a.txt", "a\n", "c1")?;
        self.g.run(&main, &["tag", "v1.0.0"])?;
        self.g.run(&main, &["worktree", "add", "../worktree-linked", "-b", "wt"])?;
        self.push("worktree-main", main.clone(), Expect::default());
        self.push(
            "worktree-linked",
            self.repos.join("worktree-linked"),
            Expect {
                kind: RepoKind::Worktree { main: RepoId::from_canonical(&main) },
                head: Head::Branch("wt".into()),
                ..Default::default()
            },
        );
        let pair = RefExpect::default().tags(&["v1.0.0"]).exists(&[
            ("main", Some(true)),
            ("wt", Some(true)),
            ("v1.0.0", Some(true)),
            ("no-such-branch", Some(false)),
            // Revision syntax is unanswerable from the filesystem.
            ("main~3", None),
        ]);
        self.refs("worktree-main", pair.clone());
        self.refs("worktree-linked", pair);

        let sub_origin = self.origin_with_commit("submodule-sub")?;
        let sup = self.init("submodule-super")?;
        self.commit(&sup, "a.txt", "a\n", "c1")?;
        self.g.run(&sup, &["submodule", "add", path_str(&sub_origin), "sub"])?;
        self.g.run(&sup, &["commit", "-m", "add submodule"])?;
        self.push("submodule-super", sup.clone(), Expect::default());
        self.push_nested(
            "submodule-sub",
            sup.join("sub"),
            Expect {
                kind: RepoKind::Submodule { parent: RepoId::from_canonical(&sup) },
                remotes: vec!["origin".into()],
                fetch: FetchExpect::Due,
                upstream: UpstreamExpect::Sync {
                    remote: "origin",
                    remote_ref: "origin/main".into(),
                    ahead: 0,
                    behind: 0,
                },
                ..Default::default()
            },
        );
        Ok(())
    }

    fn upstreams(&mut self) -> Result<(), GitError> {
        let sync = |ahead, behind| UpstreamExpect::Sync {
            remote: "origin",
            remote_ref: "origin/main".into(),
            ahead,
            behind,
        };
        let tracked = |upstream| Expect {
            remotes: vec!["origin".into()],
            fetch: FetchExpect::Due,
            upstream,
            ..Default::default()
        };

        let o = self.origin_with_commit("in-sync")?;
        let p = self.clone_from(&o, "in-sync")?;
        self.push("in-sync", p, tracked(sync(0, 0)));

        let o = self.origin_with_commit("no-upstream")?;
        let p = self.init("no-upstream")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.g.run(&p, &["remote", "add", "origin", path_str(&o)])?;
        self.push("no-upstream", p, tracked(UpstreamExpect::None));

        let o = self.origin_with_commit("ahead")?;
        let p = self.clone_from(&o, "ahead")?;
        self.commit(&p, "local.txt", "local\n", "local c1")?;
        self.push("ahead", p, tracked(sync(1, 0)));

        let o = self.origin_with_commit("behind")?;
        let p = self.clone_from(&o, "behind")?;
        self.advance_origin("behind", 2)?;
        self.g.run(&p, &["fetch"])?;
        self.push("behind", p, tracked(sync(0, 1)));

        let o = self.origin_with_commit("diverged")?;
        let p = self.clone_from(&o, "diverged")?;
        self.commit(&p, "local.txt", "local\n", "local c1")?;
        self.advance_origin("diverged", 2)?;
        self.g.run(&p, &["fetch"])?;
        self.push("diverged", p, tracked(sync(1, 1)));

        let o = self.origin_with_commit("upstream-gone")?;
        let p = self.clone_from(&o, "upstream-gone")?;
        self.g.run(&p, &["update-ref", "-d", "refs/remotes/origin/main"])?;
        self.push(
            "upstream-gone",
            p,
            tracked(UpstreamExpect::Gone { remote: "origin", remote_ref: "origin/main".into() }),
        );

        let o = self.origin_with_commit("behind-dirty")?;
        let p = self.clone_from(&o, "behind-dirty")?;
        self.advance_origin("behind-dirty", 2)?;
        self.g.run(&p, &["fetch"])?;
        self.write(&p, "shared.txt", "locally edited\n")?;
        self.push(
            "behind-dirty",
            p,
            Expect {
                remotes: vec!["origin".into()],
                fetch: FetchExpect::Due,
                upstream: sync(0, 1),
                work: WorkTree { modified: 1, ..Default::default() },
                ..Default::default()
            },
        );

        let o = self.origin_with_commit("behind-untracked")?;
        let p = self.clone_from(&o, "behind-untracked")?;
        self.advance_origin("behind-untracked", 2)?;
        self.g.run(&p, &["fetch"])?;
        self.write(&p, "scratch.txt", "untracked\n")?;
        self.push(
            "behind-untracked",
            p,
            Expect {
                remotes: vec!["origin".into()],
                fetch: FetchExpect::Due,
                upstream: sync(0, 1),
                work: WorkTree { untracked: 1, ..Default::default() },
                ..Default::default()
            },
        );

        let o = self.origin_with_commit("ahead-dirty")?;
        let p = self.clone_from(&o, "ahead-dirty")?;
        self.commit(&p, "local.txt", "local\n", "local c1")?;
        self.write(&p, "shared.txt", "locally edited\n")?;
        self.push(
            "ahead-dirty",
            p,
            Expect {
                remotes: vec!["origin".into()],
                fetch: FetchExpect::Due,
                upstream: sync(1, 0),
                work: WorkTree { modified: 1, ..Default::default() },
                ..Default::default()
            },
        );

        let p = self.init("detached")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.commit(&p, "a.txt", "a2\n", "c2")?;
        self.g.run(&p, &["checkout", "--detach", "HEAD"])?;
        let oid = self.head_oid(&p)?;
        self.push("detached", p, Expect { head: Head::Detached(oid), ..Default::default() });
        Ok(())
    }

    fn worktree_states(&mut self) -> Result<(), GitError> {
        let p = self.init("untracked")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.write(&p, "new.txt", "new\n")?;
        self.push(
            "untracked",
            p,
            Expect { work: WorkTree { untracked: 1, ..Default::default() }, ..Default::default() },
        );

        let p = self.init("modified")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.write(&p, "a.txt", "changed\n")?;
        self.push(
            "modified",
            p,
            Expect { work: WorkTree { modified: 1, ..Default::default() }, ..Default::default() },
        );

        let p = self.init("staged")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.write(&p, "a.txt", "changed\n")?;
        self.g.run(&p, &["add", "a.txt"])?;
        self.push(
            "staged",
            p,
            Expect { work: WorkTree { staged: 1, ..Default::default() }, ..Default::default() },
        );

        let p = self.init("staged-and-modified")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.write(&p, "a.txt", "staged\n")?;
        self.g.run(&p, &["add", "a.txt"])?;
        self.write(&p, "a.txt", "then modified\n")?;
        self.push(
            "staged-and-modified",
            p,
            Expect {
                work: WorkTree { staged: 1, modified: 1, ..Default::default() },
                ..Default::default()
            },
        );

        let p = self.init("renamed")?;
        self.commit(&p, "a.txt", "content\n", "c1")?;
        self.g.run(&p, &["mv", "a.txt", "b.txt"])?;
        self.push(
            "renamed",
            p,
            Expect { work: WorkTree { staged: 1, ..Default::default() }, ..Default::default() },
        );

        let p = self.init("awkward-names")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.write(&p, "with space.txt", "x\n")?;
        write_raw_name(&p, b"with\nnewline.txt")?;
        write_raw_name(&p, b"with\"quote'and\\backslash.txt")?;
        self.push(
            "awkward-names",
            p,
            Expect { work: WorkTree { untracked: 3, ..Default::default() }, ..Default::default() },
        );

        let p = self.init("stashed")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.write(&p, "a.txt", "changed\n")?;
        self.g.run(&p, &["stash"])?;
        self.push("stashed", p, Expect { stashes: 1, ..Default::default() });
        Ok(())
    }

    fn in_progress(&mut self) -> Result<(), GitError> {
        let diverge = |b: &Builder, p: &Path| -> Result<(), GitError> {
            b.commit(p, "a.txt", "base\n", "c1")?;
            b.g.run(p, &["checkout", "-b", "other"])?;
            b.commit(p, "a.txt", "other\n", "c-other")?;
            b.g.run(p, &["checkout", "main"])?;
            b.commit(p, "a.txt", "main\n", "c-main")?;
            Ok(())
        };

        let p = self.init("conflicted")?;
        diverge(self, &p)?;
        self.g.run_expect_failure(&p, &["merge", "other"])?;
        self.push(
            "conflicted",
            p,
            Expect {
                work: WorkTree { conflicted: 1, ..Default::default() },
                op: Some(InProgress::Merge),
                ..Default::default()
            },
        );

        let p = self.init("merge-in-progress")?;
        self.commit(&p, "a.txt", "base\n", "c1")?;
        self.g.run(&p, &["checkout", "-b", "other"])?;
        self.commit(&p, "b.txt", "other\n", "c-other")?;
        self.g.run(&p, &["checkout", "main"])?;
        self.commit(&p, "c.txt", "main\n", "c-main")?;
        self.g.run(&p, &["merge", "--no-commit", "--no-ff", "other"])?;
        self.push(
            "merge-in-progress",
            p,
            Expect {
                work: WorkTree { staged: 1, ..Default::default() },
                op: Some(InProgress::Merge),
                ..Default::default()
            },
        );

        let p = self.init("rebase-in-progress")?;
        diverge(self, &p)?;
        self.g.run_expect_failure(&p, &["rebase", "other"])?;
        let oid = self.head_oid(&p)?;
        self.push(
            "rebase-in-progress",
            p,
            Expect {
                head: Head::Detached(oid),
                work: WorkTree { conflicted: 1, ..Default::default() },
                op: Some(InProgress::Rebase),
                ..Default::default()
            },
        );

        let p = self.init("cherry-pick-in-progress")?;
        diverge(self, &p)?;
        self.g.run_expect_failure(&p, &["cherry-pick", "other"])?;
        self.push(
            "cherry-pick-in-progress",
            p,
            Expect {
                work: WorkTree { conflicted: 1, ..Default::default() },
                op: Some(InProgress::CherryPick),
                ..Default::default()
            },
        );

        let p = self.init("revert-in-progress")?;
        self.commit(&p, "a.txt", "one\n", "c1")?;
        self.commit(&p, "a.txt", "two\n", "c2")?;
        self.commit(&p, "a.txt", "three\n", "c3")?;
        self.g.run_expect_failure(&p, &["revert", "HEAD~1"])?;
        self.push(
            "revert-in-progress",
            p,
            Expect {
                work: WorkTree { conflicted: 1, ..Default::default() },
                op: Some(InProgress::Revert),
                ..Default::default()
            },
        );

        let p = self.init("bisect-in-progress")?;
        self.commit(&p, "a.txt", "one\n", "c1")?;
        let first = self.head_oid(&p)?;
        self.commit(&p, "a.txt", "two\n", "c2")?;
        self.commit(&p, "a.txt", "three\n", "c3")?;
        self.g.run(&p, &["bisect", "start"])?;
        self.g.run(&p, &["bisect", "bad", "HEAD"])?;
        self.g.run(&p, &["bisect", "good", first.as_str()])?;
        let oid = self.head_oid(&p)?;
        self.push(
            "bisect-in-progress",
            p,
            Expect {
                head: Head::Detached(oid),
                op: Some(InProgress::Bisect),
                ..Default::default()
            },
        );
        Ok(())
    }
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("fixture paths are ASCII by construction")
}

fn write_raw_name(repo: &Path, name: &[u8]) -> Result<(), GitError> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let path = repo.join(OsStr::from_bytes(name));
    std::fs::write(&path, b"x\n").map_err(|e| GitError {
        args: vec!["write-raw".into()],
        cwd: repo.to_path_buf(),
        code: -1,
        stderr: e.to_string(),
    })
}
