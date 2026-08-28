//! The engine end to end.
//!
//! Everything here uses local bare repositories as remotes, so the whole suite
//! runs with no network — which is itself one of the criteria.

use git_scylla_core::{Action, JobOrigin, JobState, PullMode, RepoId, SkipReason};
use git_scylla_engine::sched::Limits;
use git_scylla_engine::{plan, Config, Engine, EngineHandle, Event, Plan, Policy, Selection};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// ---- fixtures ----------------------------------------------------------

fn git(cwd: &Path, args: &[&str]) -> std::process::Output {
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
    out
}

/// A bare repository with one commit, plus `n` clones of it under `repos/`.
///
/// One origin rather than `n` of them: the point is `n` concurrent fetches, and
/// building twenty bare repositories to prove that is twenty times the setup for
/// the same assertion.
struct Cloned {
    dir: PathBuf,
    repos: PathBuf,
}

fn clones(dir: &Path, n: usize) -> Cloned {
    std::fs::create_dir_all(dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let repos = dir.join("repos");
    let scratch = dir.join("scratch");
    for p in [&repos, &scratch] {
        std::fs::create_dir_all(p).unwrap();
    }
    git(&dir, &["init", "--bare", "-b", "main", "origin.git"]);
    let origin = dir.join("origin.git");
    let origin = origin.as_path();

    let seed = scratch.join("seed");
    git(&scratch, &["clone", origin.to_str().unwrap(), "seed"]);
    std::fs::write(seed.join("a.txt"), "one\n").unwrap();
    git(&seed, &["add", "a.txt"]);
    git(&seed, &["commit", "-m", "c1"]);
    git(&seed, &["push", "-u", "origin", "main"]);

    for i in 0..n {
        git(&repos, &["clone", origin.to_str().unwrap(), &format!("r{i:02}")]);
    }
    Cloned { dir, repos }
}

/// Move `origin` forward, so every clone is one behind after a fetch.
fn advance(c: &Cloned) {
    let seed = c.dir.join("scratch/seed");
    std::fs::write(seed.join("a.txt"), "two\n").unwrap();
    git(&seed, &["commit", "-am", "c2"]);
    git(&seed, &["push", "origin", "main"]);
}

fn config() -> Config {
    Config {
        // Hermetic: a developer's ~/.gitconfig must not be able to change a
        // result here.
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

/// Run a batch and collect every job once it settles.
async fn run(h: &EngineHandle, p: Plan) -> Vec<git_scylla_core::Job> {
    let mut events = h.subscribe();
    let batch = h.start_batch(p, JobOrigin::User).await.unwrap();
    loop {
        match events.recv().await {
            Ok(Event::BatchDone { id, .. }) if id == batch => break,
            Ok(_) => {}
            Err(e) => panic!("event stream ended: {e}"),
        }
    }
    h.jobs(batch).await.unwrap()
}

/// Poll the engine's snapshots until `pred` holds, or give up.
///
/// The engine is asynchronous by design: a job finishing and the tool's view of
/// the repository catching up are two events, in that order.
async fn wait_until(
    h: &EngineHandle,
    within: Duration,
    pred: impl Fn(&[git_scylla_core::RepoSnapshot]) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if pred(&h.snapshot().await.unwrap()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// ---- scanning ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_finds_and_probes_every_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 5);
    let engine = Engine::start(config());
    let h = engine.handle();

    let snaps = h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap().snapshots;
    assert_eq!(snaps.len(), 5);
    assert!(snaps.iter().all(|s| s.is_trustworthy()));
    assert!(snaps.iter().all(|s| s.remotes.len() == 1));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_of_a_missing_root_finishes_rather_than_hanging() {
    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h
        .scan_to_completion(vec![PathBuf::from("/nope/nope/nope")], false)
        .await
        .unwrap()
        .snapshots;
    assert!(snaps.is_empty());
    engine.shutdown().await;
}

// ---- acceptance: the ff-only partition ---------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ff_only_partitions_exactly_as_expected() {
    // `pull --mode ff-only --dry-run` must produce exactly the expected
    // eligible/skipped partition.
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 4);
    advance(&c);

    // r00: behind and clean        -> eligible
    // r01: behind and dirty        -> DirtyWorktree
    // r02: behind and ahead        -> Diverged (ff-only cannot)
    // r03: never fetched, in sync  -> UpToDate
    for name in ["r00", "r01", "r02"] {
        git(&c.repos.join(name), &["fetch"]);
    }
    std::fs::write(c.repos.join("r01/a.txt"), "local\n").unwrap();
    std::fs::write(c.repos.join("r02/b.txt"), "local\n").unwrap();
    git(&c.repos.join("r02"), &["add", "b.txt"]);
    git(&c.repos.join("r02"), &["commit", "-m", "local"]);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();

    let p = h.plan(Action::Pull { mode: PullMode::FfOnly }, Selection::All).await.unwrap();

    let name = |id: &RepoId| id.name().to_string();
    let eligible: HashSet<String> = p.eligible.iter().map(|(id, _)| name(id)).collect();
    let skipped: HashMap<String, SkipReason> =
        p.skipped.iter().map(|(id, why)| (name(id), why.clone())).collect();

    assert_eq!(eligible, HashSet::from(["r00".to_string()]));
    assert_eq!(skipped["r01"], SkipReason::DirtyWorktree);
    assert_eq!(skipped["r02"], SkipReason::Diverged);
    assert_eq!(skipped["r03"], SkipReason::UpToDate);
    assert_eq!(p.selected(), 4);
    engine.shutdown().await;
}

// ---- acceptance: a batch of 20 fetches ---------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn twenty_fetches_complete_with_retrievable_transcripts() {
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 20);
    advance(&c);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();

    let p = h.plan(Action::Fetch { prune: true, tags: false }, Selection::All).await.unwrap();
    assert_eq!(p.eligible.len(), 20);

    let jobs = run(&h, p).await;
    assert_eq!(jobs.len(), 20);
    for job in &jobs {
        assert_eq!(job.state, JobState::Ok, "{} -> {:?}", job.repo, job.state);
        // Fetch does not mutate, so no head_before is recorded.
        assert!(job.head_before.is_none());
        assert_eq!(job.steps.len(), 1);
        // The transcript is retrievable afterwards, by job id.
        let log = h.job_log(job.id).await.unwrap();
        assert_eq!(log.len(), job.log.len());
    }

    // The fetch really happened: every clone now knows it is behind.
    //
    // Polled rather than read once, because `BatchDone` fires when the jobs
    // finish and the re-probes land after it — see the note on the event. A
    // single read here would be a race that passes on a fast machine.
    let settled = wait_until(&h, Duration::from_secs(20), |snaps| {
        snaps.len() == 20
            && snaps.iter().all(|s| s.upstream.as_ref().and_then(|u| u.behind()) == Some(1))
    })
    .await;
    assert!(
        settled,
        "the re-probes never reported the fetched state: {:?}",
        h.snapshot()
            .await
            .unwrap()
            .iter()
            .map(|s| (s.id.name().to_string(), s.upstream.as_ref().and_then(|u| u.behind())))
            .collect::<Vec<_>>()
    );
    engine.shutdown().await;
}

// ---- acceptance: head_before ------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn every_mutating_job_records_head_before() {
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 3);
    advance(&c);
    for i in 0..3 {
        git(&c.repos.join(format!("r{i:02}")), &["fetch"]);
    }

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();

    let p = h.plan(Action::Pull { mode: PullMode::FfOnly }, Selection::All).await.unwrap();
    assert_eq!(p.eligible.len(), 3);
    let jobs = run(&h, p).await;

    for job in &jobs {
        assert_eq!(job.state, JobState::Ok, "{:?}", job.state);
        let head = job.head_before.as_ref().expect("a pull must record head_before");
        // It is the commit the repository was on, not the one it moved to.
        let now =
            String::from_utf8(git(&c.repos.join(job.repo.name()), &["rev-parse", "HEAD"]).stdout)
                .unwrap();
        assert_ne!(head.as_str(), now.trim(), "HEAD should have moved");
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unborn_repository_records_no_head_before() {
    // The documented exception: there is no commit to name.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let repo = dir.join("repos/unborn");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main", "."]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![dir.join("repos")], false).await.unwrap().snapshots;
    assert_eq!(snaps.len(), 1);

    // Commit is mutating, so head_before is attempted — and correctly absent.
    let action = Action::Commit { message: "first".into(), stage_all: true, no_verify: false };
    let p = plan(&action, &snaps, &Selection::All, std::time::SystemTime::now(), &config().policy);
    assert_eq!(p.eligible.len(), 1, "an unborn repository with content can commit");

    let jobs = run(&h, p).await;
    assert_eq!(jobs[0].state, JobState::Ok, "{:?}", jobs[0].state);
    assert!(jobs[0].head_before.is_none(), "no commit existed to record");
    engine.shutdown().await;
}

// ---- acceptance: an unreachable remote --------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_remote_fails_fast_without_stalling_the_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 5);
    // Loopback port 1: the kernel refuses instantly, so this needs no network
    // and its timing is deterministic. A *silently dropped* route is the other
    // shape of unreachable, and it takes as long as the OS TCP handshake — 4 to
    // 75 seconds, measured — which is bounded by the job deadline rather than by
    // anything this tool can ask git for. `crates/exec` covers that case.
    let broken = c.repos.join("r00");
    git(&broken, &["remote", "set-url", "origin", "https://127.0.0.1:1/nope.git"]);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();

    let p = h.plan(Action::Fetch { prune: false, tags: false }, Selection::All).await.unwrap();
    assert_eq!(p.eligible.len(), 5);

    let started = std::time::Instant::now();
    let jobs = run(&h, p).await;
    let elapsed = started.elapsed();

    let by_name: HashMap<String, &git_scylla_core::Job> =
        jobs.iter().map(|j| (j.repo.name().to_string(), j)).collect();
    assert!(matches!(by_name["r00"].state, JobState::Failed { .. }), "{:?}", by_name["r00"].state);
    for name in ["r01", "r02", "r03", "r04"] {
        assert_eq!(by_name[name].state, JobState::Ok, "{name} was dragged down");
    }
    // The whole batch, not just the healthy part.
    assert!(elapsed < Duration::from_secs(10), "the batch took {elapsed:?}");
    // And the failure is legible rather than an empty transcript.
    assert!(!by_name["r00"].log.is_empty());
    engine.shutdown().await;
}

