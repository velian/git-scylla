use crate::skip::{is_git_dir, is_hard_skipped, looks_dataless};
use git_scylla_core::{RepoId, RepoKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

/// One discovered repository, emitted as it is found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFound {
    /// The repository's identity, resolved **here and only here**.
    ///
    /// Every consumer takes this rather than re-deriving it. Not just tidiness:
    /// canonicalizing the same path twice at different moments can give two
    /// different answers if the path vanishes in between, and two answers for
    /// one repository means a map keyed on the first is never cleared by the
    /// second. The engine's scan bookkeeping hit exactly that.
    pub id: RepoId,
    /// The repository's worktree root, or the repository itself when bare.
    pub path: PathBuf,
    pub kind: RepoKind,
    /// The resolved git directory. Discovery has already read the `.git` file
    /// for linked worktrees and submodules, so the probe — which needs it for
    /// in-progress detection and the config read — should not have to again.
    pub git_dir: PathBuf,
}

/// Something the walk could not read.
///
/// Reported to the caller rather than only logged, because on macOS this is
/// almost always TCC: an unsigned app scanning `~/Documents` gets permission
/// denied per directory and finds nothing, which is indistinguishable from an
/// empty working set unless somebody says so. That message is the highest-value
/// error in the application, and it cannot be written without this.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "type", content = "value")]
pub enum DiscoveryError {
    #[error("root {0} does not exist or is not readable")]
    UnusableRoot(PathBuf),
    /// A directory inside a root that could not be read. The walk continues.
    #[error("could not read {path}: {reason}")]
    Unreadable { path: PathBuf, reason: String },
    /// How many further unreadable directories were not listed.
    ///
    /// A tree the user has no access to produces one of these per directory,
    /// and a UI does not need ten thousand of them to say "grant access".
    #[error("and {0} more unreadable")]
    MoreUnreadable(usize),
}

/// Unreadable directories reported individually before collapsing to a count.
const MAX_UNREADABLE: usize = 20;

#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// Descend into repositories to find nested ones. Off by default: a `.git`
    /// inside a checked-out dependency tree is almost never a repository the
    /// user means.
    pub nested: bool,
    pub max_depth: Option<usize>,
}

