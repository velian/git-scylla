//! Turns filesystem events into repository invalidations.
//!
//! [`index::Index`] maps a changed path to a repository. [`classify`] judges
//! whether a path matters. [`Pending`] accumulates a debounce window into
//! [`Invalidation`] messages.

pub mod classify;
pub mod index;

pub use classify::{Change, Verdict};
pub use index::{Index, Watched};

use git_scylla_core::RepoId;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long changes are gathered before being reported.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// What the watcher asks the engine to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalidation {
    /// Re-probe these repositories.
    Repos(Vec<RepoId>),
    /// A `.git` appeared with no known owner.
    Discover(PathBuf),
    /// These are gone from disk.
    Gone(Vec<RepoId>),
    /// The backend lost history. Rescan everything.
    Rescan,
}

/// One filesystem change, reduced to what this crate reasons about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub path: PathBuf,
    pub change: Change,
}

impl Observed {
    pub fn new(path: impl Into<PathBuf>, change: Change) -> Self {
        Self { path: path.into(), change }
    }
}

/// What one debounce window has gathered.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pending {
    repos: BTreeSet<RepoId>,
    discover: BTreeSet<PathBuf>,
    gone: BTreeSet<RepoId>,
    rescan: bool,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        !self.rescan && self.repos.is_empty() && self.discover.is_empty() && self.gone.is_empty()
    }

    /// A backend that lost history.
    pub fn must_rescan(&mut self) {
        *self = Self { rescan: true, ..Default::default() };
    }

    /// Fold one change into the window.
    ///
    /// `exists` is injected so this stays pure and testable.
    pub fn absorb(&mut self, index: &Index, obs: &Observed, exists: &dyn Fn(&Path) -> bool) {
        if self.rescan {
            return;
        }

        let vanished = vanished(index, &obs.path, exists);
        if !vanished.is_empty() {
            self.gone.extend(vanished);
            return;
        }

        let Some(owner) = index.owner(&obs.path) else {
            if let Some(root) = classify::repository_appearing(&obs.path) {
                self.discover.insert(root);
            }
            return;
        };

        let Ok(rel) = obs.path.strip_prefix(&owner.path) else {
            // `owner` returned it, so it is a prefix.
            return;
        };
        if classify::verdict(rel, owner.bare, obs.change) == Verdict::Reprobe {
            self.repos.insert(owner.id.clone());
        }
    }

    /// Everything gathered, as messages to send. Leaves the window empty.
    pub fn drain(&mut self) -> Vec<Invalidation> {
        let taken = std::mem::take(self);
        if taken.rescan {
            return vec![Invalidation::Rescan];
        }
        let mut out = Vec::new();
        if !taken.gone.is_empty() {
            out.push(Invalidation::Gone(taken.gone.into_iter().collect()));
        }
        out.extend(taken.discover.into_iter().map(Invalidation::Discover));
        if !taken.repos.is_empty() {
            out.push(Invalidation::Repos(taken.repos.into_iter().collect()));
        }
        out
    }
}

/// Which repositories `path` going away would take with it.
fn vanished(index: &Index, path: &Path, exists: &dyn Fn(&Path) -> bool) -> Vec<RepoId> {
    let at_or_below: Vec<RepoId> = index.under(path).map(|w| w.id.clone()).collect();
    if !at_or_below.is_empty() {
        // Robust to backends that omit the removal's event kind.
        return if exists(path) { Vec::new() } else { at_or_below };
    }
    match index.owner(path) {
        Some(w) if is_git_dir_of(w, path) && !exists(path) => vec![w.id.clone()],
        _ => Vec::new(),
    }
}

/// Is `path` the git directory that makes `w` a repository?
fn is_git_dir_of(w: &Watched, path: &Path) -> bool {
    !w.bare && path.strip_prefix(&w.path).is_ok_and(|rel| rel == Path::new(".git"))
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("could not start watching {path}: {source}")]
    Start { path: PathBuf, source: notify::Error },
}

/// The index a [`Watcher`] consults, refillable without holding the watcher.
#[derive(Clone)]
pub struct IndexHandle(Arc<Mutex<Index>>);