// ---- acceptance: per-repository serialization -------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_will_not_finish_is_killed_by_its_deadline() {
    // The guarantee behind "no hang, ever". Not tested through a slow network,
    // whose timing is not ours to control, but through a command that simply
    // never returns — which is the same thing from the engine's side.
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 2);
    let engine = Engine::start(Config {
        limits: Limits {
            network: 4,
            per_host: 4,
            network_timeout: Duration::from_millis(600),
            ..Default::default()
        },
        ..config()
    });
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap().snapshots;

    // A network action, so it takes the network deadline.
    let stall = Action::Custom {
        args: vec!["-c".into(), "alias.stall=!sh -c 'sleep 30'".into(), "stall".into()],
        network: true,
        mutating: true,
    };
    let p = Plan {
        action: stall.clone(),
        eligible: snaps.iter().map(|s| (s.id.clone(), stall.clone())).collect(),
        skipped: vec![],
        considered: snaps.len(),
        warning: None,
    };

    let started = std::time::Instant::now();
    let jobs = run(&h, p).await;
    let elapsed = started.elapsed();

    assert_eq!(jobs.len(), 2);
    assert!(
        jobs.iter().all(|j| matches!(j.state, JobState::Failed { .. })),
        "{:?}",
        jobs.iter().map(|j| &j.state).collect::<Vec<_>>()
    );
    // The deadline plus the two-second grace, not the child's thirty seconds.
    assert!(elapsed < Duration::from_secs(6), "took {elapsed:?}");
    // And the transcript says why, rather than just ending.
    assert!(jobs[0].log.iter().any(|l| l.text.contains("timed out")), "{:?}", jobs[0].log);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_jobs_for_one_repository_never_run_at_once() {
    // Instrumented from the event stream, which is the engine's own account of
    // what it did — not from the scheduler's internals.
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 2);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();

    let mut events = h.subscribe();
    let action = Action::Fetch { prune: false, tags: false };
    let p1 = h.plan(action.clone(), Selection::All).await.unwrap();
    let p2 = h.plan(action.clone(), Selection::All).await.unwrap();
    let b1 = h.start_batch(p1, JobOrigin::User).await.unwrap();
    let b2 = h.start_batch(p2, JobOrigin::User).await.unwrap();

    let mut running: HashMap<RepoId, usize> = HashMap::new();
    let mut peak = 0usize;
    let mut done = HashSet::new();
    while done.len() < 2 {
        match events.recv().await.unwrap() {
            Event::JobStateChanged { repo, state, .. } => match state {
                JobState::Running => {
                    let n = running.entry(repo.clone()).or_default();
                    *n += 1;
                    peak = peak.max(*n);
                }
                s if s.is_terminal() => {
                    if let Some(n) = running.get_mut(&repo) {
                        *n = n.saturating_sub(1);
                    }
                }
                _ => {}
            },
            Event::BatchDone { id, .. } => {
                done.insert(id);
            }
            _ => {}
        }
    }
    assert!(done.contains(&b1) && done.contains(&b2));
    assert_eq!(peak, 1, "two jobs ran against one repository at the same time");
    engine.shutdown().await;
}

