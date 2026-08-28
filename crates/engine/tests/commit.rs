//! Commit.
//!
//! The interesting half is the *plan*: one template becomes thirty-one distinct
//! messages, and the user has to see them before anything writes history.

use git_scylla_core::{Action, JobOrigin, JobState, ResetMode, SkipReason};
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

/// `n` repositories, each with one commit and one modified tracked file.
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
        std::fs::write(p.join("a.txt"), format!("changed {i}\n")).unwrap();
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

fn commit(message: &str, stage_all: bool) -> Action {
    Action::Commit { message: message.into(), stage_all, no_verify: false }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_template_produces_one_distinct_message_per_repository() {
    // Ten repositories, ten distinct correct messages, every one visible in
    // the plan before execution.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 10);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();

    let plan = h.plan(commit("chore({repo}): sync {branch}", true), Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 10);

    // Resolved per repository, and it is the resolved value the executor
    // runs.
    for (id, action) in &plan.eligible {
        let Action::Commit { message, .. } = action else { panic!("{action:?}") };
        assert_eq!(*message, format!("chore({}): sync main", id.name()));
    }

    // ...and the plan shows all ten, because ten distinct resolved commands is
    // exactly what `action_variants` reports.
    let view = plan.view();
    assert_eq!(view.variants.len(), 10, "the plan hid the messages it would write");
    assert!(view.variants.iter().any(|v| v.command.contains("chore(r00): sync main")));

    let jobs = run(&h, plan).await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{:?}", jobs[0].log);
    for i in 0..10 {
        let name = format!("r{i:02}");
        assert_eq!(
            git(&root.join(&name), &["log", "-1", "--pretty=%s"]),
            format!("chore({name}): sync main")
        );
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_message_is_not_dressed_up_as_thirty_one_variants() {
    // One resolved command is what the headline already says; listing it again
    // per repository would be noise proportional to the working set.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 3);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let view = h.plan(commit("a plain message", true), Selection::All).await.unwrap().view();
    assert!(view.variants.is_empty());
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_all_says_how_many_untracked_files_it_would_sweep_up() {
    // `git add -A` includes untracked files and `git commit -a` does not. The
    // count is a fact about the working set rather than about the action, which
    // is why the action's own words cannot carry it.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2);
    std::fs::create_dir_all(root.join("r00/build")).unwrap();
    std::fs::write(root.join("r00/build/out.o"), "binary\n").unwrap();
    std::fs::write(root.join("r01/notes.txt"), "notes\n").unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let view = h.plan(commit("m", true), Selection::All).await.unwrap().view();
    let warning = view.warning.expect("stage_all with untracked files must warn");
    assert!(warning.contains('2'), "{warning}");
    assert!(warning.contains("untracked"), "{warning}");

    // ...and without `stage_all` there is nothing to warn about, because `-A`
    // is the whole reason untracked files are in scope.
    let quiet = h.plan(commit("m", false), Selection::All).await.unwrap().view();
    assert_eq!(quiet.warning, None);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_with_nothing_to_commit_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2);
    git(&root.join("r00"), &["checkout", "--", "a.txt"]);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let plan = h.plan(commit("m", true), Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 1);
    assert!(plan
        .skipped
        .iter()
        .any(|(id, why)| id.name() == "r00" && *why == SkipReason::UpToDate));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hooks_run_by_default_and_no_verify_says_so_in_the_plan() {
    // A `pre-commit` that refuses a secret is doing the job it was installed
    // for, so bypassing it is opt-in and visible.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1);
    let hook = root.join("r00/.git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'the hook says no' >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();

    let jobs = run(&h, h.plan(commit("m", true), Selection::All).await.unwrap()).await;
    assert!(matches!(jobs[0].state, JobState::Failed { .. }), "the hook did not run");

    // The hook's own words, classified as `Unknown` — and that is the honest
    // answer rather than a gap. Git prints *nothing* of its own when a local
    // hook exits non-zero: the hook's output is the entire failure. There is no
    // marker to match on, and labelling every unrecognised `Commit` failure as
    // a hook rejection would be the tool guessing. `HookRejected` is kept for
    // the cases that do announce themselves, which are the remote ones.
    //
    // What matters is that the message reaches the user, and it does.
    let e = git_scylla_core::explain(&jobs[0].log).expect("an explanation");
    assert_eq!(e.kind, git_scylla_core::FailureKind::Unknown);
    assert_eq!(e.evidence, "the hook says no");

    // Opt out, and say so where the user reads before pressing anything.
    let bypass = Action::Commit { message: "m".into(), stage_all: true, no_verify: true };
    let view = h.plan(bypass.clone(), Selection::All).await.unwrap().view();
    assert!(view.headline.contains("no hooks"), "{}", view.headline);
    let jobs = run(&h, h.plan(bypass, Selection::All).await.unwrap()).await;
    assert_eq!(jobs[0].state, JobState::Ok, "{:?}", jobs[0].log);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn undoing_a_commit_keeps_the_work_and_stages_it() {
    // The difference from a pull's undo: soft, not hard. The user asked for
    // the commit to go away, not for the work in it to go away.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();

    let before = git(&root.join("r00"), &["rev-parse", "HEAD"]);
    let mut events = h.subscribe();
    let plan = h.plan(commit("committed by the tool", true), Selection::All).await.unwrap();
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id, .. }) if id == batch) {}
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let undo = h.plan_undo(batch).await.unwrap();
    assert_eq!(undo.eligible.len(), 2, "{:?}", undo.skipped);
    for (_, action) in &undo.eligible {
        assert!(matches!(action, Action::Reset { mode: ResetMode::Soft, .. }), "{action:?}");
    }
    let id = h.start_undo(batch, undo).await.unwrap();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id: b, .. }) if b == id) {}

    let repo = root.join("r00");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before, "HEAD did not return");
    // The work survived, and it is staged — which is what `--soft` buys.
    assert_eq!(git(&repo, &["diff", "--cached", "--name-only"]), "a.txt");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "changed 0\n");
    engine.shutdown().await;
}
