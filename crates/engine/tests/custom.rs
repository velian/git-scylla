//! Custom commands.
//!
//! The deliberate escape hatch from the closed `Action` enum, which will not
//! cover everything. The alternative is the user dropping to a shell loop with
//! no plan, no transcript and no per-repository results. What it must never
//! become is a place where the tool pretends to know things it does not.

use git_scylla_core::{Action, JobOrigin, JobState, SkipReason};
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

fn custom(args: &[&str]) -> Action {
    Action::Custom {
        args: args.iter().map(|s| s.to_string()).collect(),
        network: false,
        mutating: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_custom_command_runs_the_exact_argv_the_plan_showed() {
    // The exact argv, no shell, a full transcript.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 3);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();

    let action = custom(&["config", "--local", "scylla.test", "set-by-custom"]);
    let plan = h.plan(action.clone(), Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 3);

    // The plan shows what will run, verbatim — the same three strings the
    // process gets and the transcript records.
    let view = plan.view();
    assert!(
        view.headline.contains("git config --local scylla.test set-by-custom"),
        "{}",
        view.headline
    );

    let jobs = run(&h, plan).await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{:?}", jobs[0].log);
    for i in 0..3 {
        assert_eq!(
            git(&root.join(format!("r{i:02}")), &["config", "--local", "scylla.test"]),
            "set-by-custom"
        );
    }
    // ...and each job kept its own transcript, like any other.
    assert!(jobs.iter().all(|j| j.started_at.is_some() && j.finished_at.is_some()));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_argument_that_would_be_shell_syntax_is_one_argument() {
    // The reason this is an argv and never a command string. A shell would read
    // this as two commands; there is no shell.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();

    let danger = "value; rm -rf /";
    let jobs = run(
        &h,
        h.plan(custom(&["config", "--local", "scylla.test", danger]), Selection::All)
            .await
            .unwrap(),
    )
    .await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{:?}", jobs[0].log);
    assert_eq!(git(&root.join("r00"), &["config", "--local", "scylla.test"]), danger);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn it_cannot_be_confirmed_without_acknowledging_what_does_not_apply() {
    // The engine has no opinion about an arbitrary command, and the
    // confirmation says so rather than looking like every other action's.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let view = h.plan(custom(&["gc"]), Selection::All).await.unwrap().view();
    match view.confirm_guard {
        Some(ConfirmGuard::Acknowledge(what)) => {
            assert!(what.contains("preconditions"), "{what}");
            assert!(what.contains("undo"), "{what}");
        }
        other => panic!("{other:?}"),
    }
    // ...and the rationale is honest about what was not checked.
    assert_eq!(view.eligible.unwrap().detail, "no preconditions apply");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_universal_preconditions_apply() {
    // The engine cannot reason about an arbitrary command and must not pretend
    // to — so a dirty worktree, a detached HEAD and a missing upstream are all
    // none of its business here. An operation in progress is, because a
    // half-finished rebase makes every command's behaviour unpredictable.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 3);
    std::fs::write(root.join("r00/a.txt"), "dirty\n").unwrap();
    git(&root.join("r01"), &["checkout", "--detach"]);
    std::fs::write(
        root.join("r02/.git/MERGE_HEAD"),
        git(&root.join("r02"), &["rev-parse", "HEAD"]),
    )
    .unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let plan = h.plan(custom(&["gc"]), Selection::All).await.unwrap();
    let eligible: Vec<&str> = plan.eligible.iter().map(|(id, _)| id.name()).collect();
    assert_eq!(eligible, ["r00", "r01"], "{:?}", plan.skipped);
    assert!(matches!(plan.skipped[0].1, SkipReason::OperationInProgress(_)));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_definitions_own_flags_decide_the_semaphore_and_the_recording() {
    // The engine cannot know; whoever wrote the definition can. Its default
    // when nobody said is the conservative pair.
    let network = Action::Custom { args: vec!["gc".into()], network: true, mutating: true };
    let local = Action::Custom { args: vec!["gc".into()], network: false, mutating: false };
    assert!(network.is_network() && network.is_mutating());
    assert!(!local.is_network() && !local.is_mutating());

    // `mutating` is what decides whether `head_before` is recorded, and a
    // recorded one is what an undo would need — except that a custom command
    // never gets one.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let jobs = run(&h, h.plan(network, Selection::All).await.unwrap()).await;
    assert!(jobs[0].head_before.is_some(), "a mutating custom command recorded nothing");

    let jobs = run(&h, h.plan(local, Selection::All).await.unwrap()).await;
    assert!(jobs[0].head_before.is_none(), "a non-mutating one paid for a rev-parse");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_custom_command_is_never_undone() {
    // Being honest about what cannot be undone matters more than maximising
    // coverage: the tool has no idea what the command did.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let mut events = h.subscribe();
    let plan = h
        .plan(
            Action::Custom { args: vec!["gc".into()], network: false, mutating: true },
            Selection::All,
        )
        .await
        .unwrap();
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id, .. }) if id == batch) {}
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let undo = h.plan_undo(batch).await.unwrap();
    assert!(undo.is_empty());
    assert!(
        undo.skipped.iter().any(|(_, why)| why.to_string().contains("effects are unknown")),
        "{:?}",
        undo.skipped
    );
    engine.shutdown().await;
}