// ---- acceptance: cancellation -----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_batch_leaves_no_orphaned_processes() {
    // A custom action whose git alias spawns a shell that ignores SIGTERM and
    // announces survival by creating a file. Driven through the engine, so the
    // batch's cancellation token is what reaches the process group.
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 3);
    let marker_dir = c.dir.join("markers");
    std::fs::create_dir_all(&marker_dir).unwrap();

    let script = c.dir.join("stubborn.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\ntrap '' TERM\nfor i in 1 2 3 4 5; do sleep 1; done\ntouch \"$1/$2\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let engine = Engine::start(Config {
        limits: Limits { network: 8, per_host: 8, local: 8, ..Default::default() },
        ..config()
    });
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap().snapshots;
    assert_eq!(snaps.len(), 3);

    let mut eligible = Vec::new();
    for s in &snaps {
        let action = Action::Custom {
            args: vec![
                "-c".into(),
                format!(
                    "alias.stall=!{} {} {}",
                    script.display(),
                    marker_dir.display(),
                    s.id.name()
                ),
                "stall".into(),
            ],
            network: true,
            mutating: true,
        };
        eligible.push((s.id.clone(), action));
    }
    let p = Plan {
        action: eligible[0].1.clone(),
        eligible,
        skipped: vec![],
        considered: snaps.len(),
        warning: None,
    };

    let mut events = h.subscribe();
    let batch = h.start_batch(p, JobOrigin::User).await.unwrap();
    // Wait until they are actually running, so cancellation has something to kill.
    let mut running = 0;
    while running < 3 {
        if let Ok(Event::JobStateChanged { state: JobState::Running, .. }) = events.recv().await {
            running += 1;
        }
    }
    h.cancel_batch(batch).await.unwrap();

    loop {
        match events.recv().await.unwrap() {
            Event::BatchDone { id, .. } if id == batch => break,
            _ => {}
        }
    }
    let jobs = h.jobs(batch).await.unwrap();
    assert!(
        jobs.iter().all(|j| j.state == JobState::Cancelled),
        "{:?}",
        jobs.iter().map(|j| &j.state).collect::<Vec<_>>()
    );

    // Past when the scripts would have finished sleeping.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let survivors: Vec<_> = std::fs::read_dir(&marker_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(survivors.is_empty(), "these outlived the cancelled batch: {survivors:?}");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_marks_queued_jobs_cancelled_without_running_them() {
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 6);
    // One network slot, so most of the batch is still queued when it is killed.
    let engine = Engine::start(Config {
        limits: Limits { network: 1, per_host: 1, ..Default::default() },
        ..config()
    });
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();

    let mut events = h.subscribe();
    let p = h.plan(Action::Fetch { prune: false, tags: false }, Selection::All).await.unwrap();
    let batch = h.start_batch(p, JobOrigin::User).await.unwrap();
    // As soon as anything is running, kill it.
    loop {
        if let Ok(Event::JobStateChanged { state: JobState::Running, .. }) = events.recv().await {
            break;
        }
    }
    h.cancel_batch(batch).await.unwrap();
    loop {
        if let Ok(Event::BatchDone { id, .. }) = events.recv().await {
            if id == batch {
                break;
            }
        }
    }

    let jobs = h.jobs(batch).await.unwrap();
    assert_eq!(jobs.len(), 6);
    let cancelled = jobs.iter().filter(|j| j.state == JobState::Cancelled).count();
    assert!(cancelled >= 5, "expected most of the batch cancelled, got {cancelled}");
    // Nothing is left in limbo: every job reached a terminal state.
    assert!(jobs.iter().all(|j| j.state.is_terminal()), "a job was left queued or running");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_queued_job_does_not_free_a_repository_another_job_holds() {
    // One job per repository, ever — across *batches*, which is where it is
    // easiest to lose. A job cancelled out of the queue never took the
    // repository's busy marker, so settling it must not hand one back: the
    // marker belongs to whichever job is actually running there, and two `git`
    // processes in one repository contend for `index.lock` and fail in ways
    // that look like bugs.
    //
    // Two batches over one repository is the ordinary way to reach this, and
    // the fetch scheduler reaches it on a timer.
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 1);
    let engine = Engine::start(Config {
        // Generous on purpose: the *only* thing that may hold a job back here is
        // its repository being busy, so a permit shortage cannot make the test
        // pass for the wrong reason.
        limits: Limits { network: 8, per_host: 8, local: 8, ..Default::default() },
        ..config()
    });
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap().snapshots;
    let repo = snaps[0].id.clone();

    // Long enough to still be running when the other two batches arrive, and
    // well inside the 60 s network deadline so it ends by exiting rather than
    // by being killed.
    let stall = Action::Custom {
        args: vec!["-c".into(), "alias.stall=!sh -c 'sleep 4'".into(), "stall".into()],
        network: true,
        mutating: true,
    };
    let quick = Action::Custom {
        args: vec!["rev-parse".into(), "HEAD".into()],
        network: true,
        mutating: true,
    };
    let against = |a: &Action| Plan {
        action: a.clone(),
        eligible: vec![(repo.clone(), a.clone())],
        skipped: vec![],
        considered: 1,
        warning: None,
    };

    let mut events = h.subscribe();
    let holder = h.start_batch(against(&stall), JobOrigin::User).await.unwrap();
    // Wait for it to be *running*, not merely queued: the busy marker is set as
    // a job launches, so until then there is nothing to release by mistake.
    loop {
        if let Ok(Event::JobStateChanged { batch, state: JobState::Running, .. }) =
            events.recv().await
        {
            if batch == Some(holder) {
                break;
            }
        }
    }

    // Queued behind the holder — in the repository's own queue, because the
    // repository is busy — and then cancelled from there.
    let cancelled = h.start_batch(against(&quick), JobOrigin::User).await.unwrap();
    h.cancel_batch(cancelled).await.unwrap();

    // The third batch must wait for the holder. Before the fix it launched
    // immediately, because cancelling the second one had cleared the marker.
    let waiter = h.start_batch(against(&quick), JobOrigin::User).await.unwrap();

    let mut holder_done = false;
    loop {
        match events.recv().await.unwrap() {
            Event::JobStateChanged { batch, state, .. } if batch == Some(holder) => {
                holder_done |= state.is_terminal();
            }
            Event::JobStateChanged { batch, state: JobState::Running, .. }
                if batch == Some(waiter) =>
            {
                assert!(holder_done, "a second job started in {repo} while the first was running");
            }
            Event::BatchDone { id, .. } if id == waiter => break,
            _ => {}
        }
    }
    engine.shutdown().await;
}

