//! FSEvents → repository invalidations.
//!
//! The watcher's job is to say *which repository moved*, and nothing else. It
//! never probes, never reads a snapshot and never decides whether a re-probe is
//! allowed — the engine owns all three, and a watcher with a second opinion
//! about engine state would be a second scheduler.
//!
//! Three layers, and the two that carry the rules are pure:
//!
//! * [`index`] answers "which repository does this path belong to" — a prefix
//!   question, hence a sorted `Vec` and a binary search.
//! * [`classify`] answers "does this path mean anything" — the rule that keeps
//!   a fetch's thousands of loose objects from becoming a probe storm.
//! * [`Pending`] accumulates a debounce window, so a save that touches forty
//!   files is one invalidation.
//!
//! Only the `notify` wiring at the bottom needs a real filesystem, and it is
//! deliberately thin for that reason.

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
///
/// One editor save touches the file, a swap file and often an atomic-rename
/// temporary; one `git commit` touches the index, two refs and a reflog. None
/// of those deserves its own probe, and 300 ms is below the threshold at which
/// a person reads the grid as lagging.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// What the watcher asks the engine to do.
///
/// Never a snapshot: the watcher does not probe. Every variant is a request the
/// engine is free to refuse — it holds the busy marker, and a repository with a
/// job in flight must not be re-probed underneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalidation {
    /// Re-probe these. Batched, because one debounce window usually covers one
    /// repository but a `git pull` across a working set covers many.
    Repos(Vec<RepoId>),
    /// A `.git` appeared where no repository was known. Discover this subtree
    /// rather than rescanning every root — cloning one repository should not
    /// cost a walk of all of them.
    Discover(PathBuf),
    /// These are gone from disk.
    Gone(Vec<RepoId>),
    /// The backend lost history and cannot say what changed. Nothing held
    /// locally can be trusted, so the only honest response is a full rescan.
    Rescan,
}

