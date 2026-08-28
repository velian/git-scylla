//! End-to-end tests of the `scan` verb: the JSON contract, filtering, exit
//! codes, and the cold-scan performance target.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

const BIN: &str = env!("CARGO_BIN_EXE_git-scylla");

fn scan(args: &[&str]) -> Output {
    Command::new(BIN).arg("scan").args(args).output().expect("run git-scylla")
}

fn json(args: &[&str]) -> Vec<serde_json::Value> {
    let out = scan(args);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("valid JSON on stdout")
}

fn make_repos(dir: &Path, n: usize) -> Vec<PathBuf> {
    (0..n)
        .map(|i| {
            let repo = dir.join(format!("repo{i:03}"));
            std::fs::create_dir_all(&repo).unwrap();
            let git = |args: &[&str]| {
                let out = Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .env("GIT_AUTHOR_NAME", "F")
                    .env("GIT_AUTHOR_EMAIL", "f@example.invalid")
                    .env("GIT_COMMITTER_NAME", "F")
                    .env("GIT_COMMITTER_EMAIL", "f@example.invalid")
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            };
            git(&["init", "-b", "main", "."]);
            std::fs::write(repo.join("a.txt"), "a\n").unwrap();
            git(&["add", "a.txt"]);
            git(&["commit", "-m", "c1"]);
            // Every third repository is dirty, so the filter test has something
            // to select and the count is predictable.
            if i % 3 == 0 {
                std::fs::write(repo.join("b.txt"), "b\n").unwrap();
            }
            repo
        })
        .collect()
}

#[test]
fn json_is_a_list_of_snapshots_with_the_documented_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    make_repos(&dir, 3);

    let rows = json(&[dir.to_str().unwrap(), "--json"]);
    assert_eq!(rows.len(), 3);
    for row in &rows {
        // The fields the generated TypeScript depends on.
        for key in [
            "id",
            "path",
            "kind",
            "head",
            "upstream",
            "remotes",
            "work",
            "op",
            "stashes",
            "fetch",
            "probed_at",
            "outcome",
        ] {
            assert!(row.get(key).is_some(), "missing {key} in {row}");
        }
        assert_eq!(row["kind"]["type"], "Normal");
        assert_eq!(row["head"]["type"], "Branch");
        assert_eq!(row["head"]["value"], "main");
        assert_eq!(row["outcome"]["type"], "Ok");
        // No remote, so automatic fetching is disabled rather than pending.
        assert_eq!(row["fetch"]["schedule"]["type"], "Disabled");
        // Timestamps are Unix milliseconds, not serde's {secs, nanos} map.
        assert!(row["probed_at"].is_i64(), "probed_at should be millis: {}", row["probed_at"]);
    }
}

#[test]
fn filter_selects_and_a_bad_filter_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    make_repos(&dir, 9);

    let dirty = json(&[dir.to_str().unwrap(), "--json", "--filter", "dirty"]);
    assert_eq!(dirty.len(), 3, "every third repository was made dirty");

    let clean = json(&[dir.to_str().unwrap(), "--json", "--filter", "!dirty"]);
    assert_eq!(clean.len(), 6);

    let none = json(&[dir.to_str().unwrap(), "--json", "--filter", "behind:>0"]);
    assert!(none.is_empty(), "no repository has an upstream");

    // A typo must be an error, not a silently empty result.
    let bad = scan(&[dir.to_str().unwrap(), "--filter", "drity"]);
    assert_eq!(bad.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("bad --select"));
}

#[test]
fn scan_is_a_report_so_it_always_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    // Nothing to find is not a failure.
    let empty = scan(&[dir.to_str().unwrap()]);
    assert_eq!(empty.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&empty.stdout).contains("no repositories found"));

    make_repos(&dir, 1);
    assert_eq!(scan(&[dir.to_str().unwrap()]).status.code(), Some(0));

    // A root that does not exist is a configuration error, and the only thing
    // that is not exit 0.
    let missing = scan(&["/nope/nope/nope"]);
    assert_eq!(missing.status.code(), Some(3));
}

#[test]
fn logs_go_to_stderr_so_json_stays_pipeable() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    make_repos(&dir, 2);
    let out = Command::new(BIN)
        .args(["scan", dir.to_str().unwrap(), "--json"])
        .env("RUST_LOG", "trace")
        .output()
        .unwrap();
    assert!(!out.stderr.is_empty(), "trace logging should produce stderr output");
    serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout)
        .expect("stdout must be JSON even with RUST_LOG=trace");
}

#[test]
fn a_hundred_repositories_scan_in_about_the_target_time() {
    // The target is a warm 100-repository scan under 1 s. Measured on an
    // 8-core M-series machine: 0.64 s release, 0.75 s debug.
    //
    // That number is essentially all `git status` process cost. 100 sequential
    // invocations take ~2.0 s on the same machine and a shell `xargs -P8` doing
    // the same work takes 0.88 s, so the scan is already faster than the obvious
    // baseline and raising concurrency past `available_parallelism()` changes
    // nothing — it is bound by fork/exec, not by our scheduling.
    //
    // Which means the margin against the 1 s target is thin and belongs to the
    // machine, not to this code — and "the machine" includes its power state.
    // The same hundred repositories measured 0.64 s on mains and 0.92 s on
    // battery, while a hundred bare `git status` spawns at 8x concurrency moved
    // from ~0.55 s to ~0.83 s alongside it. The scan stayed ~90 ms above that
    // floor throughout; only the floor moved.
    //
    // So the assertion below is a regression guard with room for a throttled
    // laptop and a loaded CI runner. A real regression in the scan pipeline —
    // accidental serialization, a second subprocess per repository — costs
    // multiples of this, not percent.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    make_repos(&dir, 100);

    // Warm: one throwaway run so the page cache and the index are hot, which is
    // the "warm" in the target.
    let _ = json(&[dir.to_str().unwrap(), "--json"]);

    let started = Instant::now();
    let rows = json(&[dir.to_str().unwrap(), "--json"]);
    let elapsed = started.elapsed();
    assert_eq!(rows.len(), 100);

    // Whole-process, so it includes binary startup and JSON serialization.
    assert!(
        elapsed.as_millis() < 3000,
        "scanning 100 repositories took {elapsed:?}; the target is under 1 s and \
         this guard allows 3 s, so something serialized"
    );
    eprintln!("100 repositories in {elapsed:?} (target: < 1 s)");
}