pub struct Walker {
    roots: Vec<PathBuf>,
    opts: WalkOptions,
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
struct Found {
    /// Repository roots discovered so far, used to prune their subtrees.
    ///
    /// A `Vec` scanned by ancestor comparison rather than a set: pruning asks
    /// "is any ancestor of this path a repository", which is a prefix question,
    /// and at fewer than a hundred repositories a linear scan of ancestors
    /// against a small vector is faster than hashing the path.
    roots: Vec<PathBuf>,
}

impl Walker {
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
            opts: WalkOptions::default(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn options(mut self, opts: WalkOptions) -> Self {
        self.opts = opts;
        self
    }

    /// A flag that abandons the walk when set.
    ///
    /// The walk is blocking filesystem work, so it cannot be cancelled by
    /// dropping a future. Checked per directory entry, which is fine-grained
    /// enough: the cost of the check is a relaxed atomic load against a `stat`.
    /// Needed because a root chosen by accident — `/`, a network mount — must be
    /// abandonable rather than something the user waits out.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Walk every root, sending each repository as it is found.
    ///
    /// Blocking: run it on a dedicated thread. Returns the count sent, and the
    /// roots that could not be walked at all — a root that does not exist is a
    /// configuration error worth reporting, while an unreadable directory
    /// *inside* a root is a warning that must not abort the walk.
    pub fn walk(&self, tx: UnboundedSender<RepoFound>) -> (usize, Vec<DiscoveryError>) {
        let mut fatal = Vec::new();
        let mut usable = Vec::new();
        for root in &self.roots {
            match root.canonicalize() {
                Ok(c) if c.is_dir() => usable.push(c),
                _ => fatal.push(DiscoveryError::UnusableRoot(root.clone())),
            }
        }
        if usable.is_empty() {
            return (0, fatal);
        }
        // Two roots naming one directory are one root. Deduplicated here so
        // that nothing downstream has to cope with a repository arriving twice.
        usable.sort();
        usable.dedup();

        let found = Arc::new(Mutex::new(Found::default()));
        let count = Arc::new(Mutex::new(0usize));

        // Roots are classified here rather than in the filter below, because
        // `ignore` never calls `filter_entry` for a depth-0 entry — it yields
        // the root and only filters what is under it. So a root that is itself
        // a repository was never looked at, and `git-scylla scan ~/one-repo`
        // answered "no repositories found": the same sentence an empty
        // directory gets.
        //
        // Doing it before the walk starts, rather than from the iterator body,
        // also keeps the prune decision independent of when entries are yielded
        // relative to when they are filtered: `Found::roots` already holds every
        // root by the time the first child is filtered, so a root repository's
        // subtree prunes through the same ancestor check as everyone else's.
        for root in &usable {
            if excluded(root, &usable) {
                continue;
            }
            if !self.opts.nested && covered(&found, root) {
                continue;
            }
            accept(root, &found, &count, &tx);
        }

        let mut builder = ignore::WalkBuilder::new(&usable[0]);
        for r in &usable[1..] {
            builder.add(r);
        }
        builder
            // A raw walk. Gitignore semantics would be actively wrong: the
            // repository we want is frequently inside an ignored directory.
            .standard_filters(false)
            .hidden(false)
            .parents(false)
            // Never follow symlinks: it is the only defence against a symlink
            // loop in the scan root, and it costs nothing we want.
            .follow_links(false)
            .max_depth(self.opts.max_depth)
            // Serial. Discovery is not the bottleneck, and a deterministic
            // depth-first order makes prune-on-match a local decision rather
            // than a race.
            .threads(1);

        let roots_for_skip = usable.clone();
        let nested = self.opts.nested;
        let stop = Arc::clone(&self.stop);
        let f = Arc::clone(&found);
        let c = Arc::clone(&count);
        let sender = tx.clone();

        // All the real work happens in the filter, not in the iterator body.
        // `ignore` only lets us prevent descent from here, and doing detection
        // here too means the prune decision for a child is always made after
        // its parent has been classified — no reliance on when entries are
        // yielded relative to when they are filtered.
        builder.filter_entry(move |entry| {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let path = entry.path();
            // Files are never interesting: we look only for directories, and
            // declining them here also spares the caller a yield per file.
            if !entry.file_type().is_some_and(|t| t.is_dir()) {
                return false;
            }
            if excluded(path, &roots_for_skip) {
                tracing::trace!(path = %path.display(), "skipped");
                return false;
            }
            if !nested && covered(&f, path) {
                return false;
            }
            accept(path, &f, &c, &sender);
            // Yield it either way so `ignore` descends once more; if it was a
            // repository, every child is then rejected by the ancestor check
            // above. One extra directory listing per repository, which at this
            // scale is cheaper than the bookkeeping to avoid it.
            true
        });

        let mut unreadable = 0usize;
        for result in builder.build() {
            if let Err(err) = result {
                // A directory we cannot read never ends the walk: one locked
                // folder must not cost the user every repository below its
                // siblings. But it is reported, because a scan that silently
                // finds nothing is the worst thing this tool can do.
                tracing::debug!(%err, "unreadable during walk");
                unreadable += 1;
                if unreadable <= MAX_UNREADABLE {
                    let (path, reason) = describe(&err);
                    fatal.push(DiscoveryError::Unreadable { path, reason });
                }
            }
        }
        if unreadable > MAX_UNREADABLE {
            fatal.push(DiscoveryError::MoreUnreadable(unreadable - MAX_UNREADABLE));
        }

        let n = *count.lock().expect("discovery state poisoned");
        (n, fatal)
    }
}

/// Split a walk error into the path it concerns and why.
///
/// `ignore` wraps errors in `WithPath`/`WithDepth`, and the display form
/// repeats the path inside the message. A UI wants the two separately.
fn describe(err: &ignore::Error) -> (PathBuf, String) {
    let mut path = PathBuf::new();
    let mut current = err;
    loop {
        match current {
            ignore::Error::WithPath { path: p, err } => {
                path = p.clone();
                current = err;
            }
            ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
                current = err;
            }
            other => {
                let reason = match other.io_error() {
                    Some(io) => io.to_string(),
                    None => other.to_string(),
                };
                return (path, reason);
            }
        }
    }
}

/// Is this directory one the walk will neither classify nor look inside?
///
/// `roots` are exempt from the name-based skips: naming `/Volumes/work` as a
/// root is an explicit request that must beat the generic list. A `.git`
/// directory is **not** exempt, however it was reached — it is machinery, and
/// the bare-repository test would otherwise match it exactly.
fn excluded(dir: &Path, roots: &[PathBuf]) -> bool {
    is_git_dir(dir) || looks_dataless(dir) || is_hard_skipped(dir, roots)
}

/// Is `dir` inside a repository already discovered?
fn covered(found: &Mutex<Found>, dir: &Path) -> bool {
    found.lock().expect("discovery state poisoned").roots.iter().any(|r| dir.starts_with(r))
}

/// Classify `dir` and, if it is a repository, record and emit it.
///
/// Shared by the root pre-pass and the walk filter, which must agree: a root
/// that is a repository has to arrive looking exactly like one found three
/// levels down.
fn accept(dir: &Path, found: &Mutex<Found>, count: &Mutex<usize>, tx: &UnboundedSender<RepoFound>) {
    let Some((kind, git_dir)) = classify(dir) else { return };
    found.lock().expect("discovery state poisoned").roots.push(dir.to_path_buf());
    *count.lock().expect("discovery state poisoned") += 1;
    // `from_canonical` without a syscall is sound here: the roots were
    // canonicalized above, and `follow_links(false)` means no component below
    // them is ever a symlink, so every path this walk yields is already
    // canonical.
    let _ = tx.send(RepoFound {
        id: RepoId::from_canonical(dir),
        path: dir.to_path_buf(),
        kind,
        git_dir,
    });
}

/// Is `dir` a repository, and if so of what kind and with which git dir?
fn classify(dir: &Path) -> Option<(RepoKind, PathBuf)> {
    let dot_git = dir.join(".git");
    match std::fs::symlink_metadata(&dot_git) {
        Ok(md) if md.is_dir() => return Some((RepoKind::Normal, dot_git)),
        Ok(md) if md.is_file() => return classify_git_file(dir, &dot_git),
        _ => {}
    }
    classify_bare(dir)
}

/// `.git` is a file containing `gitdir: <path>` — a linked worktree or a
/// submodule. Which one is decided by where that path points.
fn classify_git_file(dir: &Path, dot_git: &Path) -> Option<(RepoKind, PathBuf)> {
    let contents = std::fs::read_to_string(dot_git).ok()?;
    let target = contents.lines().find_map(|l| l.trim().strip_prefix("gitdir:")).map(str::trim)?;
    if target.is_empty() {
        return None;
    }
    // Submodules use a relative gitdir; linked worktrees an absolute one.
    let raw = Path::new(target);
    let resolved = if raw.is_absolute() { raw.to_path_buf() } else { dir.join(raw) };
    let git_dir = resolved.canonicalize().unwrap_or(resolved);

    // The owning repository is the parent of the `.git` directory the path runs
    // through. Both `.../main/.git/worktrees/wt` and
    // `.../super/.git/modules/sub` answer to the same rule.
    let owner = owner_of(&git_dir);
    let s = git_dir.to_string_lossy();
    let kind = if s.contains("/worktrees/") {
        match owner.as_deref().and_then(|p| RepoId::new(p).ok()) {
            Some(main) => RepoKind::Worktree { main },
            // A worktree whose main repository has been deleted is still a
            // repository, and reporting it as normal beats dropping it.
            None => RepoKind::Normal,
        }
    } else if s.contains("/modules/") {
        match owner.as_deref().and_then(|p| RepoId::new(p).ok()) {
            Some(parent) => RepoKind::Submodule { parent },
            None => RepoKind::Normal,
        }
    } else {
        RepoKind::Normal
    };
    Some((kind, git_dir))
}

/// The worktree that owns a `.git` directory appearing inside `git_dir`.
///
/// Known simplification: for a submodule nested inside a submodule this returns
/// the outermost superproject rather than the immediate parent, because the
/// immediate parent's worktree location is not recoverable from the path alone.
/// Nothing reads the linkage, so the cost is a slightly wrong label.
fn owner_of(git_dir: &Path) -> Option<PathBuf> {
    let mut acc = PathBuf::new();
    for comp in git_dir.components() {
        if comp.as_os_str() == ".git" {
            return Some(acc);
        }
        acc.push(comp);
    }
    None
}

/// A bare repository: `HEAD`, `objects/` and `refs/` with no `.git`.
///
/// `HEAD` is checked first and alone, so the common case — an ordinary
/// directory — costs exactly one extra stat per directory walked.
fn classify_bare(dir: &Path) -> Option<(RepoKind, PathBuf)> {
    if !dir.join("HEAD").is_file() {
        return None;
    }
    if dir.join("objects").is_dir() && dir.join("refs").is_dir() {
        return Some((RepoKind::Bare, dir.to_path_buf()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn collect(root: &Path, opts: WalkOptions) -> Vec<RepoFound> {
        let (found, errors) = collect_with_errors(root, opts);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        found
    }

    fn collect_with_errors(
        root: &Path,
        opts: WalkOptions,
    ) -> (Vec<RepoFound>, Vec<DiscoveryError>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (_n, errors) = Walker::new(vec![root.to_path_buf()]).options(opts).walk(tx);
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            out.push(f);
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        (out, errors)
    }

    fn mk_normal(at: &Path) {
        fs::create_dir_all(at.join(".git")).unwrap();
    }

    #[test]
    fn finds_normal_repos_and_prunes_their_subtrees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        mk_normal(&root.join("a"));
        mk_normal(&root.join("b/c"));
        fs::create_dir_all(root.join("a/src/deep/deeper")).unwrap();
        // A nested repository inside another repository's worktree.
        mk_normal(&root.join("a/vendor/inner"));

        let found = collect(root, WalkOptions::default());
        let paths: Vec<_> = found.iter().map(|f| f.path.strip_prefix(root).unwrap()).collect();
        assert_eq!(paths, vec![Path::new("a"), Path::new("b/c")]);
        assert!(found.iter().all(|f| f.kind == RepoKind::Normal));
    }

    #[test]
    fn a_root_that_is_itself_a_repository_is_found() {
        // Regression: the walk used to return early at depth 0 without
        // classifying, so `git-scylla scan ~/one-repo` answered "no
        // repositories found" — the same sentence an empty directory gets, and
        // in the GUI an empty grid with no hint as to why.
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        mk_normal(root);
        fs::create_dir_all(root.join("src/deep")).unwrap();
        // Still pruned: a repository named as a root is one repository, not one
        // plus whatever is checked out inside it.
        mk_normal(&root.join("vendor/inner"));

        let found = collect(root, WalkOptions::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, *root);
        assert_eq!(found[0].kind, RepoKind::Normal);
    }

    #[test]
    fn a_bare_repository_named_as_a_root_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().canonicalize().unwrap().join("thing.git");
        fs::create_dir_all(bare.join("objects")).unwrap();
        fs::create_dir_all(bare.join("refs")).unwrap();
        fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let found = collect(&bare, WalkOptions::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, RepoKind::Bare);
    }

    #[test]
    fn a_git_directory_named_as_a_root_is_still_not_a_repository() {
        // `.git` has HEAD, `objects/` and `refs/`, so the bare test matches it
        // exactly. Pointing at one explicitly does not make it a repository.
        let tmp = tempfile::tempdir().unwrap();
        let dot_git = tmp.path().canonicalize().unwrap().join("r/.git");
        fs::create_dir_all(dot_git.join("objects")).unwrap();
        fs::create_dir_all(dot_git.join("refs")).unwrap();
        fs::write(dot_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        assert!(collect(&dot_git, WalkOptions::default()).is_empty());
    }

    #[test]
    fn the_same_root_twice_yields_one_repository() {
        // Two roots resolving to one repository must not emit it twice: the
        // engine counts what it accepts, and a double count leaves a scan
        // reporting more repositories than exist.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        mk_normal(&root);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (n, errors) = Walker::new(vec![root.clone(), root.clone()]).walk(tx);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(n, 1);
        let mut found = Vec::new();
        while let Ok(f) = rx.try_recv() {
            found.push(f);
        }
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn nested_finds_the_inner_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        mk_normal(&root.join("a"));
        mk_normal(&root.join("a/vendor/inner"));

        let found = collect(root, WalkOptions { nested: true, max_depth: None });
        let paths: Vec<_> = found.iter().map(|f| f.path.strip_prefix(root).unwrap()).collect();
        assert_eq!(paths, vec![Path::new("a"), Path::new("a/vendor/inner")]);
    }

    #[test]
    fn the_reported_id_is_canonical_without_a_second_syscall() {
        // The property the engine's scan accounting rests on: the id a consumer
        // gets is the one `RepoId::new` would have produced, so nothing has to
        // canonicalize again and risk a different answer.
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        mk_normal(&root.join("a"));
        let found = collect(root, WalkOptions::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, RepoId::new(&found[0].path).unwrap());
        assert_eq!(found[0].id.path(), found[0].path);
    }

    #[test]
    fn detects_bare_repositories() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("thing.git");
        fs::create_dir_all(bare.join("objects")).unwrap();
        fs::create_dir_all(bare.join("refs")).unwrap();
        fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let found = collect(tmp.path(), WalkOptions::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, RepoKind::Bare);
        assert_eq!(found[0].git_dir, found[0].path);
    }

    #[test]
    fn a_head_file_alone_is_not_a_repository() {
        // A directory containing a file called HEAD is common enough (docs,
        // test fixtures) that the objects/refs check has to be load-bearing.
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("notarepo")).unwrap();
        fs::write(tmp.path().join("notarepo/HEAD"), "hello").unwrap();
        assert!(collect(tmp.path(), WalkOptions::default()).is_empty());
    }

    #[test]
    fn classifies_a_git_file_as_worktree_or_submodule() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let main = root.join("main");
        fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        let wt = root.join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .unwrap();

        let sup = root.join("super");
        fs::create_dir_all(sup.join(".git/modules/sub")).unwrap();
        let sub = sup.join("sub");
        fs::create_dir_all(&sub).unwrap();
        // Submodules use a relative gitdir, which must resolve correctly.
        fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();

        let found = collect(&root, WalkOptions { nested: true, max_depth: None });
        let by_name =
            |n: &str| found.iter().find(|f| f.path.file_name().unwrap() == n).unwrap().kind.clone();
        assert_eq!(by_name("wt"), RepoKind::Worktree { main: RepoId::new(&main).unwrap() });
        assert_eq!(by_name("sub"), RepoKind::Submodule { parent: RepoId::new(&sup).unwrap() });
    }