// ---- re-probe suppression --------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_completed_job_causes_exactly_one_reprobe() {
    // Without this the watcher turns every pull into a probe storm. Counted
    // through a wrapping Probe, the engine's only I/O seam.
    use git_scylla_probe::{
        BoxFuture, GitCliProbe, Probe, ProbeRequest, RefAnswer, RefError, RefQuery, RefRequest,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Counting {
        inner: GitCliProbe,
        counts: Arc<std::sync::Mutex<HashMap<PathBuf, usize>>>,
        total: Arc<AtomicUsize>,
    }
    impl Probe for Counting {
        fn probe<'a>(&'a self, req: ProbeRequest) -> BoxFuture<'a, git_scylla_core::RepoSnapshot> {
            *self.counts.lock().unwrap().entry(req.found.path.clone()).or_default() += 1;
            self.total.fetch_add(1, Ordering::SeqCst);
            self.inner.probe(req)
        }

        /// Uncounted: this test counts probes per repository, and a ref read is
        /// not one.
        fn refs<'a>(
            &'a self,
            repos: Vec<RefRequest>,
            query: RefQuery,
        ) -> BoxFuture<'a, Vec<Result<RefAnswer, RefError>>> {
            self.inner.refs(repos, query)
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 3);
    advance(&c);

    let counts = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let total = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(Counting {
        inner: GitCliProbe::hermetic(),
        counts: counts.clone(),
        total: total.clone(),
    });
    let engine = Engine::with_probe(config(), probe);
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();
    assert_eq!(total.load(Ordering::SeqCst), 3, "one probe each during the scan");

    let p = h.plan(Action::Fetch { prune: false, tags: false }, Selection::All).await.unwrap();
    let jobs = run(&h, p).await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok));

    // Give any stray extra probe a chance to land before asserting.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let per_repo = counts.lock().unwrap().clone();
    for (path, n) in &per_repo {
        assert_eq!(*n, 2, "{} was probed {n} times, expected scan + one re-probe", path.display());
    }
    assert_eq!(total.load(Ordering::SeqCst), 6);
    engine.shutdown().await;
}

