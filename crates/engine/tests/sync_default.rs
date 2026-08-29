//! Syncing the default branch: five git invocations behaving as one.

use git_scylla_core::{Action, Head, JobOrigin, JobState, Pass, PullMode, SkipReason, StepState};
use git_scylla_engine::{Config, Engine, EngineHandle, Event, Plan, Policy, Selection};
use git_scylla_probe::{FakeProbe, FakeRepo};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
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

fn commit(repo: &Path, file: &str, body: &str, msg: &str) {
    std::fs::write(repo.join(file), body).unwrap();
    git(repo, &["add", file]);
    git(repo, &["commit", "-m", msg]);
}

fn origin_and_clone(dir: &Path, name: &str, trunk: &str) -> (PathBuf, PathBuf) {
    let origin = dir.join(format!("{name}.git"));
    std::fs::create_dir_all(&origin).unwrap();
    git(dir, &["init", "--bare", "-b", trunk, &format!("{name}.git")]);

    let seed = dir.join(format!("{name}-seed"));
    git(dir, &["clone", origin.to_str().unwrap(), seed.to_str().unwrap()]);
    commit(&seed, "a.txt", "one\n", "c1");
    git(&seed, &["push", "origin", trunk]);

    let clone = dir.join("repos").join(name);
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    git(dir, &["clone", origin.to_str().unwrap(), clone.to_str().unwrap()]);

    git(&clone, &["checkout", "-b", "feature"]);
    std::fs::write(clone.join("a.txt"), "work in progress\n").unwrap();

    commit(&seed, "a.txt", "one\ntwo\n", "c2");
    git(&seed, &["push", "origin", trunk]);

    (origin, clone)
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

const SYNC: Action = Action::SyncDefault { mode: PullMode::FfOnly, plan: None };

#[tokio::test(flavor = "multi_thread")]
async fn the_default_branch_moves_and_the_user_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = origin_and_clone(tmp.path(), "r0", "main");
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let jobs = run(&h, h.plan(SYNC, Selection::All).await.unwrap()).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, JobState::Ok, "{:#?}", jobs[0].log);

    assert_eq!(git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature");
    assert_eq!(std::fs::read_to_string(clone.join("a.txt")).unwrap(), "work in progress\n");
    assert_eq!(
        git(&clone, &["rev-parse", "main"]),
        git(&clone, &["rev-parse", "origin/main"]),
        "the default branch did not move"
    );
    assert_eq!(git(&clone, &["stash", "list"]), "", "the stash was not popped");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cleanup_runs_on_the_success_path_too() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = origin_and_clone(tmp.path(), "r0", "main");
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let jobs = run(&h, h.plan(SYNC, Selection::All).await.unwrap()).await;
    assert_eq!(jobs[0].state, JobState::Ok);

    let steps = &jobs[0].steps;
    let forward: Vec<Vec<String>> =
        steps.iter().filter(|s| s.pass == Pass::Forward).map(|s| s.step.argv.clone()).collect();
    let cleanup: Vec<Vec<String>> = steps
        .iter()
        .filter(|s| s.pass == Pass::Cleanup)
        .map(|s| s.step.compensate.clone().unwrap())
        .collect();

    assert_eq!(forward.len(), 3, "{forward:?}");
    assert_eq!(forward[0], ["stash", "push"]);
    assert_eq!(forward[1], ["checkout", "main"]);
    assert_eq!(forward[2], ["pull", "--ff-only", "--no-autostash"]);
    assert_eq!(cleanup, vec![vec!["checkout", "feature"], vec!["stash", "pop"]]);
    assert!(steps.iter().all(|s| s.state == StepState::Ok), "{steps:#?}");

    let log = h.job_log(jobs[0].id).await.unwrap();
    let text: Vec<&str> = log.iter().map(|l| l.text.as_str()).collect();
    assert!(
        text.iter().any(|l| l.contains("cleanup: git checkout feature")),
        "the return is not in the transcript: {text:#?}"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pull_that_cannot_fast_forward_still_hands_the_repository_back() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = origin_and_clone(tmp.path(), "r0", "main");
    git(&clone, &["checkout", "main"]);
    commit(&clone, "local.txt", "diverging\n", "local only");
    git(&clone, &["checkout", "feature"]);
    std::fs::write(clone.join("a.txt"), "work in progress\n").unwrap();
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let jobs = run(&h, h.plan(SYNC, Selection::All).await.unwrap()).await;
    assert!(matches!(jobs[0].state, JobState::Failed { .. }), "{:?}", jobs[0].state);

    assert_eq!(git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature");
    assert_eq!(std::fs::read_to_string(clone.join("a.txt")).unwrap(), "work in progress\n");
    assert_eq!(git(&clone, &["stash", "list"]), "");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn each_repository_gets_its_own_default_branch_and_the_plan_says_which() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, a) = origin_and_clone(tmp.path(), "r0-main", "main");
    let (_, b) = origin_and_clone(tmp.path(), "r1-master", "master");
    let root = a.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(SYNC, Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 2);

    let rendered = plan.render();
    assert!(rendered.contains("git checkout main"), "{rendered}");
    assert!(rendered.contains("git checkout master"), "{rendered}");
    assert!(!rendered.lines().next().unwrap().contains("master"), "{rendered}");

    let jobs = run(&h, plan).await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{jobs:#?}");
    assert_eq!(git(&a, &["rev-parse", "main"]), git(&a, &["rev-parse", "origin/main"]));
    assert_eq!(git(&b, &["rev-parse", "master"]), git(&b, &["rev-parse", "origin/master"]));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_with_no_recognisable_default_is_refused_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = origin_and_clone(tmp.path(), "r0", "trunk");
    std::fs::remove_file(clone.join(".git/refs/remotes/origin/HEAD")).unwrap();
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(SYNC, Selection::All).await.unwrap();
    assert!(plan.eligible.is_empty());
    assert_eq!(plan.skipped, vec![(plan.skipped[0].0.clone(), SkipReason::NoDefaultBranch)]);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn standing_on_the_default_branch_with_work_in_the_tree_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fake_root(&tmp);
    let (engine, h) = fake_engine(
        &root,
        vec![FakeRepo::new(root.join("r0")).default_branch("main").snapshot(|s| {
            // Standing on the default branch, with work in a tracked file.
            s.work.modified = 1;
        })],
    )
    .await;
    let plan = h.plan(SYNC, Selection::All).await.unwrap();
    assert!(plan.eligible.is_empty(), "{:?}", plan.eligible);
    assert_eq!(plan.skipped[0].1, SkipReason::DirtyWorktree);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn standing_on_a_clean_default_branch_is_simply_a_pull() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = origin_and_clone(tmp.path(), "r0", "main");
    git(&clone, &["checkout", "main"]);
    git(&clone, &["checkout", "--", "a.txt"]);
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(SYNC, Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 1);
    assert_eq!(
        plan.eligible[0].1.to_string(),
        "git pull --ff-only --no-autostash",
        "no stash and no switch: there is nothing to get out of the way"
    );
    let jobs = run(&h, plan).await;
    assert_eq!(jobs[0].state, JobState::Ok, "{:#?}", jobs[0].log);
    assert_eq!(git(&clone, &["rev-parse", "main"]), git(&clone, &["rev-parse", "origin/main"]));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_whose_cleanup_failed_does_not_report_success() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = origin_and_clone(tmp.path(), "r0", "main");
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();
    let mut plan = h.plan(SYNC, Selection::All).await.unwrap();
    plan.eligible[0].1 = Action::SyncDefault {
        mode: PullMode::FfOnly,
        plan: Some(git_scylla_core::SyncPlan {
            default: "main".into(),
            back_to: "gone".into(),
            stash: true,
        }),
    };

    let jobs = run(&h, plan).await;
    assert!(matches!(jobs[0].state, JobState::Failed { .. }), "{:?}", jobs[0].state);

    let cleanup: Vec<StepState> =
        jobs[0].steps.iter().filter(|s| s.pass == Pass::Cleanup).map(|s| s.state.clone()).collect();
    assert!(matches!(cleanup[0], StepState::Failed { .. }), "{cleanup:?}");
    assert_eq!(cleanup[1], StepState::NotRun, "the pop ran onto the wrong branch");
    assert_ne!(git(&clone, &["stash", "list"]), "");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn origin_head_beats_the_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = origin_and_clone(tmp.path(), "r0", "trunk");
    git(&clone, &["branch", "main", "origin/trunk"]);
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(SYNC, Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 1);
    assert_eq!(
        plan.eligible[0].1.to_string(),
        "git stash push && git checkout trunk && git pull --ff-only --no-autostash"
    );
    engine.shutdown().await;
}
fn fake_root(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().canonicalize().unwrap().join("repos")
}

async fn fake_engine(root: &Path, repos: Vec<FakeRepo>) -> (Engine, EngineHandle) {
    let probe = Arc::new(repos.into_iter().fold(FakeProbe::new(), FakeProbe::with));
    probe.scaffold().unwrap();
    let engine = Engine::with_probe(config(), probe);
    let h = engine.handle();
    h.scan_to_completion(vec![root.to_path_buf()], false).await.unwrap();
    (engine, h)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_in_flight_does_not_stall_the_actor() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fake_root(&tmp);
    let probe = Arc::new(
        FakeProbe::new().with(FakeRepo::new(root.join("a"))).slow_refs(Duration::from_millis(1500)),
    );
    probe.scaffold().unwrap();
    let engine = Engine::with_probe(config(), probe);
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();

    let planning = {
        let h = h.clone();
        tokio::spawn(async move {
            h.plan(Action::Checkout { rev: "main".into(), create: false }, Selection::All).await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;

    let start = std::time::Instant::now();
    let snaps = h.snapshot().await.unwrap();
    let served_in = start.elapsed();

    assert_eq!(snaps.len(), 1);
    assert!(
        served_in < Duration::from_millis(300),
        "the actor took {served_in:?} to answer a question it had the answer to, \
         so the ref read is still holding the loop"
    );
    assert!(!planning.await.unwrap().unwrap().is_empty(), "the plan still resolves");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn each_repository_gets_its_own_default_branch_without_a_filesystem() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fake_root(&tmp);
    let (engine, h) = fake_engine(
        &root,
        vec![
            FakeRepo::new(root.join("r0-main"))
                .default_branch("main")
                .snapshot(|s| s.head = Head::Branch("feature".into())),
            FakeRepo::new(root.join("r1-master"))
                .default_branch("master")
                .snapshot(|s| s.head = Head::Branch("feature".into())),
        ],
    )
    .await;

    let plan = h.plan(SYNC, Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 2, "{:?}", plan.skipped);
    let rendered = plan.render();
    assert!(rendered.contains("git checkout main"), "{rendered}");
    assert!(rendered.contains("git checkout master"), "{rendered}");
    assert!(!rendered.lines().next().unwrap().contains("master"), "{rendered}");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trunk_the_probe_cannot_name_is_refused_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fake_root(&tmp);
    let (engine, h) =
        fake_engine(&root, vec![FakeRepo::new(root.join("r0")).no_default_branch()]).await;

    let plan = h.plan(SYNC, Selection::All).await.unwrap();
    assert!(plan.eligible.is_empty(), "{:?}", plan.eligible);
    assert_eq!(plan.skipped[0].1, SkipReason::NoDefaultBranch);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_that_cannot_be_read_is_stale_rather_than_trunkless() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fake_root(&tmp);
    let (engine, h) = fake_engine(
        &root,
        vec![FakeRepo::new(root.join("readable")), FakeRepo::new(root.join("locked")).unreadable()],
    )
    .await;

    let plan = h.plan(SYNC, Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 1, "{:?}", plan.skipped);
    assert_eq!(plan.skipped.len(), 1);
    let (id, why) = &plan.skipped[0];
    assert!(id.path().ends_with("locked"), "{id:?}");
    assert_eq!(*why, SkipReason::SnapshotStale, "an unreadable repository is not a trunkless one");
    engine.shutdown().await;
}
