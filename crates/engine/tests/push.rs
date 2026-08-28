//! Push.
//!
//! The stakes are higher than for any other action: a push the remote accepts
//! cannot be undone, and a force-with-lease that overwrites is somebody else's
//! work. So most of this is about what the tool refuses, and what it makes the
//! user do first.

use git_scylla_core::{explain, Action, FailureKind, JobOrigin, JobState, SkipReason};
use git_scylla_engine::{
    Config, ConfirmGuard, Engine, EngineHandle, Event, Plan, Policy, Selection,
};
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

struct World {
    repos: PathBuf,
}

/// `n` clones of a shared bare origin, each with one unpushed commit.
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
        let name = format!("r{i:02}");
        git(&repos, &["clone", origin.to_str().unwrap(), &name]);
        let repo = repos.join(&name);
        std::fs::write(repo.join("mine.txt"), format!("{i}\n")).unwrap();
        git(&repo, &["add", "mine.txt"]);
        git(&repo, &["commit", "-m", "local work"]);
    }
    World { repos }
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

async fn run(h: &EngineHandle, plan: Plan) -> Vec<git_scylla_core::Job> {
    let mut events = h.subscribe();
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id, .. }) if id == batch) {}
    h.jobs(batch).await.unwrap()
}

// ---- the ordinary case -------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_push_publishes_local_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap().snapshots;
    assert!(snaps.iter().all(|s| s.upstream.as_ref().unwrap().ahead() == Some(1)));

    let plan = h
        .plan(Action::Push { set_upstream: None, force_with_lease: false }, Selection::All)
        .await
        .unwrap();
    // Only one can win: they share an origin and the second is then behind.
    assert_eq!(plan.eligible.len(), 3);
    let jobs = run(&h, plan).await;
    assert_eq!(jobs.iter().filter(|j| j.state == JobState::Ok).count(), 1);

    // ...and the losers say what to do about it, rather than printing git's
    // `! [rejected]` at somebody who already knows what it means.
    for job in jobs.iter().filter(|j| matches!(j.state, JobState::Failed { .. })) {
        let e = explain(&job.log).expect("a failed push explains itself");
        assert_eq!(e.kind, FailureKind::NonFastForward, "{}", e.evidence);
        assert_eq!(e.remedy.as_deref(), Some("pull first, then push again"));
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn worktree_dirtiness_is_irrelevant_to_a_push() {
    // Explicitly not a precondition: what is being published is committed, and
    // what is in the worktree has nothing to do with it.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    std::fs::write(w.repos.join("r00/scratch.txt"), "not committed\n").unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap().snapshots;
    assert!(!snaps[0].is_clean(), "the fixture is not dirty, so this proves nothing");

    let plan = h
        .plan(Action::Push { set_upstream: None, force_with_lease: false }, Selection::All)
        .await
        .unwrap();
    assert_eq!(plan.eligible.len(), 1, "{:?}", plan.skipped);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_branch_with_nothing_to_publish_is_skipped_rather_than_pushed() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    git(&w.repos.join("r00"), &["push", "origin", "main"]);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();
    let plan = h
        .plan(Action::Push { set_upstream: None, force_with_lease: false }, Selection::All)
        .await
        .unwrap();
    assert!(plan.is_empty());
    assert_eq!(plan.skipped[0].1, SkipReason::UpToDate);
    engine.shutdown().await;
}

// ---- the dangerous case ------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_force_with_lease_batch_cannot_be_confirmed_by_pressing_one_key() {
    // Not a checkbox and not a second button — both are muscle memory after the
    // third time. Typing a number that changes with the selection is the
    // cheapest thing that cannot be done without reading the plan.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    let view = h
        .plan(Action::Push { set_upstream: None, force_with_lease: true }, Selection::All)
        .await
        .unwrap()
        .view();
    assert_eq!(view.confirm_guard, Some(ConfirmGuard::TypeCount(3)));
    assert!(view.headline.contains("force-with-lease"), "{}", view.headline);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ordinary_push_is_not_dressed_up_as_dangerous() {
    // Danger styling that appears on ordinary work is wallpaper within a week,
    // and then it is not there on the day it matters.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    for action in [
        Action::Push { set_upstream: None, force_with_lease: false },
        Action::Push { set_upstream: Some("origin".into()), force_with_lease: false },
        Action::Fetch { prune: true, tags: false },
    ] {
        let view = h.plan(action.clone(), Selection::All).await.unwrap().view();
        assert_eq!(view.confirm_guard, None, "{action:?} was dressed up as dangerous");
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lease_against_a_stale_tracking_ref_is_refused() {
    // A lease is only a lease if the remote-tracking ref is recent. Against an
    // old one it is a force push with extra steps.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    // No fetch has ever happened here beyond the clone, and the policy's window
    // is minutes — so age it by making the window zero.
    let engine = Engine::start(Config {
        policy: Policy { max_lease_age: Duration::ZERO, ..config().policy },
        ..config()
    });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    let plan = h
        .plan(Action::Push { set_upstream: None, force_with_lease: true }, Selection::All)
        .await
        .unwrap();
    assert!(plan.is_empty(), "a stale lease was accepted");
    assert_eq!(plan.skipped[0].1, SkipReason::SnapshotStale);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diverged_branch_needs_the_lease_to_be_offered_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 2);
    // r00 wins the race; r01 is now diverged.
    git(&w.repos.join("r00"), &["push", "origin", "main"]);
    git(&w.repos.join("r01"), &["fetch"]);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    let plain = h
        .plan(Action::Push { set_upstream: None, force_with_lease: false }, Selection::All)
        .await
        .unwrap();
    assert!(
        plain.skipped.iter().any(|(id, why)| id.name() == "r01" && *why == SkipReason::Diverged),
        "{:?}",
        plain.skipped
    );

    let leased = h
        .plan(Action::Push { set_upstream: None, force_with_lease: true }, Selection::All)
        .await
        .unwrap();
    assert!(leased.eligible.iter().any(|(id, _)| id.name() == "r01"));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn setting_an_upstream_resolves_the_remote_per_repository() {
    // The remote is resolved per repository, and the plan shows what it
    // resolved to rather than the template.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    let repo = w.repos.join("r00");
    git(&repo, &["checkout", "-b", "side"]);
    std::fs::write(repo.join("side.txt"), "side\n").unwrap();
    git(&repo, &["add", "side.txt"]);
    git(&repo, &["commit", "-m", "on a branch with no upstream"]);

    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap().snapshots;
    assert!(snaps[0].upstream.is_none(), "the fixture branch already tracks something");

    let plan = h
        .plan(
            Action::Push { set_upstream: Some("origin".into()), force_with_lease: false },
            Selection::All,
        )
        .await
        .unwrap();
    assert_eq!(plan.eligible.len(), 1, "{:?}", plan.skipped);
    let jobs = run(&h, plan).await;
    assert_eq!(jobs[0].state, JobState::Ok, "{:?}", jobs[0].log);
    assert_eq!(git(&repo, &["rev-parse", "--abbrev-ref", "side@{upstream}"]), "origin/side");
    engine.shutdown().await;
}

// ---- the rule that has no code path ------------------------------------

#[test]
fn force_appears_nowhere_in_the_shipped_argv() {
    // Stronger than a flag defaulting to false: there is no code path to
    // `--force`, so no combination of options can reach one.
    use git_scylla_core::{PullMode, ResetMode};
    let every = [
        Action::Fetch { prune: true, tags: true },
        Action::Pull { mode: PullMode::Rebase },
        Action::Push { set_upstream: None, force_with_lease: false },
        Action::Push { set_upstream: Some("origin".into()), force_with_lease: true },
        Action::Checkout { rev: "main".into(), create: false },
        Action::Commit { message: "m".into(), stage_all: true, no_verify: false },
        Action::Stash { include_untracked: true },
        Action::StashPop,
        Action::Reset {
            to: git_scylla_core::Oid::parse("0123456").unwrap(),
            mode: ResetMode::Hard,
        },
    ];
    for action in every {
        for step in action.steps() {
            assert!(
                !step.argv.iter().any(|a| a == "--force" || a == "-f"),
                "{action:?} can emit a bare force"
            );
        }
    }
}
