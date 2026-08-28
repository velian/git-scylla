//! The startup cache.
#![allow(
    clippy::await_holding_lock,
    reason = "the lock serializes a process-wide environment variable for the \
              whole of each test, which is exactly as long as it must be held"
)]

use git_scylla_core::ProbeOutcome;
use git_scylla_engine::{Config, Engine, Policy};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

static STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A private state directory, held for the duration of one test.
///
/// `$GIT_SCYLLA_STATE_DIR` is process-wide, so the tests in this binary take a
/// lock rather than racing each other's cache file — and the variable is
/// cleared on drop, so a test that panics cannot leak it into the next one.
struct StateDir {
    dir: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl StateDir {
    fn new() -> Self {
        let lock = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GIT_SCYLLA_STATE_DIR", dir.path());
        Self { dir, _lock: lock }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        std::env::remove_var("GIT_SCYLLA_STATE_DIR");
    }
}

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

/// `n` repositories with one commit each.
fn repos(dir: &Path, n: usize) -> PathBuf {
    let repos = dir.join("repos");
    std::fs::create_dir_all(&repos).unwrap();
    let repos = repos.canonicalize().unwrap();
    for i in 0..n {
        let name = format!("r{i:02}");
        git(&repos, &["init", "-b", "main", &name]);
        let p = repos.join(&name);
        std::fs::write(p.join("a.txt"), "a\n").unwrap();
        git(&p, &["add", "a.txt"]);
        git(&p, &["commit", "-m", "c1"]);
    }
    repos
}

fn config() -> Config {
    Config {
        extra_env: vec![
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
        ],
        probe_timeout: Duration::from_secs(20),
        policy: Policy { max_snapshot_age: Duration::from_secs(86_400), ..Default::default() },
        cache: git_scylla_engine::CacheMode::ReadWrite,
        ..Default::default()
    }
}

/// Run one engine over `roots` to completion, then stop it — which is what
/// flushes the cache.
async fn one_run(roots: Vec<PathBuf>) -> usize {
    let engine = Engine::start(config());
    let h = engine.handle();
    let n = h.scan_to_completion(roots, false).await.unwrap().snapshots.len();
    engine.shutdown().await;
    n
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_launch_has_rows_before_it_has_a_scan() {
    let _state = StateDir::new();
    let tmp = tempfile::tempdir().unwrap();
    let roots = vec![repos(tmp.path(), 3)];

    assert_eq!(one_run(roots.clone()).await, 3, "the first run has to scan for them");

    // The second engine serves the cache when the scan is *asked for*, before
    // the walk has found anything.
    let engine = Engine::start(config());
    let h = engine.handle();
    let mut events = h.subscribe();
    let scan = h.start_scan(roots.clone(), false).await.unwrap();

    let first = loop {
        match events.recv().await.unwrap() {
            git_scylla_engine::Event::ReposUpserted(snaps) => break snaps,
            _ => continue,
        }
    };
    assert_eq!(first.len(), 3, "the cached rows arrive in one batch, not one at a time");
    // ...and every one of them is unusable until the scan replaces it.
    let now = SystemTime::now();
    assert!(
        first.iter().all(|s| s.is_stale(now, Duration::from_secs(30))),
        "a cached row must never present as current"
    );

    let _ = scan;
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_the_rescan_does_not_find_is_dropped_when_the_scan_ends() {
    let _state = StateDir::new();
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 3);
    let roots = vec![root.clone()];

    assert_eq!(one_run(roots.clone()).await, 3);

    // One goes away while nothing is running.
    std::fs::remove_dir_all(root.join("r01")).unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    let outcome = h.scan_to_completion(roots.clone(), false).await.unwrap();
    // `scan_to_completion` reads the snapshot after `ScanDone`, and the pruning
    // happens before that event is emitted — so the row is already gone.
    assert_eq!(outcome.snapshots.len(), 2, "{:?}", names(&outcome.snapshots));
    assert!(!names(&outcome.snapshots).contains(&"r01".to_string()));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cache_for_other_roots_is_not_this_window_s() {
    let _state = StateDir::new();
    let tmp = tempfile::tempdir().unwrap();
    let a = repos(&tmp.path().join("a"), 2);
    let b = repos(&tmp.path().join("b"), 1);

    assert_eq!(one_run(vec![a]).await, 2);

    // Different roots: the cache is not wrong, but showing it would put rows on
    // screen the user has since removed a root for.
    let engine = Engine::start(config());
    let h = engine.handle();
    let mut events = h.subscribe();
    h.start_scan(vec![b], false).await.unwrap();
    let first = loop {
        match events.recv().await.unwrap() {
            git_scylla_engine::Event::ReposUpserted(snaps) => break snaps,
            _ => continue,
        }
    };
    assert_eq!(first.len(), 1, "the first rows came from the scan, not the cache");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_with_the_cache_off_neither_reads_nor_writes_one() {
    // The CLI's setting. It is a one-shot process whose roots are whatever was
    // on the command line, so it must not clobber the application's cache.
    let state = StateDir::new();
    let tmp = tempfile::tempdir().unwrap();
    let roots = vec![repos(tmp.path(), 2)];

    let engine = Engine::start(Config { cache: git_scylla_engine::CacheMode::Off, ..config() });
    let h = engine.handle();
    h.scan_to_completion(roots, false).await.unwrap();
    engine.shutdown().await;

    assert!(!state.path().join("cache.json").exists(), "the CLI wrote a cache");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_cache_costs_a_slower_launch_and_nothing_else() {
    let state = StateDir::new();
    std::fs::write(state.path().join("cache.json"), b"{ half a fi").unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let roots = vec![repos(tmp.path(), 2)];

    // Killing the app mid-write is the case: the file is there and unreadable.
    assert_eq!(one_run(roots).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn what_is_cached_is_what_was_probed() {
    let _state = StateDir::new();
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2);
    std::fs::write(root.join("r00/untracked.txt"), "x\n").unwrap();
    let roots = vec![root];

    one_run(roots.clone()).await;

    let cache = git_scylla_store::Cache::load_for(&roots).expect("a cache");
    assert_eq!(cache.repos.len(), 2);
    let dirty = cache.repos.iter().find(|s| s.id.name() == "r00").unwrap();
    assert_eq!(dirty.work.untracked, 1, "the cache kept the facts, not just the paths");
    assert!(cache.repos.iter().all(|s| s.outcome == ProbeOutcome::Ok));
}

fn names(snaps: &[git_scylla_core::RepoSnapshot]) -> Vec<String> {
    snaps.iter().map(|s| s.id.name().to_string()).collect()
}
