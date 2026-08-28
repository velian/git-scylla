use crate::expect::{Expect, FetchExpect, UpstreamExpect};
use crate::git::{Git, GitError};
use git_scylla_core::{Head, InProgress, Oid, RepoId, RepoKind, WorkTree};
use std::path::{Path, PathBuf};

/// One fixture repository and the snapshot it must produce.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub name: String,
    pub path: PathBuf,
    pub expect: Expect,
    /// Is this repository found only with `--nested`?
    pub nested_only: bool,
}

/// A built tree of fixture repositories.
pub struct FixtureSet {
    /// The directory everything was built in.
    pub dir: PathBuf,
    /// The directory tests should scan. Excludes `origins/` and `scratch/`, so
    /// the discovered set is exactly the fixtures.
    pub scan_root: PathBuf,
    pub fixtures: Vec<Fixture>,
}

impl FixtureSet {
    /// Build the whole set. `dir` must exist and should be empty.
    pub fn build(dir: &Path) -> Result<Self, GitError> {
        Builder::new(dir)?.build()
    }

    pub fn get(&self, name: &str) -> Option<&Fixture> {
        self.fixtures.iter().find(|f| f.name == name)
    }

    /// Fixtures a default (non-nested) scan should discover.
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
        // Canonicalize once: on macOS a temp dir is `/var/...` which is a
        // symlink to `/private/var/...`, and RepoId canonicalizes, so an
        // uncanonicalized expectation would never match.
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

    // ---- primitives ----------------------------------------------------

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

    /// A bare repository with one commit in it, to act as `origin`.
    ///
    /// Local, so the whole suite needs no network. It lives in
    /// `origins/`, outside the scan root, so it is not itself discovered.
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