// ---- the global cap --------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_network_cap_bounds_concurrent_children() {
    // The engine's version of the scheduler's unit test: 20 jobs, cap of 4,
    // observed through the state events rather than through internals.
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 20);
    let engine = Engine::start(Config {
        limits: Limits { network: 4, per_host: 4, ..Default::default() },
        ..config()
    });
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();

    let mut events = h.subscribe();
    let p = h.plan(Action::Fetch { prune: false, tags: false }, Selection::All).await.unwrap();
    let batch = h.start_batch(p, JobOrigin::User).await.unwrap();

    let (mut live, mut peak) = (0i64, 0i64);
    loop {
        match events.recv().await.unwrap() {
            Event::JobStateChanged { state: JobState::Running, .. } => {
                live += 1;
                peak = peak.max(live);
            }
            Event::JobStateChanged { state, .. } if state.is_terminal() => live -= 1,
            Event::BatchDone { id, .. } if id == batch => break,
            _ => {}
        }
    }
    assert!(peak <= 4, "peak concurrency was {peak}, cap is 4");
    assert!(peak > 1, "the cap should not have serialized everything");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_with_nothing_eligible_finishes_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();

    // Nothing is behind, so an ff-only pull has nothing to do.
    let p = h.plan(Action::Pull { mode: PullMode::FfOnly }, Selection::All).await.unwrap();
    assert!(p.is_empty());
    let jobs = run(&h, p).await;
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().all(|j| matches!(j.state, JobState::Skipped { .. })));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_scan_stops_and_still_reports_done() {
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 3);
    let engine = Engine::start(config());
    let h = engine.handle();

    let mut events = h.subscribe();
    let id = h.start_scan(vec![c.repos.clone()], false).await.unwrap();
    h.cancel_scan(id).await.unwrap();
    // Whatever it managed to find, it must still settle rather than leaving the
    // caller waiting forever.
    loop {
        match events.recv().await.unwrap() {
            Event::ScanDone { scan, .. } if scan == id => break,
            _ => {}
        }
    }
    engine.shutdown().await;
}

