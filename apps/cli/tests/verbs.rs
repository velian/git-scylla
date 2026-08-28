//! The mutating verbs end to end, through the real binary.
//!
//! Local bare repositories throughout, so the suite needs no network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_git-scylla");

fn git(cwd: &Path, args: &[&str]) {
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
}

struct Tree {
    dir: PathBuf,
    repos: PathBuf,
    seed: PathBuf,
}

/// `n` clones of one local bare repository.
fn tree(dir: &Path, n: usize) -> Tree {
    let dir = dir.canonicalize().unwrap();
    let repos = dir.join("repos");
    let scratch = dir.join("scratch");
    for p in [&repos, &scratch] {
        std::fs::create_dir_all(p).unwrap();
    }
    git(&dir, &["init", "--bare", "-b", "main", "origin.git"]);
    let origin = dir.join("origin.git");
    let seed = scratch.join("seed");
    git(&scratch, &["clone", origin.to_str().unwrap(), "seed"]);
    std::fs::write(seed.join("a.txt"), "one\n").unwrap();
    git(&seed, &["add", "a.txt"]);
    git(&seed, &["commit", "-m", "c1"]);
    git(&seed, &["push", "-u", "origin", "main"]);
    for i in 0..n {
        git(&repos, &["clone", origin.to_str().unwrap(), &format!("r{i}")]);
    }
    Tree { dir, repos, seed }
}

fn advance(t: &Tree) {
    std::fs::write(t.seed.join("a.txt"), "two\n").unwrap();
    git(&t.seed, &["commit", "-am", "c2"]);
    git(&t.seed, &["push", "origin", "main"]);
}

