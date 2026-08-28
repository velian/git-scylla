//! The watcher against the engine.
//!
//! `crates/watch` unit-tests its own rules — which repository a path belongs
//! to, which paths matter, what a debounce window collapses to. What can only
//! be asserted here is the half the engine owns: that an invalidation is a
//! *request*, and that the busy marker refuses it.

use git_scylla_core::{Action, JobOrigin, JobState, RepoId, RepoSnapshot};
use git_scylla_engine::{Config, Engine, EngineHandle, Event, Plan, Policy, Selection};
use git_scylla_probe::{BoxFuture, GitCliProbe, Probe, ProbeRequest};
use git_scylla_watch::{Change, Index, Invalidation, Observed, Pending, Watched};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---- fixtures ----------------------------------------------------------

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "F")
        .env("GIT_AUTHOR_EMAIL", "f@example.invalid")
        .env("GIT_COMMITTER_NAME", "F")
        .env("GIT_COMMITTER_EMAIL", "f@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A bare origin with one commit, and `n` clones of it.
fn clones(dir: &Path, n: usize) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let repos = dir.join("repos");
    let scratch = dir.join("scratch");
    for p in [&repos, &scratch] {
        std::fs::create_dir_all(p).unwrap();
    }
    git(&dir, &["init", "--bare", "-b", "main", "origin.git"]);
    let origin = dir.join("origin.git");
    git(&scratch, &["clone", origin.to_str().unwrap(), "seed"]);
    let seed = scratch.join("seed");
    std::fs::write(seed.join("a.txt"), "one\n").unwrap();
    git(&seed, &["add", "a.txt"]);
    git(&seed, &["commit", "-m", "c1"]);
    git(&seed, &["push", "-u", "origin", "main"]);
    for i in 0..n {
        git(&repos, &["clone", origin.to_str().unwrap(), &format!("r{i:02}")]);
    }
    // Move origin forward and let each clone learn about it, so a pull has
    // work: `behind` is computed from the local tracking ref, which only
    // advances on fetch.
    std::fs::write(seed.join("a.txt"), "two\n").unwrap();
    git(&seed, &["commit", "-am", "c2"]);
    git(&seed, &["push", "origin", "main"]);
    for i in 0..n {
        git(&repos.join(format!("r{i:02}")), &["fetch"]);
    }
    repos
}

fn config() -> Config {
    Config {
        extra_env: vec![
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
            ("GIT_AUTHOR_NAME".into(), "F".into()),
            ("GIT_AUTHOR_EMAIL".into(), "f@example.invalid".into()),
            ("GIT_COMMITTER_NAME".into(), "F".into()),
            ("GIT_COMMITTER_EMAIL".into(), "f@example.invalid".into()),
        ],
        probe_timeout: Duration::from_secs(20),
        policy: Policy { max_snapshot_age: Duration::from_secs(86_400), ..Default::default() },
        ..Default::default()
    }
}

type Counts = Arc<Mutex<std::collections::HashMap<RepoId, usize>>>;

/// A probe that counts what it was asked to do.
struct Counting {
    inner: GitCliProbe,
    per_repo: Counts,
    total: Arc<AtomicUsize>,
}

impl Probe for Counting {
    fn probe<'a>(&'a self, req: ProbeRequest) -> BoxFuture<'a, RepoSnapshot> {
        *self.per_repo.lock().unwrap().entry(req.found.id.clone()).or_default() += 1;
        self.total.fetch_add(1, Ordering::SeqCst);
        self.inner.probe(req)
    }
}

fn counting() -> (Arc<Counting>, Counts, Arc<AtomicUsize>) {
    let per_repo: Counts = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let total = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(Counting {
        inner: GitCliProbe::hermetic(),
        per_repo: Arc::clone(&per_repo),
        total: Arc::clone(&total),
    });
    (probe, per_repo, total)
}

async fn settle(h: &EngineHandle) {
    // The engine is asynchronous by design: an invalidation and the probe it
    // causes are two events, in that order.
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = h.snapshot().await;
}