    #[test]
    fn a_git_directory_is_not_reported_as_a_bare_repo() {
        // Regression: `.git` has HEAD, objects/ and refs/, so the bare test
        // matches it and `--nested` would report every repository twice — plus
        // once more per submodule, from `.git/modules/<name>`.
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        let repo = root.join("r");
        fs::create_dir_all(repo.join(".git/objects")).unwrap();
        fs::create_dir_all(repo.join(".git/refs")).unwrap();
        fs::create_dir_all(repo.join(".git/modules/sub/objects")).unwrap();
        fs::create_dir_all(repo.join(".git/modules/sub/refs")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(repo.join(".git/modules/sub/HEAD"), "ref: refs/heads/main\n").unwrap();

        for nested in [false, true] {
            let found = collect(root, WalkOptions { nested, max_depth: None });
            let paths: Vec<_> = found.iter().map(|f| f.path.strip_prefix(root).unwrap()).collect();
            assert_eq!(paths, vec![Path::new("r")], "nested={nested}");
            assert_eq!(found[0].kind, RepoKind::Normal);
        }
    }

    #[test]
    fn skips_build_directories() {
        let tmp = tempfile::tempdir().unwrap();
        mk_normal(&tmp.path().join("node_modules/pkg"));
        mk_normal(&tmp.path().join("target/vendor"));
        mk_normal(&tmp.path().join("real"));
        let found = collect(tmp.path(), WalkOptions::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path.file_name().unwrap(), "real");
    }

    #[test]
    fn a_symlink_loop_terminates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        mk_normal(&root.join("a"));
        std::os::unix::fs::symlink(root.as_path(), root.join("loop")).unwrap();
        // The assertion is that this returns at all.
        let found = collect(root, WalkOptions::default());
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn an_unreadable_directory_does_not_abort_the_walk() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        mk_normal(&locked.join("hidden"));
        mk_normal(&root.join("visible"));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let (found, errors) = collect_with_errors(root, WalkOptions::default());
        // Restore before the tempdir drop, or cleanup fails.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(found.iter().any(|f| f.path.file_name().unwrap() == "visible"));
        // ...and the caller is told what could not be read. On macOS this is
        // what TCC looks like, and a scan that reports nothing wrong while
        // silently seeing nothing is the failure the hint exists to prevent.
        assert!(
            errors.iter().any(
                |e| matches!(e, DiscoveryError::Unreadable { path, .. } if path.ends_with("locked"))
            ),
            "{errors:?}"
        );
    }