impl IndexHandle {
    pub fn replace(&self, repos: impl IntoIterator<Item = Watched>) {
        *self.0.lock().expect("watch index poisoned") = Index::new(repos);
    }
}

/// A running watcher. Dropping it stops the subscription and the debounce task.
pub struct Watcher {
    /// Held, not used: `notify` stops when its watcher is dropped.
    _backend: notify::RecommendedWatcher,
    index: Arc<Mutex<Index>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Watcher {
    /// Watch every root recursively, reporting to `tx`.
    ///
    /// FSEvents is per-volume and kernel-level; recursive watching is
    /// inexpensive regardless of tree size.
    pub fn start(
        roots: &[PathBuf],
        tx: tokio::sync::mpsc::Sender<Invalidation>,
        debounce: Duration,
    ) -> Result<Self, WatchError> {
        use notify::Watcher as _;

        let index = Arc::new(Mutex::new(Index::default()));
        let (raw_tx, mut raw_rx) =
            tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();

        let mut backend = notify::recommended_watcher(move |res| {
            // The receiver is gone only once the watcher has been dropped.
            let _ = raw_tx.send(res);
        })
        .map_err(|source| WatchError::Start { path: PathBuf::new(), source })?;

        for root in roots {
            backend
                .watch(root, notify::RecursiveMode::Recursive)
                .map_err(|source| WatchError::Start { path: root.clone(), source })?;
        }

        let task_index = Arc::clone(&index);
        let task = tokio::spawn(async move {
            let mut pending = Pending::default();
            let mut ticker = tokio::time::interval(debounce);
            // A window that fills while the engine is busy must not fire its
            // backlog of ticks all at once.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    received = raw_rx.recv() => match received {
                        Some(res) => absorb_notify(&mut pending, &task_index, res),
                        None => break,
                    },
                    _ = ticker.tick() => {
                        if pending.is_empty() {
                            continue;
                        }
                        for message in pending.drain() {
                            if tx.send(message).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self { _backend: backend, index, task })
    }

    /// Replace the path → repository index.
    pub fn reindex(&self, repos: impl IntoIterator<Item = Watched>) {
        self.index_handle().replace(repos);
    }

    /// A handle to the index, detached from the watcher.
    pub fn index_handle(&self) -> IndexHandle {
        IndexHandle(Arc::clone(&self.index))
    }
}

fn absorb_notify(pending: &mut Pending, index: &Mutex<Index>, res: notify::Result<notify::Event>) {
    let event = match res {
        Ok(e) => e,
        Err(e) => {
            // A backend error most likely means lost history; rescan.
            tracing::warn!(%e, "watch backend error; rescanning");
            pending.must_rescan();
            return;
        }
    };
    if event.need_rescan() {
        tracing::warn!("the watch backend lost history; rescanning");
        pending.must_rescan();
        return;
    }
    let change = change_of(&event.kind);
    let index = index.lock().expect("watch index poisoned");
    for path in &event.paths {
        pending.absorb(&index, &Observed::new(path.clone(), change), &|p| p.exists());
    }
}

fn change_of(kind: &notify::EventKind) -> Change {
    use notify::EventKind;
    match kind {
        EventKind::Create(_) | EventKind::Modify(_) => Change::Touched,
        EventKind::Remove(_) => Change::Removed,
        // FSEvents commonly reports `Any`; treated as `Unknown`.
        EventKind::Any | EventKind::Access(_) | EventKind::Other => Change::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watched(path: &str, bare: bool) -> Watched {
        Watched { id: RepoId::from_canonical(path), path: path.into(), bare }
    }

    fn index() -> Index {
        Index::new([watched("/work/api", false), watched("/work/web", false)])
    }

    fn present(_: &Path) -> bool {
        true
    }

    fn absorb(pending: &mut Pending, obs: &[Observed]) {
        for o in obs {
            pending.absorb(&index(), o, &present);
        }
    }

    fn touched(path: &str) -> Observed {
        Observed::new(path, Change::Touched)
    }

    #[test]
    fn a_window_of_forty_events_in_one_repository_is_one_invalidation() {
        let mut p = Pending::default();
        absorb(
            &mut p,
            &(0..40).map(|i| touched(&format!("/work/api/src/f{i}.rs"))).collect::<Vec<_>>(),
        );
        assert_eq!(p.drain(), [Invalidation::Repos(vec![RepoId::from_canonical("/work/api")])]);
    }

    #[test]
    fn a_fetch_writing_thousands_of_objects_produces_nothing() {
        let mut p = Pending::default();
        absorb(
            &mut p,
            &(0..2000)
                .map(|i| touched(&format!("/work/api/.git/objects/ab/{i:04}")))
                .collect::<Vec<_>>(),
        );
        assert!(p.is_empty());
        assert_eq!(p.drain(), []);
    }

    #[test]
    fn the_ref_write_at_the_end_of_that_fetch_is_the_one_that_counts() {
        let mut p = Pending::default();
        absorb(
            &mut p,
            &[
                touched("/work/api/.git/objects/ab/cdef"),
                touched("/work/api/.git/refs/remotes/origin/main"),
                touched("/work/api/.git/FETCH_HEAD"),
            ],
        );
        assert_eq!(p.drain(), [Invalidation::Repos(vec![RepoId::from_canonical("/work/api")])]);
    }

    #[test]
    fn several_repositories_are_reported_together() {
        let mut p = Pending::default();
        absorb(&mut p, &[touched("/work/web/a.txt"), touched("/work/api/b.txt")]);
        assert_eq!(
            p.drain(),
            [Invalidation::Repos(vec![
                RepoId::from_canonical("/work/api"),
                RepoId::from_canonical("/work/web"),
            ])]
        );
    }

    #[test]
    fn a_git_directory_appearing_asks_for_a_targeted_discovery() {
        let mut p = Pending::default();
        absorb(&mut p, &[touched("/work/fresh/.git/HEAD"), touched("/work/fresh/.git/config")]);
        assert_eq!(p.drain(), [Invalidation::Discover("/work/fresh".into())]);
    }

    #[test]
    fn a_file_outside_every_repository_asks_for_nothing() {
        let mut p = Pending::default();
        absorb(&mut p, &[touched("/work/notes.txt"), touched("/elsewhere/thing")]);
        assert!(p.is_empty());
    }

    #[test]
    fn a_removed_repository_is_reported_gone() {
        let mut p = Pending::default();
        p.absorb(&index(), &Observed::new("/work/api", Change::Removed), &|_| false);
        assert_eq!(p.drain(), [Invalidation::Gone(vec![RepoId::from_canonical("/work/api")])]);
    }

    #[test]
    fn removing_a_directory_of_checkouts_takes_all_of_them() {
        let mut p = Pending::default();
        p.absorb(&index(), &Observed::new("/work", Change::Removed), &|_| false);
        assert_eq!(
            p.drain(),
            [Invalidation::Gone(vec![
                RepoId::from_canonical("/work/api"),
                RepoId::from_canonical("/work/web"),
            ])]
        );
    }

    #[test]
    fn a_repository_whose_git_directory_went_is_gone_even_though_its_path_remains() {
        let mut p = Pending::default();
        p.absorb(&index(), &Observed::new("/work/api/.git", Change::Removed), &|path| {
            path != Path::new("/work/api/.git")
        });
        assert_eq!(p.drain(), [Invalidation::Gone(vec![RepoId::from_canonical("/work/api")])]);
    }

    #[test]
    fn disappearance_rests_on_the_path_being_gone_rather_than_on_the_event_kind() {
        let mut p = Pending::default();
        p.absorb(&index(), &Observed::new("/work/api", Change::Unknown), &|_| false);
        assert_eq!(p.drain(), [Invalidation::Gone(vec![RepoId::from_canonical("/work/api")])]);

        let mut p = Pending::default();
        p.absorb(&index(), &Observed::new("/work/api/a.txt", Change::Removed), &present);
        assert_eq!(p.drain(), [Invalidation::Repos(vec![RepoId::from_canonical("/work/api")])]);
    }

    #[test]
    fn a_lost_history_notification_replaces_everything_in_the_window() {
        let mut p = Pending::default();
        absorb(&mut p, &[touched("/work/api/a.txt")]);
        p.must_rescan();
        absorb(&mut p, &[touched("/work/web/b.txt")]);
        assert_eq!(p.drain(), [Invalidation::Rescan]);
    }

    #[test]
    fn a_drained_window_starts_empty_again() {
        let mut p = Pending::default();
        absorb(&mut p, &[touched("/work/api/a.txt")]);
        assert!(!p.drain().is_empty());
        assert!(p.is_empty());
        assert_eq!(p.drain(), []);
    }

    #[test]
    fn a_repository_is_dropped_before_it_is_re_probed() {
        let mut p = Pending::default();
        absorb(&mut p, &[touched("/work/web/a.txt")]);
        p.absorb(&index(), &Observed::new("/work/api", Change::Removed), &|_| false);
        let drained = p.drain();
        assert!(matches!(drained[0], Invalidation::Gone(_)), "{drained:?}");
        assert!(matches!(drained[1], Invalidation::Repos(_)), "{drained:?}");
    }

    #[test]
    fn a_bare_repository_is_watched_by_its_own_layout() {
        let index = Index::new([watched("/mirrors/thing.git", true)]);
        let mut p = Pending::default();
        p.absorb(&index, &touched("/mirrors/thing.git/objects/ab/cd"), &present);
        assert!(p.is_empty(), "a mirror's objects are no more news than a checkout's");
        p.absorb(&index, &touched("/mirrors/thing.git/refs/heads/main"), &present);
        assert_eq!(
            p.drain(),
            [Invalidation::Repos(vec![RepoId::from_canonical("/mirrors/thing.git")])]
        );
    }
}

#[cfg(test)]
mod backend {
    //! The `notify` wiring, tested against a real filesystem.

    use super::*;

    /// FSEvents is not instant; generous timeout.
    const PATIENCE: Duration = Duration::from_secs(20);

    async fn next(rx: &mut tokio::sync::mpsc::Receiver<Invalidation>) -> Option<Invalidation> {
        tokio::time::timeout(PATIENCE, rx.recv()).await.ok().flatten()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_written_in_a_watched_repository_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("api");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let watcher =
            Watcher::start(std::slice::from_ref(&root), tx, Duration::from_millis(100)).unwrap();
        watcher.reindex([Watched {
            id: RepoId::from_canonical(&repo),
            path: repo.clone(),
            bare: false,
        }]);

        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        assert_eq!(
            next(&mut rx).await,
            Some(Invalidation::Repos(vec![RepoId::from_canonical(&repo)]))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_repository_appearing_under_a_root_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let _watcher =
            Watcher::start(std::slice::from_ref(&root), tx, Duration::from_millis(100)).unwrap();

        std::fs::create_dir_all(root.join("fresh/.git")).unwrap();
        std::fs::write(root.join("fresh/.git/HEAD"), "ref: refs/heads/main\n").unwrap();

        // Other events may arrive first, so wait for the one this is about
        // rather than assuming it is first.
        let deadline = std::time::Instant::now() + PATIENCE;
        while std::time::Instant::now() < deadline {
            match next(&mut rx).await {
                Some(Invalidation::Discover(path)) => {
                    assert_eq!(path, root.join("fresh"));
                    return;
                }
                Some(_) => continue,
                None => break,
            }
        }
        panic!("a repository appearing under a watched root went unreported");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_the_watcher_stops_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("api");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let watcher =
            Watcher::start(std::slice::from_ref(&root), tx, Duration::from_millis(100)).unwrap();
        watcher.reindex([Watched {
            id: RepoId::from_canonical(&repo),
            path: repo.clone(),
            bare: false,
        }]);
        drop(watcher);

        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        // The sender drops with the watcher, so the channel closes rather
        // than merely going quiet.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(matches!(tokio::time::timeout(Duration::from_secs(2), rx.recv()).await, Ok(None)));
    }
}