// ---- collapsing an event storm ------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_storm_of_events_during_a_job_collapses_to_one_reprobe() {
    // "A fetch writing thousands of objects produces exactly one re-probe" has
    // two halves, and this is the engine's. `crates/watch` discards the object
    // writes outright; what reaches here is whatever survives, and the busy
    // marker has to collapse it however much there is.
    //
    // The job is a stall rather than a real pull, deliberately: a local pull
    // finishes in milliseconds, so events sent "during" it would race the end
    // of it and a straggler arriving afterwards would earn a third probe
    // legitimately. That would make the test flaky about a property that is not
    // the one under test.
    let tmp = tempfile::tempdir().unwrap();
    let repos = clones(tmp.path(), 3);
    let (probe, per_repo, total) = counting();
    let engine = Engine::with_probe(config(), probe);
    let h = engine.handle();

    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;
    assert_eq!(snaps.len(), 3);
    assert_eq!(total.load(Ordering::SeqCst), 3, "one probe each during the scan");

    let stall = Action::Custom {
        args: vec!["-c".into(), "alias.stall=!sh -c 'sleep 2'".into(), "stall".into()],
        network: true,
        mutating: true,
    };
    let plan = Plan {
        action: stall.clone(),
        eligible: snaps.iter().map(|s| (s.id.clone(), stall.clone())).collect(),
        skipped: vec![],
        considered: snaps.len(),
        warning: None,
    };

    let mut events = h.subscribe();
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    let mut running = 0;
    while running < 3 {
        if let Ok(Event::JobStateChanged { state: JobState::Running, .. }) = events.recv().await {
            running += 1;
        }
    }

    // Every repository holds a job now. Report a storm at each.
    for snap in &snaps {
        for _ in 0..50 {
            h.invalidate(Invalidation::Repos(vec![snap.id.clone()])).await.unwrap();
        }
    }

    while !matches!(events.recv().await, Ok(Event::BatchDone { id, .. }) if id == batch) {}
    settle(&h).await;
    engine.shutdown().await;

    let counts = per_repo.lock().unwrap();
    for snap in &snaps {
        let n = counts[&snap.id];
        // One for the scan, then exactly one more — however many of the 50
        // arrived while the job held the repository.
        assert_eq!(n, 2, "{} was probed {n} times, not twice", snap.id.name());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalidation_re_probes_a_repository_that_is_not_busy() {
    // The other half: with nothing in flight, a report is acted on rather than
    // deferred. Editing a file has to move the row.
    let tmp = tempfile::tempdir().unwrap();
    let repos = clones(tmp.path(), 1);
    let (probe, per_repo, _) = counting();
    let engine = Engine::with_probe(config(), probe);
    let h = engine.handle();

    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;
    let id = snaps[0].id.clone();
    assert!(snaps[0].is_clean());

    std::fs::write(id.path().join("untracked.txt"), "hello\n").unwrap();
    h.invalidate(Invalidation::Repos(vec![id.clone()])).await.unwrap();
    settle(&h).await;

    let after = h.snapshot().await.unwrap();
    assert_eq!(after[0].work.untracked, 1, "the row did not catch up");
    assert_eq!(per_repo.lock().unwrap()[&id], 2);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_appearing_is_discovered_without_a_rescan() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = clones(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    assert_eq!(h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots.len(), 1);

    // Clone a second one, the way a user would, and report only the `.git`.
    let origin = tmp.path().canonicalize().unwrap().join("origin.git");
    git(&repos, &["clone", origin.to_str().unwrap(), "fresh"]);
    h.invalidate(Invalidation::Discover(repos.join("fresh"))).await.unwrap();
    settle(&h).await;

    let after = h.snapshot().await.unwrap();
    assert_eq!(after.len(), 2, "{:?}", after.iter().map(|s| s.id.name()).collect::<Vec<_>>());
    assert!(after.iter().any(|s| s.id.name() == "fresh"));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_that_went_away_leaves_the_grid() {
    // A row for a directory the user deleted is worse than no row: every action
    // offered on it will fail.
    let tmp = tempfile::tempdir().unwrap();
    let repos = clones(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;
    assert_eq!(snaps.len(), 2);
    let doomed = snaps[0].id.clone();

    let mut events = h.subscribe();
    std::fs::remove_dir_all(doomed.path()).unwrap();
    h.invalidate(Invalidation::Gone(vec![doomed.clone()])).await.unwrap();

    let removed = loop {
        match events.recv().await.unwrap() {
            Event::ReposRemoved(ids) => break ids,
            _ => continue,
        }
    };
    assert_eq!(removed, vec![doomed.clone()]);

    let after = h.snapshot().await.unwrap();
    assert_eq!(after.len(), 1);
    assert!(!after.iter().any(|s| s.id == doomed));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalidation_for_a_repository_the_engine_never_had_is_ignored() {
    // The watcher's index is rebuilt when a scan settles, so it can briefly
    // name a repository this actor has already dropped. That must not panic and
    // must not resurrect a row.
    let tmp = tempfile::tempdir().unwrap();
    let repos = clones(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![repos.clone()], false).await.unwrap();

    let ghost = RepoId::from_canonical(repos.join("never-existed"));
    h.invalidate(Invalidation::Repos(vec![ghost.clone()])).await.unwrap();
    h.invalidate(Invalidation::Gone(vec![ghost])).await.unwrap();
    settle(&h).await;

    assert_eq!(h.snapshot().await.unwrap().len(), 1);
    engine.shutdown().await;
}

// ---- the watcher's own pipeline, end to end ----------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_pipeline_turns_real_paths_into_the_right_requests() {
    // `Pending` against an index built the way the engine builds it, over the
    // paths a real `git pull` touches. The pieces are unit-tested apart; this
    // is the seam.
    let tmp = tempfile::tempdir().unwrap();
    let repos = clones(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![repos.clone()], false).await.unwrap();

    let watched: Vec<Watched> = h.watched().await.unwrap();
    assert_eq!(watched.len(), 1);
    let repo = watched[0].path.clone();
    let index = Index::new(watched);

    let mut pending = Pending::default();
    let touched = |p: PathBuf| Observed::new(p, Change::Touched);
    for i in 0..500 {
        pending.absorb(&index, &touched(repo.join(format!(".git/objects/ab/{i:03}"))), &|p| {
            p.exists()
        });
    }
    assert!(pending.is_empty(), "a fetch's objects are not news");

    pending.absorb(&index, &touched(repo.join(".git/refs/remotes/origin/main")), &|p| p.exists());
    pending.absorb(&index, &touched(repo.join("a.txt")), &|p| p.exists());
    assert_eq!(
        pending.drain(),
        [Invalidation::Repos(vec![RepoId::from_canonical(&repo)])],
        "one repository, one request"
    );

    engine.shutdown().await;
}

// ---- engine integration -------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_directory_cannot_spin_the_probe_pool() {
    // The watcher's debounce collapses one window; a directory being written to
    // *continuously* produces a fresh window every 300 ms forever, and each one
    // would otherwise be a `git status`.
    let tmp = tempfile::tempdir().unwrap();
    let repos = clones(tmp.path(), 1);
    let (probe, per_repo, _) = counting();
    let engine = Engine::with_probe(config(), probe);
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;
    let id = snaps[0].id.clone();

    // Two seconds of a build directory, reported the way a watcher would.
    let until = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < until {
        h.invalidate(Invalidation::Repos(vec![id.clone()])).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    settle(&h).await;
    engine.shutdown().await;

    // One for the scan, then at most one per second of activity. The bound is
    // generous — what it rules out is the hundred that arrived.
    let n = per_repo.lock().unwrap()[&id];
    assert!(n <= 5, "{n} probes for two seconds of filesystem noise");
    assert!(n >= 2, "the noise was ignored entirely, which is the other failure");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_finishing_is_never_rate_limited() {
    // The limit must not delay the row that shows a job's result. A completed
    // job is a definite change and there is exactly one of it.
    let tmp = tempfile::tempdir().unwrap();
    let repos = clones(tmp.path(), 2);
    let (probe, per_repo, _) = counting();
    let engine = Engine::with_probe(config(), probe);
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;

    // Immediately — well inside the rate limit's window from the scan's probe.
    let plan = h.plan(Action::Fetch { prune: false, tags: false }, Selection::All).await.unwrap();
    let mut events = h.subscribe();
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id, .. }) if id == batch) {}

    // A short wait, much less than the rate limit: the re-probe must already
    // have happened.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let counts = per_repo.lock().unwrap().clone();
    for snap in &snaps {
        assert_eq!(counts[&snap.id], 2, "{} was not re-probed promptly", snap.id.name());
    }
    drop(counts);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_coverage_is_what_keeps_a_quiet_repository_current() {
    // A snapshot goes stale by *sitting still*. Without coverage a repository
    // nobody touches is refused every action thirty seconds after launch, with
    // "refresh first", while being perfectly current. A watcher is a positive
    // guarantee that nothing changed, which is better evidence than a
    // timestamp.
    let tmp = tempfile::tempdir().unwrap();
    let repos = clones(tmp.path(), 1);
    let engine = Engine::start(Config {
        policy: git_scylla_engine::Policy {
            max_snapshot_age: Duration::from_millis(1),
            ..Default::default()
        },
        ..config()
    });
    let h = engine.handle();
    h.scan_to_completion(vec![repos.clone()], false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Uncovered: the bound has passed, so the row is stale and a pull is
    // refused by name rather than acted on.
    let plan = h
        .plan(Action::Pull { mode: git_scylla_core::PullMode::FfOnly }, Selection::All)
        .await
        .unwrap();
    assert!(plan.is_empty());
    assert!(
        plan.skipped.iter().all(|(_, why)| *why == git_scylla_core::SkipReason::SnapshotStale),
        "{:?}",
        plan.skipped
    );

    // Covered: the same row, the same age, and now current.
    h.set_watched(true).await.unwrap();
    settle(&h).await;
    let plan = h
        .plan(Action::Pull { mode: git_scylla_core::PullMode::FfOnly }, Selection::All)
        .await
        .unwrap();
    assert_eq!(plan.eligible.len(), 1, "{:?}", plan.skipped);
    engine.shutdown().await;
}
