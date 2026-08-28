//! Undo.
//!
//! `reset --hard` destroying real work is the risk this whole task carries, so
//! most of what is asserted here is the *refusals*. Each guard gets its own
//! case, because a guard that is only covered incidentally is a guard that can
//! be deleted without anything going red.

use git_scylla_core::{Action, JobOrigin, JobState, PullMode, ResetMode, SkipReason};
use git_scylla_engine::{Config, Engine, EngineHandle, Event, Plan, Policy, Selection};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn git(cwd: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `n` clones, each one commit behind its origin, so a pull has work.
fn world(dir: &Path, n: usize) -> PathBuf {
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

/// Run a plan and wait for it to finish.
async fn run(h: &EngineHandle, plan: Plan) -> git_scylla_core::BatchId {
    let mut events = h.subscribe();
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id, .. }) if id == batch) {}
    batch
}

/// Pull everything, and return the batch.
async fn pull_all(h: &EngineHandle) -> git_scylla_core::BatchId {
    let plan = h.plan(Action::Pull { mode: PullMode::FfOnly }, Selection::All).await.unwrap();
    assert!(!plan.eligible.is_empty(), "the fixture gave the pull nothing to do");
    run(h, plan).await
}

/// Let the post-job re-probes land, so the undo plan reads current facts.
async fn settle(h: &EngineHandle) {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = h.snapshot().await;
}

fn reasons(plan: &Plan) -> Vec<String> {
    plan.skipped.iter().map(|(_, why)| why.to_string()).collect()
}

// ---- the repair --------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_pull_batch_can_be_undone_and_every_head_returns() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = world(tmp.path(), 3);
    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;
    let before: Vec<String> =
        snaps.iter().map(|s| git(s.path.as_path(), &["rev-parse", "HEAD"])).collect();

    let batch = pull_all(&h).await;
    settle(&h).await;
    let after: Vec<String> =
        snaps.iter().map(|s| git(s.path.as_path(), &["rev-parse", "HEAD"])).collect();
    assert_ne!(before, after, "the pull did not move anything, so there is nothing to undo");

    let plan = h.plan_undo(batch).await.unwrap();
    assert_eq!(plan.eligible.len(), 3, "{:?}", reasons(&plan));
    // Each repository resets to *its own* commit.
    for (_, action) in &plan.eligible {
        assert!(matches!(action, Action::Reset { mode: ResetMode::Hard, .. }), "{action:?}");
    }

    let undo = h.start_undo(batch, plan).await.unwrap();
    let mut events = h.subscribe();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id, .. }) if id == undo) {}

    let restored: Vec<String> =
        snaps.iter().map(|s| git(s.path.as_path(), &["rev-parse", "HEAD"])).collect();
    assert_eq!(restored, before, "HEAD did not return to where the batch started");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn undo_is_an_ordinary_plan_with_an_ordinary_confirmation() {
    // Not a special case, and it must not bypass the sheet. The confirmation
    // copy has to say what `--hard` does, in the headline, because that is the
    // line the user reads before pressing anything.
    let tmp = tempfile::tempdir().unwrap();
    let repos = world(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![repos.clone()], false).await.unwrap();
    let batch = pull_all(&h).await;
    settle(&h).await;

    let view = h.plan_undo(batch).await.unwrap().view();
    assert!(view.confirm_label.is_some(), "an undo with work to do must offer a control");
    let headline = view.headline.to_lowercase();
    assert!(headline.contains("undo"), "{}", view.headline);
    assert!(
        headline.contains("discards uncommitted work"),
        "the headline does not say what --hard does: {}",
        view.headline
    );
    // ...and the repositories it would touch are named, not just counted.
    assert_eq!(view.eligible.as_ref().unwrap().repos.len(), 2);
    engine.shutdown().await;
}