/// One filesystem change, reduced to what this crate reasons about.
///
/// Its own type rather than `notify::Event` so that the fold below is testable
/// without producing real filesystem events, and so that a backend that reports
/// a kind this crate does not distinguish cannot leak that distinction inward.
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
///
/// Sets, so that forty events in one repository are one invalidation and the
/// order they arrived in cannot leak into the order they are reported.
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

    /// A backend that lost history. Everything else gathered is moot.
    pub fn must_rescan(&mut self) {
        *self = Self { rescan: true, ..Default::default() };
    }

    /// Fold one change into the window.
    ///
    /// `exists` is a parameter rather than a call to [`Path::exists`] so the
    /// disappearance rules stay a pure function of their inputs — the
    /// alternative is a test that creates and deletes real directories to
    /// assert a classification.
    pub fn absorb(&mut self, index: &Index, obs: &Observed, exists: &dyn Fn(&Path) -> bool) {
        if self.rescan {
            return;
        }

        // Gone first: a path that no longer exists cannot be classified by what
        // is inside it, and `rm -rf` of a directory holding four checkouts is
        // one event that takes all four.
        let vanished = vanished(index, &obs.path, exists);
        if !vanished.is_empty() {
            self.gone.extend(vanished);
            return;
        }

        let Some(owner) = index.owner(&obs.path) else {
            // Under a root but inside no known repository. Only a `.git` is
            // worth a discovery pass; anything else is somebody saving a note
            // in a directory that is not a checkout.
            if let Some(root) = classify::repository_appearing(&obs.path) {
                self.discover.insert(root);
            }
            return;
        };

        let Ok(rel) = obs.path.strip_prefix(&owner.path) else {
            // `owner` returned it, so it is a prefix. Unreachable.
            return;
        };
        if classify::verdict(rel, owner.bare, obs.change) == Verdict::Reprobe {
            self.repos.insert(owner.id.clone());
        }
    }

    /// Everything gathered, as the messages to send. Leaves the window empty.
    ///
    /// `Gone` precedes `Repos` so the engine drops a repository before being
    /// asked to re-probe it; `Rescan` replaces everything, since a backend that
    /// lost history has made the rest of the window meaningless.
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
///
/// Two shapes, and the second is easy to miss: a directory holding checkouts
/// removed wholesale, and a repository whose `.git` was removed while the
/// directory stayed. The second is no longer a repository even though its path
/// still exists.
fn vanished(index: &Index, path: &Path, exists: &dyn Fn(&Path) -> bool) -> Vec<RepoId> {
    let at_or_below: Vec<RepoId> = index.under(path).map(|w| w.id.clone()).collect();
    if !at_or_below.is_empty() {
        // The existence check is what makes this robust to a backend that
        // coalesced the removal into an event with no kind — FSEvents does,
        // routinely — rather than trusting `Change::Removed` to have survived.
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

// ---- the notify wiring -------------------------------------------------

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
    /// Recursive is not a cost decision to revisit: FSEvents is per-volume and
    /// kernel-level, so a large tree is essentially free — unlike kqueue, which
    /// would need a descriptor per file.
    ///
    /// The index starts empty, which means every event is unattributable until
    /// [`Watcher::reindex`] is called on the first settled scan. That is the
    /// right default: attributing paths to repositories the engine has not
    /// finished discovering would ask it to re-probe rows it does not have.
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
            // A window that fills while the engine is busy must not then fire
            // its backlog of ticks all at once.
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
    ///
    /// Called when a scan settles. Rebuilt rather than mutated, because the set
    /// it mirrors is rebuilt then too and an incrementally maintained copy
    /// would be a second source of truth about which repositories exist.
    pub fn reindex(&self, repos: impl IntoIterator<Item = Watched>) {
        self.index_handle().replace(repos);
    }

    /// A handle to the index, detached from the watcher.
    ///
    /// Filling the index means asking the engine what it holds, which is
    /// asynchronous — and a caller that keeps its watcher behind a
    /// `std::sync::Mutex` must not hold that lock across the await. This is
    /// what it holds instead.
    pub fn index_handle(&self) -> IndexHandle {
        IndexHandle(Arc::clone(&self.index))
    }
}

fn absorb_notify(pending: &mut Pending, index: &Mutex<Index>, res: notify::Result<notify::Event>) {
    let event = match res {
        Ok(e) => e,
        Err(e) => {
            // A backend error is not something to swallow: the most likely one
            // is a lost-history notification arriving as an error, and the
            // honest response to "I cannot tell you what changed" is a rescan.
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
        // FSEvents coalesces, so `Any` is common and means exactly what it
        // says. Every rule but `index.lock` treats it the same as a touch.
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

    /// Everything exists. The common case, and the one where disappearance
    /// rules must stay out of the way.
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
        // An editor save touches the file, a swap file and a rename temporary;
        // a commit touches the index, two refs and a reflog. One probe.
        let mut p = Pending::default();
        absorb(
            &mut p,
            &(0..40).map(|i| touched(&format!("/work/api/src/f{i}.rs"))).collect::<Vec<_>>(),
        );
        assert_eq!(p.drain(), [Invalidation::Repos(vec![RepoId::from_canonical("/work/api")])]);
    }

    #[test]
    fn a_fetch_writing_thousands_of_objects_produces_nothing() {
        // The failure the watcher is most likely to cause.
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
        // Cloning a repository should make it appear without a walk of every
        // root.
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
        // Still a directory. No longer a repository.
        let mut p = Pending::default();
        p.absorb(&index(), &Observed::new("/work/api/.git", Change::Removed), &|path| {
            path != Path::new("/work/api/.git")
        });
        assert_eq!(p.drain(), [Invalidation::Gone(vec![RepoId::from_canonical("/work/api")])]);
    }

    #[test]
    fn disappearance_rests_on_the_path_being_gone_rather_than_on_the_event_kind() {
        // FSEvents coalesces, so a removal routinely arrives with no kind at
        // all. Trusting `Change::Removed` to have survived would miss it.
        let mut p = Pending::default();
        p.absorb(&index(), &Observed::new("/work/api", Change::Unknown), &|_| false);
        assert_eq!(p.drain(), [Invalidation::Gone(vec![RepoId::from_canonical("/work/api")])]);

        // ...and the converse: a `Removed` for a file inside a repository that
        // still exists is an ordinary re-probe, not a disappearance.
        let mut p = Pending::default();
        p.absorb(&index(), &Observed::new("/work/api/a.txt", Change::Removed), &present);
        assert_eq!(p.drain(), [Invalidation::Repos(vec![RepoId::from_canonical("/work/api")])]);
    }

    #[test]
    fn a_lost_history_notification_replaces_everything_in_the_window() {
        // Nothing gathered before it can be trusted, and nothing gathered after
        // it adds anything: the rescan already covers the working set.
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
        // Ordering the engine depends on: asking it to re-probe a row it is
        // about to drop is work it cannot use.
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
    //! The `notify` wiring, against a real filesystem.
    //!
    //! Everything above is a pure function and is tested as one. This is the
    //! one part that can only be exercised by making real changes on a real
    //! volume, and it is thin precisely so that there is little here to be
    //! wrong — but "little" is not "nothing", and untested it would be the
    //! layer where a backend quirk hides.

    use super::*;

    /// FSEvents coalesces and is not instant. Generous, because what is being
    /// asserted is "the event arrives", not how fast.
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
        // Cloning a repository has to make it appear without a full rescan.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        // The index is empty, which is the state a watcher is in before its
        // first scan settles — and the state in which a `.git` appearing is the
        // only thing worth reporting.
        let _watcher =
            Watcher::start(std::slice::from_ref(&root), tx, Duration::from_millis(100)).unwrap();

        std::fs::create_dir_all(root.join("fresh/.git")).unwrap();
        std::fs::write(root.join("fresh/.git/HEAD"), "ref: refs/heads/main\n").unwrap();

        // Other events may arrive first — the directory itself, for one — so
        // this waits for the one it is about rather than assuming it is first.
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
        // The sender is dropped with the watcher, so the channel closes rather
        // than merely going quiet — which is the difference between a stopped
        // watcher and a slow one.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(matches!(tokio::time::timeout(Duration::from_secs(2), rx.recv()).await, Ok(None)));
    }
}