    #[test]
    fn a_flood_of_unreadable_directories_collapses_to_a_count() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        for i in 0..30 {
            let d = root.join(format!("locked{i}"));
            fs::create_dir_all(d.join("inner")).unwrap();
            fs::set_permissions(&d, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let (_, errors) = collect_with_errors(root, WalkOptions::default());
        for i in 0..30 {
            let _ = fs::set_permissions(
                root.join(format!("locked{i}")),
                fs::Permissions::from_mode(0o755),
            );
        }

        let listed =
            errors.iter().filter(|e| matches!(e, DiscoveryError::Unreadable { .. })).count();
        assert_eq!(listed, MAX_UNREADABLE, "individual reports are capped");
        let more: usize = errors
            .iter()
            .filter_map(|e| match e {
                DiscoveryError::MoreUnreadable(n) => Some(*n),
                _ => None,
            })
            .sum();
        assert_eq!(listed + more, 30, "every one is still accounted for");
    }

    #[test]
    fn a_cancelled_walk_stops_early() {
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        for i in 0..40 {
            mk_normal(&root.join(format!("r{i}")));
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let walker = Walker::new(vec![root.to_path_buf()]);
        // Cancelled before it starts, so nothing below the roots is visited.
        walker.cancel_flag().store(true, Ordering::Relaxed);
        let (n, fatal) = walker.walk(tx);
        assert!(fatal.is_empty());
        assert_eq!(n, 0, "a cancelled walk finds nothing");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_missing_root_is_reported_not_ignored() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (n, fatal) = Walker::new(vec![PathBuf::from("/nope/nope/nope")]).walk(tx);
        assert_eq!(n, 0);
        assert_eq!(fatal.len(), 1);
    }
}