// ---- scan bookkeeping --------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_scans_keep_separate_accounts() {
    // The counters used to be shared: every discovery incremented every scan,
    // so two scans each counted the other's work and `ScanDone` fired on the
    // wrong arithmetic. There are two scans the moment the user adds a root
    // while one is running, or presses Refresh.
    let tmp = tempfile::tempdir().unwrap();
    let a = clones(&tmp.path().join("a"), 4);
    let b = clones(&tmp.path().join("b"), 7);

    let engine = Engine::start(config());
    let h = engine.handle();
    let mut events = h.subscribe();

    let id_a = h.start_scan(vec![a.repos.clone()], false).await.unwrap();
    let id_b = h.start_scan(vec![b.repos.clone()], false).await.unwrap();
    assert_ne!(id_a, id_b);

    let mut peak: HashMap<_, (usize, usize)> = HashMap::new();
    let mut done = HashSet::new();
    while done.len() < 2 {
        match events.recv().await.unwrap() {
            Event::ScanProgress { scan, found, probed } => {
                let e = peak.entry(scan).or_default();
                e.0 = e.0.max(found);
                e.1 = e.1.max(probed);
                assert!(probed <= found, "{scan}: probed {probed} > found {found}");
            }
            Event::ScanDone { scan, .. } => {
                done.insert(scan);
            }
            _ => {}
        }
    }
    assert!(done.contains(&id_a) && done.contains(&id_b));
    // Each scan counted only its own root.
    assert_eq!(peak[&id_a], (4, 4), "scan a");
    assert_eq!(peak[&id_b], (7, 7), "scan b");

    assert_eq!(h.snapshot().await.unwrap().len(), 11);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_alongside_a_batch_settles_on_its_own_repositories() {
    // The re-probes a batch produces used to increment every in-flight scan's
    // `probed`, so a scan could satisfy its own completion test before its
    // repositories had been read — reporting done with rows still missing.
    let tmp = tempfile::tempdir().unwrap();
    let running = clones(&tmp.path().join("running"), 6);
    let fresh = clones(&tmp.path().join("fresh"), 6);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![running.repos.clone()], false).await.unwrap();

    // Start a batch, then scan a *different* root while it runs.
    let p = h.plan(Action::Fetch { prune: false, tags: false }, Selection::All).await.unwrap();
    assert_eq!(p.eligible.len(), 6);
    let mut events = h.subscribe();
    let batch = h.start_batch(p, JobOrigin::User).await.unwrap();
    let scan = h.start_scan(vec![fresh.repos.clone()], false).await.unwrap();

    let mut scan_done_at: Option<usize> = None;
    let mut upserted = 0usize;
    let mut batch_done = false;
    loop {
        match events.recv().await.unwrap() {
            Event::ReposUpserted(v) => upserted += v.len(),
            Event::ScanDone { scan: done, .. } if done == scan => scan_done_at = Some(upserted),
            Event::BatchDone { id, .. } if id == batch => batch_done = true,
            _ => {}
        }
        if scan_done_at.is_some() && batch_done {
            break;
        }
    }
    assert!(scan_done_at.is_some());

    // The real property: when the scan said it was done, every repository under
    // its root actually had a snapshot.
    let snaps = h.snapshot().await.unwrap();
    for i in 0..6 {
        let want = fresh.repos.join(format!("r{i:02}"));
        assert!(snaps.iter().any(|s| s.path == want), "{} missing after ScanDone", want.display());
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_settles_even_when_discovered_paths_vanish_underneath_it() {
    // `Internal::Found` used to count a repository before canonicalizing its
    // path. A path that disappeared in between — a build directory being cleaned
    // mid-scan — left the scan permanently one short, so `ScanDone` never fired
    // and `shutdown()` never returned.
    //
    // The trigger is a race, so this deletes aggressively during the walk and
    // asserts the scan settles regardless of whether it lands. It cannot fail
    // spuriously: settling is required either way.
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 30);

    let engine = Engine::start(config());
    let h = engine.handle();
    let mut events = h.subscribe();
    let scan = h.start_scan(vec![c.repos.clone()], false).await.unwrap();

    let repos = c.repos.clone();
    tokio::task::spawn_blocking(move || {
        for i in (0..30).step_by(2) {
            let _ = std::fs::remove_dir_all(repos.join(format!("r{i:02}")));
        }
    })
    .await
    .unwrap();

    let settled = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(Event::ScanDone { scan: done, .. }) = events.recv().await {
                if done == scan {
                    return;
                }
            }
        }
    })
    .await;
    assert!(settled.is_ok(), "the scan never settled");

    // And shutdown returns, which is the consequence that actually bit.
    let stopped = tokio::time::timeout(Duration::from_secs(30), engine.shutdown()).await;
    assert!(stopped.is_ok(), "shutdown hung");
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_repo_re_reads_one_repository() {
    // What the Refresh control calls.
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 3);
    advance(&c);

    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap().snapshots;
    assert!(snaps.iter().all(|s| s.upstream.as_ref().and_then(|u| u.behind()) == Some(0)));

    // Fetch behind the engine's back, so only a refresh can notice.
    let target = c.repos.join("r00");
    git(&target, &["fetch"]);
    let id = snaps.iter().find(|s| s.path == target).unwrap().id.clone();

    h.refresh_repo(id.clone()).await.unwrap();
    let updated = wait_until(&h, Duration::from_secs(10), |snaps| {
        snaps.iter().find(|s| s.id == id).and_then(|s| s.upstream.as_ref().and_then(|u| u.behind()))
            == Some(1)
    })
    .await;
    assert!(updated, "refresh_repo did not re-read the repository");

    // ...and only that one.
    let others = h.snapshot().await.unwrap();
    for s in others.iter().filter(|s| s.id != id) {
        assert_eq!(s.upstream.as_ref().and_then(|u| u.behind()), Some(0), "{}", s.path.display());
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refreshing_a_repository_the_engine_does_not_know_is_a_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();
    // Must not hang, panic, or leave the engine un-idle.
    h.refresh_repo(git_scylla_core::RepoId::from_canonical("/nope/nope")).await.unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(10), engine.shutdown()).await;
    assert!(stopped.is_ok(), "an unknown refresh left the engine busy");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_directory_is_reported_rather_than_looking_like_an_empty_root() {
    // The signal the Full Disk Access hint is built on. An unsigned build
    // scanning a protected directory finds nothing, and that
    // is indistinguishable from an empty working set unless the walk says what
    // it could not read.
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let root = dir.join("root");
    let locked = root.join("locked");
    std::fs::create_dir_all(locked.join("inner")).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    let outcome = h.scan_to_completion(vec![root.clone()], false).await.unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(outcome.snapshots.is_empty(), "nothing readable to find");
    assert!(
        outcome.errors.iter().any(|e| matches!(
            e,
            git_scylla_discovery::DiscoveryError::Unreadable { path, .. } if path.ends_with("locked")
        )),
        "an empty result with no explanation is the failure this prevents: {:?}",
        outcome.errors
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_genuinely_empty_root_reports_no_errors() {
    // The other half: nothing found under a readable, empty directory is not a
    // problem, and dressing it up as one would make the hint noise.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let root = dir.join("empty");
    std::fs::create_dir_all(root.join("just/plain/folders")).unwrap();

    let engine = Engine::start(config());
    let outcome = engine.handle().scan_to_completion(vec![root], false).await.unwrap();
    assert!(outcome.snapshots.is_empty());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_event_stream_alone_is_enough_to_build_the_drawer() {
    // Stated as a property: a consumer that subscribes and then never asks the
    // engine another question must still be able to show every repository in
    // the batch, attribute it, and watch it move.
    //
    // Two things make that true and both are easy to lose. Queued jobs announce
    // themselves, so the drawer starts full rather than filling in as the
    // scheduler lets work through. And every job event carries its batch, so the
    // events emitted *during* `start_batch` — before it has returned an id to
    // attribute them to — are not homeless.
    let tmp = tempfile::tempdir().unwrap();
    let c = clones(tmp.path(), 3);
    advance(&c);
    for name in ["r00", "r01", "r02"] {
        git(&c.repos.join(name), &["fetch"]);
    }
    // One repository cannot be pulled, so the batch carries both shapes.
    std::fs::write(c.repos.join("r00/a.txt"), "changed\n").unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![c.repos.clone()], false).await.unwrap();

    let mut events = h.subscribe();
    let p = h.plan(Action::Pull { mode: PullMode::Rebase }, Selection::All).await.unwrap();
    assert_eq!(p.eligible.len(), 2, "two behind and clean");
    assert_eq!(p.skipped.len(), 1, "one dirty");

    // Deliberately started *without* reading the returned id first: everything
    // below is reconstructed from events only.
    let started = h.start_batch(p, JobOrigin::User).await.unwrap();

    let mut rows: HashMap<git_scylla_core::JobId, (RepoId, JobState)> = HashMap::new();
    let mut batches: HashSet<git_scylla_core::BatchId> = HashSet::new();
    let mut seen_queued = 0;
    let summary;
    loop {
        match events.recv().await.unwrap() {
            Event::JobStateChanged { id, batch, origin, repo, state } => {
                assert_eq!(origin, JobOrigin::User);
                batches.insert(batch.expect("a batch job says which batch"));
                if state == JobState::Queued {
                    assert!(!rows.contains_key(&id), "{id} announced itself twice");
                    seen_queued += 1;
                }
                rows.insert(id, (repo, state));
            }
            Event::BatchDone { id, summary: s } if id == started => {
                summary = s;
                break;
            }
            _ => {}
        }
    }

    // Every repository has a row, from events alone.
    assert_eq!(rows.len(), 3);
    let repos: HashSet<&RepoId> = rows.values().map(|(r, _)| r).collect();
    assert_eq!(repos.len(), 3, "one row per repository, not per state change");
    assert_eq!(batches, HashSet::from([started]), "every row attributed to the batch");
    assert_eq!(seen_queued, 2, "the two eligible jobs announced themselves before running");

    // ...and the rows agree with the summary the banner is drawn from.
    let s = summary;
    assert_eq!(s.ok, 2);
    assert_eq!(s.skipped, 1);
    assert_eq!(s.failed, 0);
    let states: Vec<&JobState> = rows.values().map(|(_, s)| s).collect();
    assert_eq!(states.iter().filter(|s| ***s == JobState::Ok).count(), s.ok);
    assert_eq!(states.iter().filter(|s| matches!(s, JobState::Skipped { .. })).count(), s.skipped);
    engine.shutdown().await;
}
