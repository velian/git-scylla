//! The probe against every fixture, asserting the full expected snapshot.
//!
//! This table is the specification of the parsing layer. It runs the real
//! walker and the real `git`, and compares a normalized snapshot to the
//! expectation the fixture declared when it was built — so a change in how git
//! reports something shows up here as a diff rather than as a wrong grid.

use git_scylla_discovery::{WalkOptions, Walker};
use git_scylla_probe::{GitCliProbe, Probe, ProbeRequest};
use git_scylla_testkit::{normalize, FixtureSet};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

async fn probe_all(
    scan_root: &std::path::Path,
) -> BTreeMap<PathBuf, git_scylla_core::RepoSnapshot> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let walker = Walker::new(vec![scan_root.to_path_buf()])
        .options(WalkOptions { nested: true, max_depth: None });
    let root = scan_root.to_path_buf();
    let walk = tokio::task::spawn_blocking(move || walker.walk(tx));

    // Hermetic: the developer's ~/.gitconfig must not be able to change an
    // assertion. A global `status.showUntrackedFiles=no` would otherwise turn
    // every untracked expectation red on one machine and green on another.
    let probe = GitCliProbe::hermetic();
    let mut out = BTreeMap::new();
    while let Some(found) = rx.recv().await {
        let snap = probe
            .probe(ProbeRequest { found, deadline: Instant::now() + Duration::from_secs(30) })
            .await;
        let rel = snap.path.strip_prefix(&root).unwrap().to_path_buf();
        out.insert(rel, snap);
    }
    walk.await.unwrap();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn every_fixture_produces_its_declared_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let probed = probe_all(&set.scan_root).await;

    let mut mismatches = Vec::new();
    for f in &set.fixtures {
        let rel = f.path.strip_prefix(&set.scan_root).unwrap();
        let Some(snap) = probed.get(rel) else {
            mismatches.push(format!("{}: not discovered", f.name));
            continue;
        };
        let actual = normalize(&f.name, snap);
        let expected = f.expect.to_json(&f.name);
        if actual != expected {
            mismatches.push(format!(
                "{}:\n  expected {}\n  actual   {}",
                f.name,
                serde_json::to_string(&expected).unwrap(),
                serde_json::to_string(&actual).unwrap()
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} fixture(s) differ:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_fixture_probes_successfully() {
    // Separate from the table assertion so that a probe that fails outright is
    // reported as such rather than as a hundred field differences.
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    for (path, snap) in probe_all(&set.scan_root).await {
        assert!(snap.is_trustworthy(), "{} probed as {:?}", path.display(), snap.outcome);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scan_never_creates_an_index_lock() {
    // `--no-optional-locks` is the whole reason this holds. Without it, `git
    // status` refreshes the index, takes `index.lock`, and can make the user's
    // own git operation in another terminal fail.
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");

    let locks_before = count_index_locks(&set.scan_root);
    assert_eq!(locks_before, 0, "fixtures should not be built with a lock left behind");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let (root, stop) = (set.scan_root.clone(), stop.clone());
        // Poll for a lock file appearing at any point during the scan, not just
        // after it: the probe holds one for milliseconds if it holds one at all.
        std::thread::spawn(move || {
            let mut seen = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                seen += count_index_locks(&root);
                std::thread::sleep(Duration::from_millis(1));
            }
            seen
        })
    };

    let _ = probe_all(&set.scan_root).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(watcher.join().unwrap(), 0, "the scan created an index.lock");
}

fn count_index_locks(root: &std::path::Path) -> usize {
    fn walk(dir: &std::path::Path, n: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|f| f == "index.lock") {
                *n += 1;
            } else if p.is_dir() {
                walk(&p, n);
            }
        }
    }
    let mut n = 0;
    walk(root, &mut n);
    n
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scan_does_not_disturb_a_rebase_in_progress() {
    // The state a monitoring tool is most likely to break: the user has a
    // rebase stopped on a conflict, possibly with `git rebase -i`'s editor still
    // open. Probing it must neither fail nor leave the rebase unfinishable.
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let repo = &set.get("rebase-in-progress").expect("fixture").path;

    let probed = probe_all(&set.scan_root).await;
    let rel = repo.strip_prefix(&set.scan_root).unwrap();
    let snap = &probed[rel];
    assert!(snap.is_trustworthy(), "{:?}", snap.outcome);
    assert_eq!(snap.op, Some(git_scylla_core::InProgress::Rebase));

    // The real assertion: git can still finish what it started. If the scan had
    // taken a lock, refreshed the index or written state, this fails.
    let out = std::process::Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the scan left the rebase unfinishable: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_never_touches_the_network() {
    // Structural in the probe — one `git status` and some file reads — but worth
    // asserting behaviourally, because a regression here is invisible until
    // someone is on a train. First paint must never wait on ssh.
    //
    // The remote points at an unroutable address. Anything that tried to reach
    // it would block for seconds or minutes; the scan must not notice it exists.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let repo = dir.join("black-hole");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
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
    // 198.51.100.0/24 is TEST-NET-2: reserved, and guaranteed not to route.
    git(&["remote", "add", "origin", "https://198.51.100.1/nope.git"]);
    git(&["config", "branch.main.remote", "origin"]);
    git(&["config", "branch.main.merge", "refs/heads/main"]);

    let started = Instant::now();
    let probed = probe_all(&dir).await;
    let elapsed = started.elapsed();

    let snap = &probed[std::path::Path::new("black-hole")];
    assert!(snap.is_trustworthy(), "{:?}", snap.outcome);
    // The remote was read from config — so the host is known and bucketable —
    // without ever being contacted.
    assert_eq!(snap.remotes.len(), 1);
    assert_eq!(snap.remotes[0].host.as_deref(), Some("198.51.100.1"));
    // Upstream is configured but never fetched, so there is no tracking ref.
    assert!(snap.upstream.as_ref().is_some_and(|u| u.is_gone()));
    assert!(
        elapsed < Duration::from_secs(2),
        "probing a repository with an unroutable remote took {elapsed:?}; something \
         tried to reach the network"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_probe_always_reports_the_id_it_was_given() {
    // The property the engine's scan accounting depends on. It tracks a
    // repository by the id discovery produced, and clears it when a snapshot
    // arrives — so a probe that resolved its own id independently could return a
    // different one, and the entry would never clear. That is a hung scan and,
    // downstream, a `shutdown` that never returns.
    //
    // Tested against the awkward case on purpose: the repository is deleted
    // between discovery and probing, which is exactly when a second
    // canonicalization would disagree with the first.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let repo = dir.join("doomed");
    std::fs::create_dir_all(&repo).unwrap();
    let out = std::process::Command::new("git")
        .args(["init", "-b", "main", "."])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_n, fatal) = Walker::new(vec![dir.clone()]).walk(tx);
    assert!(fatal.is_empty());
    let found = rx.try_recv().expect("discovered");
    let expected = found.id.clone();

    // Gone before it is read.
    std::fs::remove_dir_all(&repo).unwrap();

    let snap = GitCliProbe::hermetic()
        .probe(ProbeRequest { found, deadline: Instant::now() + Duration::from_secs(10) })
        .await;

    assert_eq!(snap.id, expected, "the probe invented a different identity");
    assert!(!snap.is_trustworthy(), "a deleted repository must not read as ok");
    assert_eq!(snap.badge(), git_scylla_core::Badge::Unknown);
}