// ---- the guards --------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_that_went_dirty_after_the_batch_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = world(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;
    let batch = pull_all(&h).await;
    settle(&h).await;

    // The user got back to work. `--hard` here would throw away something that
    // has nothing to do with the batch.
    std::fs::write(snaps[0].path.join("mine.txt"), "work\n").unwrap();
    h.refresh_repo(snaps[0].id.clone()).await.unwrap();
    settle(&h).await;

    let plan = h.plan_undo(batch).await.unwrap();
    assert_eq!(plan.eligible.len(), 1, "{:?}", reasons(&plan));
    assert!(
        plan.skipped
            .iter()
            .any(|(id, why)| *id == snaps[0].id && *why == SkipReason::DirtyWorktree),
        "{:?}",
        reasons(&plan)
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_committed_on_top_of_is_refused() {
    // The guard `head_before` alone cannot express: a repository that has moved
    // has either been moved by this job, or moved by this job *and then*
    // committed on. Undoing the second discards work nobody asked to lose.
    let tmp = tempfile::tempdir().unwrap();
    let repos = world(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;
    let batch = pull_all(&h).await;
    settle(&h).await;

    let repo = snaps[0].path.clone();
    std::fs::write(repo.join("later.txt"), "after\n").unwrap();
    git(&repo, &["add", "later.txt"]);
    git(&repo, &["commit", "-m", "committed after the batch"]);
    h.refresh_repo(snaps[0].id.clone()).await.unwrap();
    settle(&h).await;

    let plan = h.plan_undo(batch).await.unwrap();
    assert!(
        plan.skipped.iter().any(|(id, why)| *id == snaps[0].id && *why == SkipReason::HeadMoved),
        "{:?}",
        reasons(&plan)
    );
    assert_eq!(plan.eligible.len(), 1);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_left_mid_operation_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = world(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;
    let batch = pull_all(&h).await;
    settle(&h).await;

    // A half-finished merge. Resetting out from under one leaves a repository
    // in a state the user cannot reason about.
    let repo = snaps[0].path.clone();
    std::fs::write(repo.join(".git/MERGE_HEAD"), git(&repo, &["rev-parse", "HEAD"])).unwrap();
    h.refresh_repo(snaps[0].id.clone()).await.unwrap();
    settle(&h).await;

    let plan = h.plan_undo(batch).await.unwrap();
    assert!(
        plan.skipped.iter().any(
            |(id, why)| *id == snaps[0].id && matches!(why, SkipReason::OperationInProgress(_))
        ),
        "{:?}",
        reasons(&plan)
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_batch_offers_nothing_to_undo() {
    // Being honest about what cannot be undone matters more than maximising
    // coverage: a fetch only advanced remote-tracking refs.
    let tmp = tempfile::tempdir().unwrap();
    let repos = world(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![repos.clone()], false).await.unwrap();

    let plan = h.plan(Action::Fetch { prune: false, tags: false }, Selection::All).await.unwrap();
    let batch = run(&h, plan).await;
    settle(&h).await;

    let plan = h.plan_undo(batch).await.unwrap();
    assert!(plan.is_empty());
    assert!(
        reasons(&plan).iter().all(|r| r.contains("remote-tracking refs")),
        "{:?}",
        reasons(&plan)
    );
    // An empty plan offers no control at all, rather than a disabled one whose
    // meaning the user has to work out.
    assert!(plan.view().confirm_label.is_none());
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_undo_is_never_itself_undone() {
    // One level, explicit, recent. A stack would need a history the tool
    // deliberately does not keep, and a second undo of the same work is a
    // `reset --hard` whose target nobody chose.
    let tmp = tempfile::tempdir().unwrap();
    let repos = world(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![repos.clone()], false).await.unwrap();
    let batch = pull_all(&h).await;
    settle(&h).await;

    let plan = h.plan_undo(batch).await.unwrap();
    let undo = h.start_undo(batch, plan).await.unwrap();
    let mut events = h.subscribe();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id, .. }) if id == undo) {}
    settle(&h).await;

    let second = h.plan_undo(undo).await.unwrap();
    assert!(second.is_empty(), "an undo offered an undo of itself");
    assert_eq!(second.considered, 0);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_the_engine_never_had_plans_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = world(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![repos], false).await.unwrap();

    let plan = h.plan_undo(git_scylla_core::BatchId(999)).await.unwrap();
    assert!(plan.is_empty());
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_failed_has_nothing_to_repair() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = world(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![repos.clone()], false).await.unwrap().snapshots;

    // A checkout of a ref that does not exist: mutating, so `head_before` is
    // recorded, but it changed nothing.
    let action = Action::Checkout { rev: "no-such-branch".into(), create: false };
    let plan = Plan {
        action: action.clone(),
        eligible: vec![(snaps[0].id.clone(), action)],
        skipped: vec![],
        considered: 1,
        warning: None,
    };
    let batch = run(&h, plan).await;
    assert!(h.jobs(batch).await.unwrap().iter().all(|j| j.state != JobState::Ok));
    settle(&h).await;

    let plan = h.plan_undo(batch).await.unwrap();
    assert!(plan.is_empty());
    assert!(reasons(&plan).iter().any(|r| r.contains("did not run")), "{:?}", reasons(&plan));
    engine.shutdown().await;
}