/// Run the binary with an isolated state directory, so `log` cannot pick up a
/// previous test's run and no test writes to the developer's real state.
fn run(t: &Tree, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("GIT_SCYLLA_STATE_DIR", t.dir.join("state"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn head(repo: &Path) -> String {
    let out = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(repo).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

// ---- dry run -----------------------------------------------------------

#[test]
fn dry_run_prints_the_plan_exits_zero_and_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 3);
    advance(&t);
    for i in 0..3 {
        git(&t.repos.join(format!("r{i}")), &["fetch"]);
    }
    let before: Vec<String> = (0..3).map(|i| head(&t.repos.join(format!("r{i}")))).collect();

    let out = run(&t, &["pull", t.repos.to_str().unwrap(), "--dry-run"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("Pull 3 repos (ff-only)"), "{text}");
    assert!(text.contains("3 will pull"), "{text}");

    let after: Vec<String> = (0..3).map(|i| head(&t.repos.join(format!("r{i}")))).collect();
    assert_eq!(before, after, "--dry-run mutated something");
}

#[test]
fn without_yes_and_without_a_terminal_it_refuses_rather_than_assuming() {
    // "Never mutate without one of the two." With no terminal there is nobody
    // to ask, so the only safe answer is to stop.
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 2);
    advance(&t);
    for i in 0..2 {
        git(&t.repos.join(format!("r{i}")), &["fetch"]);
    }
    let before = head(&t.repos.join("r0"));

    let out = run(&t, &["pull", t.repos.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("refusing to run without confirmation"), "{err}");
    assert!(err.contains("-y"), "the message must say how to proceed: {err}");
    assert_eq!(head(&t.repos.join("r0")), before);
}

// ---- doing the work ----------------------------------------------------

#[test]
fn pull_advances_every_eligible_repository_and_leaves_the_rest_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 4);
    advance(&t);
    // r0, r1: fetched and clean -> will pull
    // r2: fetched but dirty     -> skipped
    // r3: never fetched         -> already up to date
    for name in ["r0", "r1", "r2"] {
        git(&t.repos.join(name), &["fetch"]);
    }
    std::fs::write(t.repos.join("r2/a.txt"), "local\n").unwrap();
    let before: Vec<String> =
        ["r0", "r1", "r2", "r3"].iter().map(|n| head(&t.repos.join(n))).collect();

    let out = run(&t, &["pull", t.repos.to_str().unwrap(), "-y"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let summary = String::from_utf8_lossy(&out.stdout);
    assert!(summary.contains("2 ok"), "{summary}");
    assert!(summary.contains("2 skipped"), "{summary}");

    assert_ne!(head(&t.repos.join("r0")), before[0], "r0 should have moved");
    assert_ne!(head(&t.repos.join("r1")), before[1]);
    assert_eq!(head(&t.repos.join("r2")), before[2], "the dirty one must not move");
    assert_eq!(head(&t.repos.join("r3")), before[3]);
    // ...and the dirty one still has its uncommitted work.
    assert_eq!(std::fs::read_to_string(t.repos.join("r2/a.txt")).unwrap(), "local\n");
}

#[test]
fn fetch_updates_remote_tracking_refs_without_touching_the_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 3);
    advance(&t);
    // Deliberately dirty, to prove fetch does not care.
    std::fs::write(t.repos.join("r0/a.txt"), "local\n").unwrap();
    let before = head(&t.repos.join("r0"));

    let out = run(&t, &["fetch", t.repos.to_str().unwrap(), "-y", "--prune"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("3 ok"));

    assert_eq!(head(&t.repos.join("r0")), before, "fetch moved HEAD");
    assert_eq!(std::fs::read_to_string(t.repos.join("r0/a.txt")).unwrap(), "local\n");
    // The tracking ref did move.
    let scan = run(&t, &["scan", t.repos.to_str().unwrap(), "--json"]);
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&scan.stdout).unwrap();
    for row in &rows {
        assert_eq!(row["upstream"]["sync"]["behind"], 1, "{row}");
    }
}

// ---- exit codes --------------------------------------------------------

#[test]
fn a_failed_job_exits_one_and_names_the_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 3);
    // Loopback port 1: refused instantly, so this needs no network and does not
    // depend on how long a dropped route takes to give up.
    git(&t.repos.join("r0"), &["remote", "set-url", "origin", "https://127.0.0.1:1/x.git"]);

    let out = run(&t, &["fetch", t.repos.to_str().unwrap(), "-y"]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    // Partial failure is a normal outcome, so the successes are still reported.
    assert!(text.contains("2 ok"), "{text}");
    assert!(text.contains("1 failed"), "{text}");
    assert!(text.contains("failed:"), "{text}");
    assert!(text.contains("r0"), "{text}");
    // And it says how to read the transcript.
    assert!(text.contains("git-scylla log"), "{text}");
}

#[test]
fn nothing_found_is_not_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 1);
    let empty = t.dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = run(&t, &["fetch", empty.to_str().unwrap(), "-y"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no repositories found"));
}

#[test]
fn nothing_eligible_is_not_a_failure_either() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 2);
    // Nothing is behind, so an ff-only pull has nothing to do.
    let out = run(&t, &["pull", t.repos.to_str().unwrap(), "-y"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Nothing to do"));
}

#[test]
fn a_bad_selection_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 1);
    let out = run(&t, &["pull", t.repos.to_str().unwrap(), "--select", "brunch:main", "-y"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("bad --select"));
}

// ---- selection ---------------------------------------------------------

#[test]
fn select_narrows_the_batch_and_the_header_says_by_how_much() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 4);
    advance(&t);
    for i in 0..4 {
        git(&t.repos.join(format!("r{i}")), &["fetch"]);
    }

    let out = run(&t, &["pull", t.repos.to_str().unwrap(), "--select", "name:r0", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("1 of 4 selected"), "{text}");
    assert!(text.contains("1 will pull"), "{text}");
}

// ---- json and log ------------------------------------------------------

