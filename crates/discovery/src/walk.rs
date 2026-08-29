use crate::skip::{is_git_dir, is_hard_skipped, looks_dataless};
use git_scylla_core::{RepoId, RepoKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFound {
    pub id: RepoId,
    pub path: PathBuf,
    pub kind: RepoKind,
    pub per_worktree_dir: PathBuf,
}

/// Something the walk could not read. On macOS this is almost always TCC: an
/// unsigned app scanning `~/Documents` gets permission denied per directory,
/// indistinguishable from an empty tree unless reported.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "type", content = "value")]
pub enum DiscoveryError {
    #[error("root {0} does not exist or is not readable")]
    UnusableRoot(PathBuf),
    #[error("could not read {path}: {reason}")]
    Unreadable { path: PathBuf, reason: String },
    #[error("and {0} more unreadable")]
    MoreUnreadable(usize),
}

const MAX_UNREADABLE: usize = 20;

#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
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

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

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
        usable.sort();
        usable.dedup();

        let found = Arc::new(Mutex::new(Found::default()));
        let count = Arc::new(Mutex::new(0usize));

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
            .standard_filters(false)
            .hidden(false)
            .parents(false)
            .follow_links(false)
            .max_depth(self.opts.max_depth)
            .threads(1);

        let roots_for_skip = usable.clone();
        let nested = self.opts.nested;
        let stop = Arc::clone(&self.stop);
        let f = Arc::clone(&found);
        let c = Arc::clone(&count);
        let sender = tx.clone();

        builder.filter_entry(move |entry| {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let path = entry.path();
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
            true
        });

        let mut unreadable = 0usize;
        for result in builder.build() {
            if let Err(err) = result {
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

fn excluded(dir: &Path, roots: &[PathBuf]) -> bool {
    is_git_dir(dir) || looks_dataless(dir) || is_hard_skipped(dir, roots)
}

fn covered(found: &Mutex<Found>, dir: &Path) -> bool {
    found.lock().expect("discovery state poisoned").roots.iter().any(|r| dir.starts_with(r))
}

fn accept(dir: &Path, found: &Mutex<Found>, count: &Mutex<usize>, tx: &UnboundedSender<RepoFound>) {
    let Some((kind, per_worktree_dir)) = classify(dir) else { return };
    found.lock().expect("discovery state poisoned").roots.push(dir.to_path_buf());
    *count.lock().expect("discovery state poisoned") += 1;
    let _ = tx.send(RepoFound {
        id: RepoId::from_canonical(dir),
        path: dir.to_path_buf(),
        kind,
        per_worktree_dir,
    });
}

fn classify(dir: &Path) -> Option<(RepoKind, PathBuf)> {
    let dot_git = dir.join(".git");
    match std::fs::symlink_metadata(&dot_git) {
        Ok(md) if md.is_dir() => return Some((RepoKind::Normal, dot_git)),
        Ok(md) if md.is_file() => return classify_git_file(dir, &dot_git),
        _ => {}
    }
    classify_bare(dir)
}

fn classify_git_file(dir: &Path, dot_git: &Path) -> Option<(RepoKind, PathBuf)> {
    let contents = std::fs::read_to_string(dot_git).ok()?;
    let target = contents.lines().find_map(|l| l.trim().strip_prefix("gitdir:")).map(str::trim)?;
    if target.is_empty() {
        return None;
    }
    let raw = Path::new(target);
    let resolved = if raw.is_absolute() { raw.to_path_buf() } else { dir.join(raw) };
    let per_worktree_dir = resolved.canonicalize().unwrap_or(resolved);

    let owner = owner_of(&per_worktree_dir);
    let s = per_worktree_dir.to_string_lossy();
    let kind = if s.contains("/worktrees/") {
        match owner.as_deref().and_then(|p| RepoId::new(p).ok()) {
            Some(main) => RepoKind::Worktree { main },
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
    Some((kind, per_worktree_dir))
}

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
        mk_normal(&root.join("a/vendor/inner"));

        let found = collect(root, WalkOptions::default());
        let paths: Vec<_> = found.iter().map(|f| f.path.strip_prefix(root).unwrap()).collect();
        assert_eq!(paths, vec![Path::new("a"), Path::new("b/c")]);
        assert!(found.iter().all(|f| f.kind == RepoKind::Normal));
    }

    #[test]
    fn a_root_that_is_itself_a_repository_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        let root = &tmp.path().canonicalize().unwrap();
        mk_normal(root);
        fs::create_dir_all(root.join("src/deep")).unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let dot_git = tmp.path().canonicalize().unwrap().join("r/.git");
        fs::create_dir_all(dot_git.join("objects")).unwrap();
        fs::create_dir_all(dot_git.join("refs")).unwrap();
        fs::write(dot_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        assert!(collect(&dot_git, WalkOptions::default()).is_empty());
    }

    #[test]
    fn the_same_root_twice_yields_one_repository() {
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
        assert_eq!(found[0].per_worktree_dir, found[0].path);
    }

    #[test]
    fn a_head_file_alone_is_not_a_repository() {
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
        fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();

        let found = collect(&root, WalkOptions { nested: true, max_depth: None });
        let by_name =
            |n: &str| found.iter().find(|f| f.path.file_name().unwrap() == n).unwrap().kind.clone();
        assert_eq!(by_name("wt"), RepoKind::Worktree { main: RepoId::new(&main).unwrap() });
        assert_eq!(by_name("sub"), RepoKind::Submodule { parent: RepoId::new(&sup).unwrap() });
    }

    #[test]
    fn a_git_directory_is_not_reported_as_a_bare_repo() {
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
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(found.iter().any(|f| f.path.file_name().unwrap() == "visible"));
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
