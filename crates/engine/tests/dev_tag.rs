//! Cutting a dev tag across a working set.
//!
//! The arithmetic has its own tests in `core::version`, over names alone. What
//! is left to prove here is the part that touches a repository: that each one
//! is given the name derived from *its* tags rather than from the batch's, that
//! the tag lands on the remote, and — the one that decides the step order —
//! that a name somebody else already took leaves nothing behind.

use git_scylla_core::version::Bump;
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

/// A clone with an upstream, one commit, and whatever tags are asked for —
/// pushed, so the remote has them too.
fn repo(dir: &Path, name: &str, tags: &[&str]) -> (PathBuf, PathBuf) {
    let origin = dir.join(format!("{name}.git"));
    git(dir, &["init", "--bare", "-b", "main", &format!("{name}.git")]);
    let clone = dir.join("repos").join(name);
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    git(dir, &["clone", origin.to_str().unwrap(), clone.to_str().unwrap()]);
    std::fs::write(clone.join("a.txt"), "one\n").unwrap();
    git(&clone, &["add", "a.txt"]);
    git(&clone, &["commit", "-m", "c1"]);
    git(&clone, &["push", "origin", "main"]);
    for tag in tags {
        git(&clone, &["tag", tag]);
        git(&clone, &["push", "origin", tag]);
    }
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

fn dev_tag(push: Option<&str>) -> Action {
    Action::DevTag {
        channel: "dev".into(),
        bump: Bump::Minor,
        name: None,
        push: push.map(Into::into),
    }
}

/// The resolved command for one repository, by name.
fn command_for(plan: &Plan, repo: &str) -> String {
    plan.eligible
        .iter()
        .find(|(id, _)| id.name() == repo)
        .unwrap_or_else(|| panic!("{repo} is not eligible: {:?}", plan.skipped))
        .1
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_repository_gets_the_name_derived_from_its_own_tags() {
    // Three repositories at three different points in their own histories. A
    // plan that read "create the next dev tag" would be unreviewable.
    let tmp = tempfile::tempdir().unwrap();
    let (_, fresh) = repo(tmp.path(), "fresh", &[]);
    let (_, released) = repo(tmp.path(), "released", &["v2.3.7"]);
    let (_, underway) = repo(tmp.path(), "underway", &["v1.9.0", "v1.10.0-dev.2"]);
    let root = fresh.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(dev_tag(Some("origin")), Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 3, "{:?}", plan.skipped);

    assert!(command_for(&plan, "fresh").contains("v0.1.0-dev.1"));
    assert!(command_for(&plan, "released").contains("v2.4.0-dev.1"));
    // Ten, not two: the arithmetic is over numbers, and a string sort would
    // have said `v1.9.0` was the newest release here.
    assert!(command_for(&plan, "underway").contains("v1.10.0-dev.3"));

    let jobs = run(&h, plan).await;
    assert!(jobs.iter().all(|j| j.state == JobState::Ok), "{jobs:#?}");

    // On the remote, which is the half that matters, and locally too.
    for (path, tag) in
        [(&fresh, "v0.1.0-dev.1"), (&released, "v2.4.0-dev.1"), (&underway, "v1.10.0-dev.3")]
    {
        assert_eq!(git(path, &["tag", "-l", tag]), tag, "{tag} is not local");
        let origin = path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("{}.git", path.file_name().unwrap().to_string_lossy()));
        assert_eq!(git(&origin, &["tag", "-l", tag]), tag, "{tag} was not published");
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_the_remote_already_has_leaves_nothing_behind() {
    // The test the step order exists for. The remote has `v2.4.0-dev.1` at a
    // commit this repository does not know about, and its local tag list does
    // not — so the derivation picks a name that is taken.
    //
    // With the ordinary order (`tag` then `push`) this would leave a local
    // `v2.4.0-dev.1` at the wrong commit, and every later derivation would skip
    // past it while the two silently disagreed for ever.
    let tmp = tempfile::tempdir().unwrap();
    let (origin, clone) = repo(tmp.path(), "r0", &["v2.3.7"]);

    // Somebody else cuts the tag, on a commit of their own.
    let other = tmp.path().join("other");
    git(tmp.path(), &["clone", origin.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("a.txt"), "theirs\n").unwrap();
    git(&other, &["commit", "-am", "theirs"]);
    git(&other, &["push", "origin", "HEAD:refs/tags/v2.4.0-dev.1"]);

    let root = clone.parent().unwrap().to_path_buf();
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(dev_tag(Some("origin")), Selection::All).await.unwrap();
    assert!(command_for(&plan, "r0").contains("v2.4.0-dev.1"));

    let jobs = run(&h, plan).await;
    assert!(matches!(jobs[0].state, JobState::Failed { .. }), "{:?}", jobs[0].state);

    // Nothing local. This is the whole point of publishing first.
    assert_eq!(git(&clone, &["tag", "-l", "v2.4.0-dev.1"]), "");
    // And the remote still has the other person's tag, untouched.
    assert_eq!(git(&origin, &["rev-parse", "v2.4.0-dev.1"]), git(&other, &["rev-parse", "HEAD"]));

    // The failure says what to do, and does not send the user to `pull`.
    let log = h.job_log(jobs[0].id).await.unwrap();
    let e = explain(&log).expect("a rejected push writes to stderr");
    assert_eq!(e.kind, FailureKind::TagExists, "evidence: {}", e.evidence);
    assert!(e.remedy.unwrap().contains("fetch tags"));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn re_running_after_a_partial_batch_is_harmless() {
    // The same name at the same commit is `Everything up-to-date` and succeeds
    // — verified against real git — so a batch of forty in which three failed
    // for unrelated reasons can simply be run again.
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = repo(tmp.path(), "r0", &["v2.3.7"]);
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let jobs = run(&h, h.plan(dev_tag(Some("origin")), Selection::All).await.unwrap()).await;
    assert_eq!(jobs[0].state, JobState::Ok, "{:#?}", jobs[0].log);

    // Second time round the local tag exists, so the derivation moves on rather
    // than colliding with itself.
    let plan = h.plan(dev_tag(Some("origin")), Selection::All).await.unwrap();
    assert!(command_for(&plan, "r0").contains("v2.4.0-dev.2"), "{}", command_for(&plan, "r0"));
    let jobs = run(&h, plan).await;
    assert_eq!(jobs[0].state, JobState::Ok, "{:#?}", jobs[0].log);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_worktree_is_warned_about_rather_than_refused() {
    // A tag names a commit, so dirtiness is not a reason to refuse one. It *is*
    // a reason to say something: the tag marks HEAD, not what is on disk.
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = repo(tmp.path(), "r0", &["v1.0.0"]);
    repo(tmp.path(), "r1", &["v1.0.0"]);
    std::fs::write(clone.join("a.txt"), "uncommitted\n").unwrap();
    let root = clone.parent().unwrap().to_path_buf();

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(dev_tag(Some("origin")), Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 2, "dirtiness is not a reason to refuse a tag");

    let warning = plan.warning.clone().expect("a dirty repository must be called out");
    assert!(warning.starts_with("1 of these has"), "{warning}");
    assert!(warning.contains("not what is on disk"), "{warning}");
    // And it reaches the sheet, rather than stopping at the plan.
    assert_eq!(plan.view().warning, Some(warning));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_detached_head_can_be_tagged_and_an_unborn_branch_cannot() {
    // The first action to allow a detached HEAD: every other one either moves a
    // branch or promises to put the user back on one, and tagging does neither.
    let tmp = tempfile::tempdir().unwrap();
    let (_, detached) = repo(tmp.path(), "detached", &[]);
    git(&detached, &["checkout", "--detach"]);

    let unborn = tmp.path().join("repos").join("unborn");
    git(tmp.path(), &["init", "-b", "main", unborn.to_str().unwrap()]);
    git(&unborn, &["remote", "add", "origin", tmp.path().join("detached.git").to_str().unwrap()]);

    let root = detached.parent().unwrap().to_path_buf();
    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();
    let plan = h.plan(dev_tag(Some("origin")), Selection::All).await.unwrap();

    assert_eq!(plan.eligible.len(), 1);
    assert_eq!(plan.eligible[0].0.name(), "detached");
    assert!(plan
        .skipped
        .iter()
        .any(|(id, why)| id.name() == "unborn" && *why == SkipReason::UnbornBranch));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_only_tag_needs_no_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("repos").join("solo");
    git(tmp.path(), &["init", "-b", "main", bare.to_str().unwrap()]);
    std::fs::write(bare.join("a.txt"), "one\n").unwrap();
    git(&bare, &["add", "a.txt"]);
    git(&bare, &["commit", "-m", "c1"]);

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![bare.parent().unwrap().to_path_buf()], false).await.unwrap();

    // Pushing needs somewhere to push to; creating one locally does not.
    let pushing = h.plan(dev_tag(Some("origin")), Selection::All).await.unwrap();
    assert_eq!(pushing.skipped[0].1, SkipReason::NoRemote);

    let plan = h.plan(dev_tag(None), Selection::All).await.unwrap();
    assert_eq!(plan.eligible.len(), 1);
    assert_eq!(command_for(&plan, "solo"), "git tag v0.1.0-dev.1");
    let jobs = run(&h, plan).await;
    assert_eq!(jobs[0].state, JobState::Ok, "{:#?}", jobs[0].log);
    assert_eq!(git(&bare, &["tag", "-l"]), "v0.1.0-dev.1");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn packed_tags_count() {
    // A repository that has been `git gc`-ed has no loose tag files at all. A
    // loose-only read would report a decade-old project as never released, and
    // derive `v0.1.0-dev.1` over the top of its real history.
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone) = repo(tmp.path(), "r0", &["v2.3.7", "v2.4.0-dev.1"]);
    git(&clone, &["pack-refs", "--all"]);
    assert!(!clone.join(".git/refs/tags/v2.3.7").exists(), "the fixture is not packed");

    let engine = Engine::start(config());
    let h = engine.handle();
    h.scan_to_completion(vec![clone.parent().unwrap().to_path_buf()], false).await.unwrap();
    let plan = h.plan(dev_tag(Some("origin")), Selection::All).await.unwrap();
    assert!(command_for(&plan, "r0").contains("v2.4.0-dev.2"), "{}", command_for(&plan, "r0"));
    engine.shutdown().await;
}
