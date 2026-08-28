//! Deadline behaviour: one slow repository must not stall the grid, and the
//! process group must actually die.
//!
//! "Slow" is produced with a `core.fsmonitor` hook that sleeps. That is a real
//! slow `git status` — git invokes the hook and waits for it, and
//! `--no-optional-locks` does not change that — rather than a mocked one, so
//! this exercises the same spawn, deadline and kill path production uses.

use git_scylla_core::ProbeOutcome;
use git_scylla_discovery::RepoFound;
use git_scylla_probe::{GitCliProbe, Probe, ProbeRequest};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A repository whose `git status` sleeps, and which records the fact that it
/// finished sleeping by creating `marker`.
///
/// The marker is how the process-group kill is verified: if only the direct
/// child were signalled, the hook would survive its parent and touch it.
fn slow_repo(dir: &Path, sleep_secs: u32) -> (PathBuf, PathBuf) {
    let repo = dir.join("slow");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "f@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "f@example.invalid")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-b", "main", "."]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-m", "c1"]);

    let marker = dir.join("hook-completed");
    let hook = repo.join("slow-hook.sh");
    std::fs::write(&hook, format!("#!/bin/sh\nsleep {sleep_secs}\ntouch {}\n", marker.display()))
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&["config", "core.fsmonitor", hook.to_str().unwrap()]);
    (repo, marker)
}

fn plain_repo(dir: &Path, name: &str) -> PathBuf {
    let repo = dir.join(name);
    std::fs::create_dir_all(&repo).unwrap();
    Command::new("git").args(["init", "-b", "main", "."]).current_dir(&repo).output().unwrap();
    repo
}

fn found(path: &Path) -> RepoFound {
    RepoFound {
        id: git_scylla_core::RepoId::new(path).unwrap(),
        path: path.to_path_buf(),
        kind: git_scylla_core::RepoKind::Normal,
        git_dir: path.join(".git"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_repository_times_out_and_is_not_reported_as_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let (repo, marker) = slow_repo(&dir, 6);

    let probe = GitCliProbe::hermetic();
    let started = Instant::now();
    let snap = probe
        .probe(ProbeRequest {
            found: found(&repo),
            deadline: Instant::now() + Duration::from_millis(700),
        })
        .await;
    let elapsed = started.elapsed();

    assert_eq!(snap.outcome, ProbeOutcome::Timeout);
    assert_eq!(snap.badge(), git_scylla_core::Badge::Unknown);
    assert!(!snap.is_trustworthy());
    assert!(elapsed < Duration::from_secs(3), "gave up after {elapsed:?}, deadline was 700ms");

    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(
        !marker.exists(),
        "the fsmonitor hook outlived the timed-out probe: the process group was not killed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_slow_repository_does_not_delay_the_others() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let (slow, _marker) = slow_repo(&dir, 6);
    let fast: Vec<PathBuf> = (0..8).map(|i| plain_repo(&dir, &format!("fast{i}"))).collect();

    let probe = std::sync::Arc::new(GitCliProbe::hermetic());
    let started = Instant::now();

    let mut tasks = tokio::task::JoinSet::new();
    {
        let probe = probe.clone();
        let slow = slow.clone();
        tasks.spawn(async move {
            let s = probe
                .probe(ProbeRequest {
                    found: found(&slow),
                    deadline: Instant::now() + Duration::from_secs(2),
                })
                .await;
            (s.path, s.outcome)
        });
    }
    for f in &fast {
        let (probe, f) = (probe.clone(), f.clone());
        tasks.spawn(async move {
            let s = probe
                .probe(ProbeRequest {
                    found: found(&f),
                    deadline: Instant::now() + Duration::from_secs(5),
                })
                .await;
            (s.path, s.outcome)
        });
    }

    let mut fast_done = 0;
    let mut fast_elapsed = Duration::ZERO;
    let mut slow_outcome = None;
    while let Some(r) = tasks.join_next().await {
        let (path, outcome) = r.unwrap();
        if path == slow {
            slow_outcome = Some(outcome);
        } else {
            assert_eq!(outcome, ProbeOutcome::Ok, "{}", path.display());
            fast_done += 1;
            if fast_done == fast.len() {
                fast_elapsed = started.elapsed();
            }
        }
    }

    assert_eq!(fast_done, fast.len());
    assert_eq!(slow_outcome, Some(ProbeOutcome::Timeout));
    assert!(
        fast_elapsed < Duration::from_millis(1500),
        "fast repositories took {fast_elapsed:?}; they were blocked by the slow one"
    );
}
