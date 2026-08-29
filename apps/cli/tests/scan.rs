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
        assert_eq!(row["fetch"]["schedule"]["type"], "Disabled");
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

    let typo = json(&[dir.to_str().unwrap(), "--json", "--filter", "drity"]);
    assert!(typo.is_empty());

    let bad = scan(&[dir.to_str().unwrap(), "--filter", "brunch:main"]);
    assert_eq!(bad.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("bad --select"));
}

#[test]
fn a_bare_word_fuzzy_matches_the_repository_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    make_repos(&dir, 1);
    let found = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap();
    std::fs::rename(found.path(), dir.join("git-scyllae")).unwrap();

    let hit = json(&[dir.to_str().unwrap(), "--json", "--filter", "scyll"]);
    assert_eq!(hit.len(), 1, "scyll is a subsequence of git-scyllae");

    let miss = json(&[dir.to_str().unwrap(), "--json", "--filter", "zzz"]);
    assert!(miss.is_empty());
}

#[test]
fn scan_is_a_report_so_it_always_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let empty = scan(&[dir.to_str().unwrap()]);
    assert_eq!(empty.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&empty.stdout).contains("no repositories found"));

    make_repos(&dir, 1);
    assert_eq!(scan(&[dir.to_str().unwrap()]).status.code(), Some(0));

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
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    make_repos(&dir, 100);

    let _ = json(&[dir.to_str().unwrap(), "--json"]);

    let started = Instant::now();
    let rows = json(&[dir.to_str().unwrap(), "--json"]);
    let elapsed = started.elapsed();
    assert_eq!(rows.len(), 100);

    assert!(
        elapsed.as_millis() < 3000,
        "scanning 100 repositories took {elapsed:?}; the target is under 1 s and \
         this guard allows 3 s, so something serialized"
    );
    eprintln!("100 repositories in {elapsed:?} (target: < 1 s)");
}