#[test]
fn json_goes_to_stdout_and_carries_per_job_state() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 3);
    advance(&t);

    let out = run(&t, &["fetch", t.repos.to_str().unwrap(), "-y", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert_eq!(v["summary"]["ok"], 3);
    assert_eq!(v["command"], "git fetch");
    let jobs = v["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 3);
    for job in jobs {
        assert_eq!(job["state"]["type"], "Ok");
        assert!(job["duration_ms"].is_u64());
        assert_eq!(job["log_truncated"], false);
        assert!(job["path"].is_string());
    }
}

#[test]
fn log_prints_a_transcript_from_the_last_run() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 2);
    git(&t.repos.join("r0"), &["remote", "set-url", "origin", "https://127.0.0.1:1/x.git"]);
    let out = run(&t, &["fetch", t.repos.to_str().unwrap(), "-y", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let failed = v["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["state"]["type"] == "Failed")
        .expect("one job should have failed");
    let id = failed["id"].as_u64().unwrap().to_string();

    let listed = run(&t, &["log"]);
    assert_eq!(listed.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&listed.stdout).contains("last run:"));

    let shown = run(&t, &["log", &id]);
    assert_eq!(shown.status.code(), Some(0));
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(text.contains("git fetch"), "{text}");
    assert!(text.contains("fatal:"), "the transcript should carry git's own words: {text}");
}

#[test]
fn log_for_an_unknown_job_lists_what_is_available() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 2);
    run(&t, &["fetch", t.repos.to_str().unwrap(), "-y"]);
    let out = run(&t, &["log", "9999"]);
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no job 9999"), "{err}");
    assert!(err.contains("r0"), "it should list the jobs it does have: {err}");
}

#[test]
fn log_with_no_previous_run_says_so_rather_than_crashing() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 1);
    let out = run(&t, &["log", "1"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no transcripts available"));
}

// ---- modes -------------------------------------------------------------

#[test]
fn the_pull_mode_reaches_the_plan_header() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 1);
    for (flag, expected) in [("ff-only", "(ff-only)"), ("rebase", "(rebase)"), ("merge", "(merge)")]
    {
        let out = run(&t, &["pull", t.repos.to_str().unwrap(), "--mode", flag, "--dry-run"]);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains(expected), "--mode {flag} produced: {text}");
    }
}

#[test]
fn rebase_pulls_a_branch_that_ff_only_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 1);
    advance(&t);
    let repo = t.repos.join("r0");
    git(&repo, &["fetch"]);
    // Local commit as well as a remote one: diverged.
    std::fs::write(repo.join("local.txt"), "local\n").unwrap();
    git(&repo, &["add", "local.txt"]);
    git(&repo, &["commit", "-m", "local"]);

    let ff = run(&t, &["pull", t.repos.to_str().unwrap(), "--mode", "ff-only", "--dry-run"]);
    assert!(String::from_utf8_lossy(&ff.stdout).contains("diverged from upstream"));

    let rebase = run(&t, &["pull", t.repos.to_str().unwrap(), "--mode", "rebase", "-y"]);
    assert_eq!(rebase.status.code(), Some(0), "{}", String::from_utf8_lossy(&rebase.stderr));
    assert!(String::from_utf8_lossy(&rebase.stdout).contains("1 ok"));
    // The local commit survived the rebase and the remote one arrived.
    assert!(repo.join("local.txt").exists());
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "two\n");
}

#[test]
fn a_root_that_cannot_be_read_is_a_configuration_error_not_an_empty_result() {
    // The distinction that matters: "no repositories here" and "I could not
    // look" produce the same empty list, and only one of them is the user's
    // problem to fix. Reporting the second as success is how a tool tells
    // someone their working set is fine when it has not looked at it — the
    // same failure the Full Disk Access hint exists for.
    let tmp = tempfile::tempdir().unwrap();
    let t = tree(tmp.path(), 1);

    for verb in ["scan", "fetch", "pull"] {
        let mut args = vec![verb, "/nope/nope/nope"];
        if verb != "scan" {
            args.push("-y");
        }
        let out = run(&t, &args);
        assert_eq!(out.status.code(), Some(3), "{verb} on an unreadable root");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("does not exist or is not readable"), "{verb}: {err}");
        // Reported once, not once by the engine's logging and once by us.
        assert_eq!(err.matches("/nope/nope/nope").count(), 1, "{verb}: {err}");
    }

    // ...and an empty-but-readable directory is still a plain report.
    let empty = t.dir.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = run(&t, &["scan", empty.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
}
