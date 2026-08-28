//! Checkout and branch.

use git_scylla_core::{Action, JobOrigin, JobState, SkipReason};
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

/// `n` repositories on `main`; the first `with_release` also have `release`.
fn repos(dir: &Path, n: usize, with_release: usize) -> PathBuf {
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
        if i < with_release {
            git(&p, &["branch", "release"]);
        }
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

async fn run(
    h: &EngineHandle,
    plan: Plan,
) -> (git_scylla_core::BatchId, Vec<git_scylla_core::Job>) {
    let mut events = h.subscribe();
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id, .. }) if id == batch) {}
    (batch, h.jobs(batch).await.unwrap())
}

async fn settle(h: &EngineHandle) {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = h.snapshot().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bulk_checkout_succeeds_where_the_ref_exists_and_names_where_it_does_not() {
    // 20 repositories, 4 of them without the branch.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 20, 16);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();

    let plan = h
        .plan(Action::Checkout { rev: "release".into(), create: false }, Selection::All)
        .await
        .unwrap();
    assert_eq!(plan.eligible.len(), 16, "{:?}", plan.skipped);
    let missing: Vec<String> = plan
        .skipped
        .iter()
        .filter(|(_, why)| matches!(why, SkipReason::RefNotFound(_)))
        .map(|(id, _)| id.name().to_string())
        .collect();
    assert_eq!(missing, ["r16", "r17", "r18", "r19"], "the plan did not name what it missed");
    // ...and the reason says which ref, because "no such ref" alone is a dead
    // end when a plan covers twenty repositories.
    assert!(plan.skipped.iter().any(|(_, why)| why.to_string().contains("release")));

    let (_, jobs) = run(&h, plan).await;
    // A batch carries a job per *skip* as well, so the four named above are
    // here too, in `Skipped`. What must all be `Ok` is the sixteen that ran.
    let ran: Vec<_> =
        jobs.iter().filter(|j| !matches!(j.state, JobState::Skipped { .. })).collect();
    assert_eq!(ran.len(), 16);
    assert!(ran.iter().all(|j| j.state == JobState::Ok), "{:?}", ran[0].log);
    for i in 0..16 {
        assert_eq!(
            git(&root.join(format!("r{i:02}")), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "release"
        );
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revision_expression_is_let_through_rather_than_refused() {
    // `has_ref` declines to answer for anything carrying revision syntax, and
    // the plan lets those try. Refusing a checkout that would have worked is
    // worse than a job that fails with a good message.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2, 0);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let plan = h
        .plan(Action::Checkout { rev: "main~0".into(), create: false }, Selection::All)
        .await
        .unwrap();
    assert_eq!(plan.eligible.len(), 2, "a revision expression was refused: {:?}", plan.skipped);
    let (_, jobs) = run(&h, plan).await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{:?}", jobs[0].log);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_repository_is_refused_and_that_is_not_negotiable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2, 2);
    std::fs::write(root.join("r00/a.txt"), "mine\n").unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h
        .plan(Action::Checkout { rev: "release".into(), create: false }, Selection::All)
        .await
        .unwrap();
    assert!(plan
        .skipped
        .iter()
        .any(|(id, why)| id.name() == "r00" && *why == SkipReason::DirtyWorktree));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_branch_name_templates_per_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 3, 0);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();

    let plan = h
        .plan(Action::Branch { name: "wip/{repo}".into(), from: None }, Selection::All)
        .await
        .unwrap();
    assert_eq!(plan.eligible.len(), 3);
    let (_, jobs) = run(&h, plan).await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{:?}", jobs[0].log);
    for i in 0..3 {
        let name = format!("r{i:02}");
        let repo = root.join(&name);
        // Created, and *not* switched to — which is the difference from
        // `Checkout { create: true }`.
        assert_eq!(git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
        assert!(git(&repo, &["branch", "--list", &format!("wip/{name}")]).contains(&name));
    }
    engine.shutdown().await;
}

// ---- undoing a switch ---------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn undoing_a_checkout_switches_back_rather_than_resetting() {
    // The repair for a switch is a switch. Undoing "check out release" with
    // `reset --hard <previous tip>` would move *release itself* to where the
    // previous branch was — leaving a normal-looking branch that had silently
    // swallowed the other one's commits.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1, 1);
    let repo = root.join("r00");
    // Give the two branches different tips, so a reset would be visible.
    std::fs::write(repo.join("b.txt"), "on main\n").unwrap();
    git(&repo, &["add", "b.txt"]);
    git(&repo, &["commit", "-m", "main moves on"]);
    let main_tip = git(&repo, &["rev-parse", "main"]);
    let release_tip = git(&repo, &["rev-parse", "release"]);
    assert_ne!(main_tip, release_tip);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();

    let plan = h
        .plan(Action::Checkout { rev: "release".into(), create: false }, Selection::All)
        .await
        .unwrap();
    let (batch, jobs) = run(&h, plan).await;
    assert_eq!(jobs[0].state, JobState::Ok, "{:?}", jobs[0].log);
    assert_eq!(git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "release");
    settle(&h).await;

    let undo = h.plan_undo(batch).await.unwrap();
    assert_eq!(undo.eligible.len(), 1, "{:?}", undo.skipped);
    // A checkout, not a reset.
    assert!(
        matches!(&undo.eligible[0].1, Action::Checkout { rev, create: false } if rev == "main"),
        "{:?}",
        undo.eligible[0].1
    );

    let mut events = h.subscribe();
    let id = h.start_undo(batch, undo).await.unwrap();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id: b, .. }) if b == id) {}

    assert_eq!(git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    // The load-bearing assertion: neither branch moved.
    assert_eq!(git(&repo, &["rev-parse", "main"]), main_tip, "main was dragged by the undo");
    assert_eq!(git(&repo, &["rev-parse", "release"]), release_tip, "release moved");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkout_from_a_detached_head_has_no_branch_to_return_to() {
    // Honest refusal rather than a guess. There is no branch to switch back to,
    // and resetting is the wrong repair for a switch whatever the starting
    // point.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1, 1);
    let repo = root.join("r00");
    git(&repo, &["checkout", "--detach"]);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h
        .plan(Action::Checkout { rev: "release".into(), create: false }, Selection::All)
        .await
        .unwrap();
    let (batch, jobs) = run(&h, plan).await;
    assert_eq!(jobs[0].state, JobState::Ok, "{:?}", jobs[0].log);
    settle(&h).await;

    let undo = h.plan_undo(batch).await.unwrap();
    assert!(undo.is_empty());
    assert!(
        undo.skipped.iter().any(|(_, why)| why.to_string().contains("detached")),
        "{:?}",
        undo.skipped
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn creating_a_branch_is_not_undone() {
    // It moved nothing, so there is nothing for a reset to repair — and
    // deleting the branch is a different operation with its own hazards.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1, 0);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let plan =
        h.plan(Action::Branch { name: "wip".into(), from: None }, Selection::All).await.unwrap();
    let (batch, _) = run(&h, plan).await;
    settle(&h).await;

    let undo = h.plan_undo(batch).await.unwrap();
    assert!(undo.is_empty());
    assert!(
        undo.skipped.iter().any(|(_, why)| why.to_string().contains("moved nothing")),
        "{:?}",
        undo.skipped
    );
    engine.shutdown().await;
}
