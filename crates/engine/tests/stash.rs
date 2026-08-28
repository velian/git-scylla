//! Stash and pop.
//!
//! The interesting case is a pop that cannot apply. The tool detects it, leaves
//! the stash entry alone, and reports — it does not resolve the conflict, and it
//! does not second-guess git about what to keep.

use git_scylla_core::{explain, Action, FailureKind, JobOrigin, JobState, SkipReason};
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

/// `n` repositories with one commit and one modified tracked file.
fn repos(dir: &Path, n: usize) -> PathBuf {
    let repos = dir.join("repos");
    std::fs::create_dir_all(&repos).unwrap();
    let repos = repos.canonicalize().unwrap();
    for i in 0..n {
        let name = format!("r{i:02}");
        git(&repos, &["init", "-b", "main", &name]);
        let p = repos.join(&name);
        std::fs::write(p.join("a.txt"), "committed\n").unwrap();
        git(&p, &["add", "a.txt"]);
        git(&p, &["commit", "-m", "c1"]);
        std::fs::write(p.join("a.txt"), format!("work in progress {i}\n")).unwrap();
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

async fn settle(h: &EngineHandle) {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = h.snapshot().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stash_then_pop_is_the_workflow_this_exists_for() {
    // Stash all, do something, pop all. Three independent actions rather than
    // one macro, because each is separately useful and a macro would have to
    // decide what to do when the middle step fails.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 3);
    let engine = Engine::start(config());
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![root.clone()], false).await.unwrap().snapshots;
    assert!(snaps.iter().all(|s| !s.is_clean()));

    let jobs =
        run(&h, h.plan(Action::Stash { include_untracked: false }, Selection::All).await.unwrap())
            .await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{:?}", jobs[0].log);
    settle(&h).await;

    let after = h.snapshot().await.unwrap();
    assert!(after.iter().all(|s| s.is_clean()), "the stash left changes behind");
    // The count is on the snapshot, and so in the grid.
    assert!(after.iter().all(|s| s.stashes == 1));

    let jobs = run(&h, h.plan(Action::StashPop, Selection::All).await.unwrap()).await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{:?}", jobs[0].log);
    settle(&h).await;

    let back = h.snapshot().await.unwrap();
    assert!(back.iter().all(|s| !s.is_clean()), "the work did not come back");
    assert!(back.iter().all(|s| s.stashes == 0));
    for i in 0..3 {
        assert_eq!(
            std::fs::read_to_string(root.join(format!("r{i:02}/a.txt"))).unwrap(),
            format!("work in progress {i}\n")
        );
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_with_nothing_to_stash_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 2);
    git(&root.join("r00"), &["checkout", "--", "a.txt"]);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(Action::Stash { include_untracked: false }, Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 1);
    assert!(plan
        .skipped
        .iter()
        .any(|(id, why)| id.name() == "r00" && *why == SkipReason::UpToDate));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn untracked_only_is_stashable_only_when_untracked_are_included() {
    // `git stash push` without `-u` leaves untracked files alone, so a
    // repository with nothing but untracked files has nothing to stash.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1);
    git(&root.join("r00"), &["checkout", "--", "a.txt"]);
    std::fs::write(root.join("r00/new.txt"), "untracked\n").unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let without = h.plan(Action::Stash { include_untracked: false }, Selection::All).await.unwrap();
    assert!(without.is_empty(), "an untracked-only repository was offered a plain stash");
    let with = h.plan(Action::Stash { include_untracked: true }, Selection::All).await.unwrap();
    assert_eq!(with.eligible.len(), 1);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pop_that_cannot_apply_reports_and_keeps_the_stash() {
    // Detect, stop, report — never resolve. The entry stays, which is git's own
    // behaviour and not something to second-guess: the work in it is the only
    // copy.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1);
    let repo = root.join("r00");
    git(&repo, &["stash", "push"]);
    // The same file changed again, so the pop cannot apply cleanly.
    std::fs::write(repo.join("a.txt"), "different work\n").unwrap();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    let plan = h.plan(Action::StashPop, Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 1, "a dirty-but-unconflicted tree may attempt a pop");
    let jobs = run(&h, plan).await;
    assert!(matches!(jobs[0].state, JobState::Failed { .. }), "the pop should not have applied");

    // Reported in terms the user can act on...
    let e = explain(&jobs[0].log).expect("a failed pop explains itself");
    assert_eq!(e.kind, FailureKind::WouldOverwrite, "{}", e.evidence);
    assert_eq!(e.remedy.as_deref(), Some("commit or stash the changes first"));

    // ...git's own reassurance is in the transcript, one click away, so the
    // tool does not need to restate it.
    assert!(
        jobs[0].log.iter().any(|l| l.text.contains("stash entry is kept")),
        "the transcript lost git's own account of what it did"
    );

    // ...and the entry really is intact, which is the whole point.
    assert_eq!(git(&repo, &["stash", "list"]).lines().count(), 1);
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "different work\n");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conflicted_tree_is_refused_a_pop_before_it_is_attempted() {
    // "Clean enough" means no conflicts: popping onto merely-modified files
    // usually works and git says so clearly when it does not, but popping onto
    // an unresolved conflict cannot.
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1);
    let repo = root.join("r00");
    git(&repo, &["stash", "push"]);
    // Manufacture a conflict state without a real merge: an unmerged index
    // entry is what the probe reads.
    git(&repo, &["branch", "side"]);
    std::fs::write(repo.join("a.txt"), "ours\n").unwrap();
    git(&repo, &["commit", "-am", "ours"]);
    git(&repo, &["checkout", "side"]);
    std::fs::write(repo.join("a.txt"), "theirs\n").unwrap();
    git(&repo, &["commit", "-am", "theirs"]);
    let merge = Command::new("git")
        .args(["merge", "main"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(!merge.status.success(), "the fixture did not conflict");

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(Action::StashPop, Selection::All).await.unwrap();
    assert!(plan.is_empty());
    // An operation in progress outranks the conflict itself: a stopped merge is
    // the fact the user needs, and "conflicted files" would be true and less
    // useful.
    assert!(matches!(plan.skipped[0].1, SkipReason::OperationInProgress(_)), "{:?}", plan.skipped);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_with_no_stash_is_skipped_rather_than_attempted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repos(tmp.path(), 1);
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(Action::StashPop, Selection::All).await.unwrap();
    assert!(plan.is_empty());
    assert_eq!(plan.skipped[0].1, SkipReason::NoStash);
    engine.shutdown().await;
}