    /// Advance `origin` by one commit, via the scratch clone made above.
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
        self.out.push(Fixture { name: name.to_string(), path, expect, nested_only: false });
    }

    fn push_nested(&mut self, name: &str, path: PathBuf, expect: Expect) {
        self.out.push(Fixture { name: name.to_string(), path, expect, nested_only: true });
    }

    // ---- the set -------------------------------------------------------

    fn build(mut self) -> Result<FixtureSet, GitError> {
        self.shapes()?;
        self.worktree_and_submodule()?;
        self.upstreams()?;
        self.worktree_states()?;
        self.in_progress()?;
        Ok(FixtureSet { dir: self.dir, scan_root: self.repos, fixtures: self.out })
    }

    /// Repository shapes.
    fn shapes(&mut self) -> Result<(), GitError> {
        // The baseline every other expectation is a delta from.
        let p = self.init("clean")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.push("clean", p, Expect::default());

        // Fresh `git init`: HEAD names a branch that has no commit.
        let p = self.init("unborn")?;
        self.push("unborn", p, Expect { head: Head::Unborn("main".into()), ..Default::default() });

        // Bare: no worktree, so working-tree state is meaningless rather than
        // clean, and `git status` is never run there at all.
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

        // Bare with commits, and with its refs packed — which is what a real
        // mirror looks like, since `git gc` packs them and `git clone --mirror`
        // arrives that way. The `bare` fixture above cannot stand in for it:
        // an empty `git init --bare` is genuinely unborn, so a probe that only
        // ever looked for a *loose* ref file passed against it while reporting
        // every populated mirror as unborn too.
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

        // A repository inside another repository's worktree. Pruned by default,
        // found with --nested.
        let outer = self.init("nested-outer")?;
        self.commit(&outer, "a.txt", "a\n", "c1")?;
        let inner = outer.join("vendor/inner");
        std::fs::create_dir_all(&inner).ok();
        self.g.run(&outer.join("vendor"), &["init", "inner"])?;
        self.commit(&inner, "b.txt", "b\n", "c1")?;
        // The inner repository is untracked content in the outer one.
        self.push(
            "nested-outer",
            outer,
            Expect { work: WorkTree { untracked: 1, ..Default::default() }, ..Default::default() },
        );
        self.push_nested("nested-inner", inner, Expect::default());
        Ok(())
    }

    /// The two `.git`-is-a-file shapes.
    fn worktree_and_submodule(&mut self) -> Result<(), GitError> {
        let main = self.init("worktree-main")?;
        self.commit(&main, "a.txt", "a\n", "c1")?;
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

        let sub_origin = self.origin_with_commit("submodule-sub")?;
        let sup = self.init("submodule-super")?;
        self.commit(&sup, "a.txt", "a\n", "c1")?;
        self.g.run(&sup, &["submodule", "add", path_str(&sub_origin), "sub"])?;
        self.g.run(&sup, &["commit", "-m", "add submodule"])?;
        self.push("submodule-super", sup.clone(), Expect::default());
        // Nested-only, and not by accident: a submodule lives inside its
        // superproject's worktree, so prune-on-match hides it from a default
        // scan exactly as it hides any other nested repository. That is the
        // right default for a bulk tool — "pull everything" should not
        // independently pull the submodules out from under their pins — and
        // `--nested` is how you ask for them.
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

    /// HEAD and upstream.
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

        // A remote is configured but this branch tracks nothing. Distinct from
        // in-sync, and the grid must not conflate them.
        let o = self.origin_with_commit("no-upstream")?;
        let p = self.init("no-upstream")?;
        self.commit(&p, "a.txt", "a\n", "c1")?;
        self.g.run(&p, &["remote", "add", "origin", path_str(&o)])?;
        self.push("no-upstream", p, tracked(UpstreamExpect::None));

        let o = self.origin_with_commit("ahead")?;
        let p = self.clone_from(&o, "ahead")?;
        self.commit(&p, "local.txt", "local\n", "local c1")?;
        self.push("ahead", p, tracked(sync(1, 0)));

        // "behind" only exists relative to a fetch: advance origin, then fetch
        // once.
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

        // Upstream configured, remote-tracking ref deleted. git then omits
        // `# branch.ab` entirely, which is the only signal that this is not
        // "in sync".
        let o = self.origin_with_commit("upstream-gone")?;
        let p = self.clone_from(&o, "upstream-gone")?;
        self.g.run(&p, &["update-ref", "-d", "refs/remotes/origin/main"])?;
        self.push(
            "upstream-gone",
            p,
            tracked(UpstreamExpect::Gone { remote: "origin", remote_ref: "origin/main".into() }),
        );

        // Combinations of an upstream with a dirty worktree.
        //
        // These exist for the preconditions, not for the probe. Every other
        // fixture varies one axis, which is right for the status parser — but a
        // precondition lives on the *intersection*: "behind and dirty" is what
        // decides whether a bulk pull touches uncommitted work. Without these,
        // deleting the clean-worktree rule leaves every test passing.
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

        // Behind with only an *untracked* file. Separate from the above because
        // it is the case where the clean requirement is most arguable: git would
        // happily pull, and the tool refuses. Having it in the table makes that
        // threshold visible rather than incidental.
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

        // Ahead and dirty: the case that proves push does *not* care about
        // worktree state while pull does.
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

    /// Working-tree states.
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

        // One path, both sides of the index. The case that makes WorkTree a
        // struct of counts rather than a state.
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

        // A rename produces a type-2 record, which occupies two NUL-separated
        // fields. Exercised here against real git output, not just the parser
        // unit tests.
        let p = self.init("renamed")?;
        self.commit(&p, "a.txt", "content\n", "c1")?;
        self.g.run(&p, &["mv", "a.txt", "b.txt"])?;
        self.push(
            "renamed",
            p,
            Expect { work: WorkTree { staged: 1, ..Default::default() }, ..Default::default() },
        );

        // Adversarial filenames. The newline is the important one: it is what a
        // line-oriented status parser gets wrong, and with `-z` git does not
        // quote it either.
        //
        // **Not** covered here: a filename containing non-UTF-8 bytes. APFS and
        // HFS+ reject them outright (EILSEQ), so the state cannot exist on the
        // only platform this tool targets. The parser is tested against those
        // bytes directly instead, in `probe::porcelain`, which is where the
        // behaviour actually lives.
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

    /// Half-finished operations.
    ///
    /// Every one is built by running a git command that *fails*, which is the
    /// only honest way to reach these states.
    fn in_progress(&mut self) -> Result<(), GitError> {
        // Two branches editing the same line, so any of these conflicts.
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

        // A merge that stopped on purpose rather than on a conflict: staged
        // content, no conflicts, MERGE_HEAD present.
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

        // A stopped rebase detaches HEAD, so the expectation carries the oid
        // the rebase is replaying onto.
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

        // Bisect checks out a commit, so this repository is detached too.
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

/// Create a file from a raw byte name, bypassing `&str`.
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
