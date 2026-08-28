//! What one hardened spawn costs, against a bare `tokio::process` call.
//!
//! A hundred-repository scan is budgeted at under a second and almost all of it
//! is subprocess time, so the wrapper's overhead is a number worth having rather
//! than assuming.

use git_scylla_exec::GitCommand;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const N: usize = 100;
const CONCURRENCY: usize = 8;
const ARGS: &[&str] =
    &["--no-optional-locks", "status", "--porcelain=v2", "--branch", "-z", "-unormal"];

fn repo(dir: &Path) -> std::path::PathBuf {
    let repo = dir.join("r");
    std::fs::create_dir_all(&repo).unwrap();
    for args in [&["init", "-b", "main", "."][..], &["add", "-A"][..]] {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
    }
    repo
}

async fn via_exec(repo: &Path) {
    let _ =
        GitCommand::new(repo).args(ARGS).capture(Instant::now() + Duration::from_secs(10)).await;
}

async fn via_raw(repo: &Path) {
    let _ = tokio::process::Command::new("git")
        .args(ARGS)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hardened_wrapper_is_not_where_a_scan_spends_its_time() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Arc::new(repo(tmp.path()));

    let run = |raw: bool| {
        let repo = Arc::clone(&repo);
        async move {
            let sem = Arc::new(Semaphore::new(CONCURRENCY));
            let start = Instant::now();
            let mut set = tokio::task::JoinSet::new();
            for _ in 0..N {
                let (sem, repo) = (Arc::clone(&sem), Arc::clone(&repo));
                set.spawn(async move {
                    let _p = sem.acquire().await.unwrap();
                    if raw {
                        via_raw(&repo).await
                    } else {
                        via_exec(&repo).await
                    }
                });
            }
            while set.join_next().await.is_some() {}
            start.elapsed()
        }
    };

    // Warm, then take the best of three: this measures a wrapper, not a machine.
    let mut exec = Duration::MAX;
    let mut raw = Duration::MAX;
    for _ in 0..3 {
        exec = exec.min(run(false).await);
        raw = raw.min(run(true).await);
    }

    let overhead = exec.saturating_sub(raw);
    eprintln!(
        "{N} spawns at {CONCURRENCY}x — exec {exec:?}, raw {raw:?}, overhead {overhead:?} \
         ({:.0}µs per spawn)",
        overhead.as_micros() as f64 / N as f64
    );
    assert!(
        overhead < raw,
        "the hardening costs more than the work it wraps: exec {exec:?} vs raw {raw:?}"
    );
}
