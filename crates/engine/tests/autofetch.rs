//! Automatic fetching, against the engine.
//!
//! The schedule itself — backoff, quarantine, jitter — is a pure function and is
//! tested in `policy.rs` by advancing a clock. What can only be asserted here is
//! the part the engine owns: that it fetches without being asked, that it does
//! not do so before the scan has settled, that it yields to the user, and that
//! a fetch's outcome reaches the repository's health.

use git_scylla_core::{Action, FetchSchedule, JobOrigin, JobState};
use git_scylla_engine::{Config, Engine, EngineHandle, Event, FetchPolicy, Plan, Policy};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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

struct World {
    dir: PathBuf,
    repos: PathBuf,
}

/// A bare origin with one commit, and `n` clones of it.
fn world(dir: &Path, n: usize) -> World {
    std::fs::create_dir_all(dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let (repos, scratch) = (dir.join("repos"), dir.join("scratch"));
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
    World { dir, repos }
}

impl World {
    /// Push a commit into the origin. Nothing tells the clones.
    fn advance(&self) {
        let seed = self.dir.join("scratch/seed");
        let n = std::fs::read_to_string(seed.join("a.txt")).unwrap().len();
        std::fs::write(seed.join("a.txt"), format!("{n}\n")).unwrap();
        git(&seed, &["commit", "-am", "next"]);
        git(&seed, &["push", "origin", "main"]);
    }
}

/// Fetch aggressively, so a test is seconds rather than a quarter of an hour.
fn config(interval: Duration) -> Config {
    Config {
        extra_env: vec![
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
        ],
        probe_timeout: Duration::from_secs(20),
        policy: Policy { max_snapshot_age: Duration::from_secs(86_400), ..Default::default() },
        fetch: FetchPolicy { interval, jitter_pct: 0, ..FetchPolicy::default() },
        fetch_tick: Duration::from_millis(200),
        ..Default::default()
    }
}

/// Poll until `pred` holds, or give up.
async fn wait_for(
    h: &EngineHandle,
    within: Duration,
    pred: impl Fn(&[git_scylla_core::RepoSnapshot]) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if pred(&h.snapshot().await.unwrap()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

// ---- the headline claim ------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn behind_catches_up_with_no_user_action_at_all() {
    // The headline claim: with no user interaction at all, every repository's
    // `behind` count reflects its remote within one interval of a push.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    let engine = Engine::start(config(Duration::from_millis(300)));
    let h = engine.handle();

    let snaps = h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap().snapshots;
    assert_eq!(snaps.len(), 3);
    assert!(snaps.iter().all(|s| s.upstream.as_ref().unwrap().behind() == Some(0)));

    // Somebody pushes. Nothing tells the clones, and nobody presses anything.
    w.advance();

    assert!(
        wait_for(&h, Duration::from_secs(30), |snaps| {
            snaps.iter().all(|s| s.upstream.as_ref().and_then(|u| u.behind()) == Some(1))
        })
        .await,
        "the working set never noticed the push"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_initial_scan_issues_no_network_commands() {
    // There is no fetch in the startup path, and adding one is a regression.
    // Asserted through the job stream: a background fetch is a job, so "no job
    // before `ScanDone`" is the property exactly.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 4);
    let engine = Engine::start(config(Duration::from_millis(200)));
    let h = engine.handle();

    let mut events = h.subscribe();
    let scan = h.start_scan(vec![w.repos.clone()], false).await.unwrap();
    loop {
        match events.recv().await.unwrap() {
            Event::ScanDone { scan: id, .. } if id == scan => break,
            Event::JobStateChanged { id, repo, .. } => {
                panic!("job {id:?} against {} ran before the scan settled", repo.name())
            }
            _ => continue,
        }
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_records_its_outcome_against_the_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    let engine = Engine::start(config(Duration::from_millis(300)));
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    assert!(
        wait_for(&h, Duration::from_secs(20), |snaps| {
            snaps[0].fetch.last_success.is_some()
                && matches!(snaps[0].fetch.schedule, FetchSchedule::Due(_))
        })
        .await,
        "a successful background fetch left no trace on the repository"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_remote_backs_off_and_keeps_its_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    // Point it somewhere that cannot work, without touching the network: a
    // local path that is not a repository fails immediately and locally.
    let repo = w.repos.join("r00");
    let nowhere = w.dir.join("not-a-repo");
    std::fs::create_dir_all(&nowhere).unwrap();
    git(&repo, &["remote", "set-url", "origin", nowhere.to_str().unwrap()]);

    let engine = Engine::start(config(Duration::from_millis(200)));
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    assert!(
        wait_for(&h, Duration::from_secs(20), |snaps| {
            matches!(snaps[0].fetch.schedule, FetchSchedule::BackingOff { .. })
        })
        .await,
        "a failing remote was not backed off"
    );
    let snaps = h.snapshot().await.unwrap();
    match &snaps[0].fetch.schedule {
        FetchSchedule::BackingOff { failures, .. } => assert!(*failures >= 1),
        other => panic!("{other:?}"),
    }
    assert!(snaps[0].fetch.last_attempt.is_some());
    assert!(snaps[0].fetch.last_success.is_none());
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_with_no_remote_is_disabled_rather_than_perpetually_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let solo = tmp.path().join("repos/solo");
    std::fs::create_dir_all(&solo).unwrap();
    git(&solo, &["init", "-b", "main", "."]);
    std::fs::write(solo.join("a.txt"), "a\n").unwrap();
    git(&solo, &["add", "a.txt"]);
    git(&solo, &["commit", "-m", "c1"]);

    let root = tmp.path().join("repos").canonicalize().unwrap();
    let engine = Engine::start(config(Duration::from_millis(200)));
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    tokio::time::sleep(Duration::from_secs(2)).await;
    let snaps = h.snapshot().await.unwrap();
    assert_eq!(snaps[0].fetch.schedule, FetchSchedule::Disabled);
    assert!(snaps[0].fetch.last_attempt.is_none(), "it was attempted anyway");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_fetching_yields_while_a_user_batch_runs() {
    // Their batch has the permits and the priority. Adding background network
    // work to it is how a fetch cycle becomes something the user can feel.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    let engine = Engine::start(config(Duration::from_millis(150)));
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap().snapshots;

    let stall = Action::Custom {
        args: vec!["-c".into(), "alias.stall=!sh -c 'sleep 3'".into(), "stall".into()],
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
    let mut background = Vec::new();
    loop {
        match events.recv().await.unwrap() {
            Event::BatchDone { id, .. } if id == batch => break,
            Event::JobStateChanged { origin: JobOrigin::Background, repo, .. } => {
                background.push(repo)
            }
            _ => continue,
        }
    }
    // Ticks fired throughout — the batch takes three seconds and the tick is
    // 150 ms — and every one of them declined.
    assert!(background.is_empty(), "background work ran during a user batch: {background:?}");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_off_switch_means_nothing_fetches() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 2);
    let engine = Engine::start(Config {
        fetch: FetchPolicy { enabled: false, ..config(Duration::from_millis(100)).fetch },
        ..config(Duration::from_millis(100))
    });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();
    w.advance();

    tokio::time::sleep(Duration::from_secs(2)).await;
    let snaps = h.snapshot().await.unwrap();
    assert!(snaps.iter().all(|s| s.fetch.last_attempt.is_none()), "something fetched anyway");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_users_own_fetch_clears_a_quarantine() {
    // The reset button, and the only one. Driven through the engine rather than
    // through `policy` so that the *routing* is asserted too: a user-origin
    // fetch has to take the manual path.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    let engine = Engine::start(Config {
        // Off, so nothing races the manual fetch under test.
        fetch: FetchPolicy { enabled: false, ..FetchPolicy::default() },
        ..config(Duration::from_secs(900))
    });
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap().snapshots;
    let id = snaps[0].id.clone();

    let plan = h
        .plan(
            Action::Fetch { prune: true, tags: false },
            git_scylla_engine::Selection::ids([id.clone()]),
        )
        .await
        .unwrap();
    assert_eq!(plan.eligible.len(), 1);
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    let mut events = h.subscribe();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id: b, .. }) if b == batch) {
        if h.jobs(batch).await.unwrap().iter().all(|j| j.state == JobState::Ok) {
            break;
        }
    }

    assert!(
        wait_for(&h, Duration::from_secs(10), move |snaps| {
            snaps.iter().any(|s| s.id == id && s.fetch.last_success.is_some())
        })
        .await,
        "a manual fetch left no trace"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_fetch_takes_the_ordinary_post_job_reprobe_path() {
    // Nothing about the re-probe path changes for a background job: one
    // re-probe, and the row catches up on its own.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    let engine = Engine::start(config(Duration::from_millis(300)));
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();
    w.advance();

    assert!(
        wait_for(&h, Duration::from_secs(20), |snaps| {
            snaps[0].upstream.as_ref().and_then(|u| u.behind()) == Some(1)
        })
        .await,
        "the fetch happened but the row never caught up"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_transcripts_are_bounded() {
    // A process that runs forever needs a ceiling on what it keeps.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 2);
    let engine =
        Engine::start(Config { background_history: 3, ..config(Duration::from_millis(120)) });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    // Let several cycles run.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let kept = h.background_jobs().await.unwrap();
    assert!(kept.len() <= 3, "kept {} background transcripts, bound is 3", kept.len());
    assert!(!kept.is_empty(), "the bound evicted everything");
    engine.shutdown().await;
}
