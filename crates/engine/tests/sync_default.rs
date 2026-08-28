//! Syncing the default branch: five git invocations behaving as one.
//!
//! The tests that matter here are about the *cleanup*, not the pull. A sync
//! that works is a pull with extra steps; a sync that is worth shipping is one
//! that puts the user back on their branch with their work restored **whether
//! or not** the pull succeeded, and that refuses rather than guesses when it
//! cannot tell which branch to visit.

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

/// An upstream on `trunk`, and a clone of it parked on a dirty `feature`.
///
/// `trunk` is a parameter because the whole point of the action is that the
/// default is not uniform across a working set — one repository calling it
/// `master` is the case a fixture named `main` would never catch.
fn origin_and_clone(dir: &Path, name: &str, trunk: &str) -> (PathBuf, PathBuf) {
    let origin = dir.join(format!("{name}.git"));
    std::fs::create_dir_all(&origin).unwrap();
    git(dir, &["init", "--bare", "-b", trunk, &format!("{name}.git")]);

    // A working copy of the upstream, used only to publish commits to it.
    let seed = dir.join(format!("{name}-seed"));
    git(dir, &["clone", origin.to_str().unwrap(), seed.to_str().unwrap()]);
    commit(&seed, "a.txt", "one\n", "c1");
    git(&seed, &["push", "origin", trunk]);

    let clone = dir.join("repos").join(name);
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    git(dir, &["clone", origin.to_str().unwrap(), clone.to_str().unwrap()]);

    // The user is somewhere else, with work in progress on a tracked file.
    git(&clone, &["checkout", "-b", "feature"]);
    std::fs::write(clone.join("a.txt"), "work in progress\n").unwrap();

    // ...and the upstream has moved on since.
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

    // The point of the whole thing: `main` advanced, and the user is exactly
    // where they were, with the work they had.
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
    // Compensation used to run only when a later step failed. A sync's last two
    // steps are owed whether or not the pull worked, and a transcript showing
    // the switch to `main` without the switch back would be describing a
    // repository the user has been left in.
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
    // Reverse order: undo the switch before putting the work back, or the work
    // lands on the branch the user was moved off.
    assert_eq!(cleanup, vec![vec!["checkout", "feature"], vec!["stash", "pop"]]);
    assert!(steps.iter().all(|s| s.state == StepState::Ok), "{steps:#?}");

    // And it is in the transcript, so a reader can see where they were left.
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
    // The failure path, and the reason this is one action rather than five.
    // `main` here has a local commit the upstream does not, so `--ff-only`
    // refuses — and the user must not be left standing on it with their work in
    // a stash they did not take.
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

    // Failed, and yet the repository is untouched from the user's side.
    assert_eq!(git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature");
    assert_eq!(std::fs::read_to_string(clone.join("a.txt")).unwrap(), "work in progress\n");
    assert_eq!(git(&clone, &["stash", "list"]), "");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn each_repository_gets_its_own_default_branch_and_the_plan_says_which() {
    // `main` versus `master` is not uniform across a working set, and a plan
    // that showed one command for both would be wrong for one of them.
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
    // Not in the headline: naming one of them there would be wrong for the
    // other. The resolved-command list is what carries it.
    assert!(!rendered.lines().next().unwrap().contains("master"), "{rendered}");

    let jobs = run(&h, plan).await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{jobs:#?}");
    assert_eq!(git(&a, &["rev-parse", "main"]), git(&a, &["rev-parse", "origin/main"]));
    assert_eq!(git(&b, &["rev-parse", "master"]), git(&b, &["rev-parse", "origin/master"]));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_with_no_recognisable_default_is_refused_by_name() {
    // No `origin/HEAD`, and a trunk called neither `main` nor `master`. The
    // tool does not know where to go, and guessing across a working set is not
    // a mistake worth making to save the user reading one line.
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
    // Found by running the CLI over four real repositories, not by reading.
    //
    // With no switch to make, the stash has nothing to clear out of the way,
    // and stashing purely so a pull can run on a dirty tree is `pull
    // --autostash` — refused by name everywhere else in this tool. It is also
    // the *only* arrangement in which the pop can conflict, because every other
    // one puts the work back on the branch it came from. The symptom was
    // conflict markers in a tracked file and a job that said `ok`.
    //
    // Driven through a fake since the rule became reachable that way: it is
    // `default == back_to && !is_clean()` over a snapshot and one ref answer,
    // and nothing in it needs a real clone to be true.
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
    // The other half: refusing the dirty case must not refuse the clean one,
    // which is an ordinary and useful thing to ask for.
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
    // The general half of the same discovery. Every forward step can succeed
    // and still leave the user somewhere they have to repair — here by deleting
    // the branch out from under the return switch, so the switch back fails and
    // the pop behind it is skipped. "Succeeded" is not a thing to say about
    // that, whatever the pull did.
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = origin_and_clone(tmp.path(), "r0", "main");
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root.clone()], false).await.unwrap();
    let mut plan = h.plan(SYNC, Selection::All).await.unwrap();
    // Resolve against a branch that will not be there. Rewriting the plan is
    // the only way to arrange this without a race, and it is exactly what a
    // branch deleted between the plan and the execution would look like.
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
    // The work is still on the stack, which is where the transcript says it is.
    assert_ne!(git(&clone, &["stash", "list"]), "");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn origin_head_beats_the_fallback() {
    // The fallback exists for repositories set up by hand, which never get an
    // `origin/HEAD`. It must not override one that is present: a repository
    // with both a `main` branch and an `origin/HEAD` pointing at `trunk` is
    // told by git itself which one is the default.
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
        // The stash is there because the fixture leaves work in progress; what
        // this asserts is the branch, which is `trunk` and not the `main` the
        // fallback would have picked.
        "git stash push && git checkout trunk && git pull --ff-only --no-autostash"
    );
    engine.shutdown().await;
}

// ---- resolution without a filesystem -----------------------------------
//
// These drive the engine through a `FakeProbe`. The repositories are empty
// `.git` directories and every fact — the snapshot, the default branch, whether
// the git directory can be read at all — comes from the fake.
//
// What is left to test that way is the engine's half: how a ref answer becomes
// a resolved action or a named skip. That half has nothing to learn from a real
// clone, and until resolution went through the seam it could not be reached
// without one.

/// The scan root for a fake working set.
///
/// Canonicalized, because `RepoId` is: on macOS a temp dir is `/var/…`, a
/// symlink to `/private/var/…`, and an uncanonicalized path would never match
/// the id discovery reports.
fn fake_root(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().canonicalize().unwrap().join("repos")
}

/// An engine whose probe answers for exactly these repositories, scanned.
async fn fake_engine(root: &Path, repos: Vec<FakeRepo>) -> (Engine, EngineHandle) {
    let probe = Arc::new(repos.into_iter().fold(FakeProbe::new(), FakeProbe::with));
    probe.scaffold().unwrap();
    let engine = Engine::with_probe(config(), probe);
    let h = engine.handle();
    h.scan_to_completion(vec![root.to_path_buf()], false).await.unwrap();
    (engine, h)
}

#[tokio::test(flavor = "multi_thread")]
async fn each_repository_gets_its_own_default_branch_without_a_filesystem() {
    // The same claim as `each_repository_gets_its_own_default_branch_and_the_
    // plan_says_which`, made without git: two repositories, two different
    // trunks, one plan, no `git init` and no subprocess.
    //
    // This is the test that could not have been written before resolution went
    // through the seam. It needs nothing but a substitutable probe.
    let tmp = tempfile::tempdir().unwrap();
    let root = fake_root(&tmp);
    let (engine, h) = fake_engine(
        &root,
        vec![
            // Parked on `feature`, as the real fixture's clones are: the user
            // is somewhere else, which is the case the action exists for. On
            // the default branch already there is no switch to plan, and this
            // test is about the switch.
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
    // Not in the headline, for the same reason as the real-git version: naming
    // one of them there would be wrong for the other.
    assert!(!rendered.lines().next().unwrap().contains("master"), "{rendered}");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trunk_the_probe_cannot_name_is_refused_by_name() {
    // The engine's half of `a_repository_with_no_recognisable_default_is_
    // refused_by_name`, which keeps its real clone because what it proves is
    // that *the probe* returns nothing for a repository with no `origin/HEAD`
    // and a trunk called neither `main` nor `master`.
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
    // The distinction the seam was widened to draw, and the reason a read
    // failure is not an answer of "no".
    //
    // Reading `refs/` used to swallow its errors, so an unreadable git
    // directory arrived as `None` and the plan told the user this repository
    // has no default branch — a sentence that may be plainly false about a
    // repository that has one. Unknown is not no, and the remedy differs:
    // `SnapshotStale` says refresh and try again.
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
