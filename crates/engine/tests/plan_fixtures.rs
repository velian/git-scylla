//! Planning over the real fixture set: discovery, probe, policy, plan, render.
//!
//! The unit tests in `plan.rs` use synthetic snapshots, which is right for
//! covering shapes exhaustively. This one proves the whole stack agrees on real
//! repositories built by real `git` — the seam where a plausible-looking
//! synthetic snapshot and an actual one diverge.

use git_scylla_core::{Action, PullMode, RepoSnapshot, SkipReason};
use git_scylla_discovery::{WalkOptions, Walker};
use git_scylla_engine::{plan, Policy, Selection};
use git_scylla_probe::{GitCliProbe, Probe, ProbeRequest};
use git_scylla_testkit::FixtureSet;
use std::time::{Duration, Instant, SystemTime};

async fn snapshots(scan_root: &std::path::Path) -> Vec<RepoSnapshot> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let walker = Walker::new(vec![scan_root.to_path_buf()])
        .options(WalkOptions { nested: true, max_depth: None });
    let walk = tokio::task::spawn_blocking(move || walker.walk(tx));
    let probe = GitCliProbe::hermetic();
    let mut out = Vec::new();
    while let Some(found) = rx.recv().await {
        out.push(
            probe
                .probe(ProbeRequest { found, deadline: Instant::now() + Duration::from_secs(30) })
                .await,
        );
    }
    walk.await.unwrap();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// A policy that cannot be tripped by how long the fixture build took.
fn policy() -> Policy {
    Policy { max_snapshot_age: Duration::from_secs(86_400), ..Default::default() }
}

fn name_of(snap_path: &std::path::Path, root: &std::path::Path) -> String {
    snap_path.strip_prefix(root).unwrap().to_string_lossy().into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pull_plan_over_the_fixture_set() {
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let snaps = snapshots(&set.scan_root).await;
    let p = plan(
        &Action::Pull { mode: PullMode::Rebase },
        &snaps,
        &Selection::All,
        SystemTime::now(),
        &policy(),
    );

    // Everything is accounted for: nothing silently vanishes between the
    // snapshot list and the plan.
    assert_eq!(p.selected(), snaps.len());
    assert_eq!(p.eligible.len() + p.skipped.len(), snaps.len());

    let eligible: Vec<String> =
        p.eligible.iter().map(|(id, _)| name_of(id.path(), &set.scan_root)).collect();
    // Exactly the fixtures that are behind, clean, tracked and attached.
    assert_eq!(eligible, vec!["behind".to_string(), "diverged".to_string()]);

    // The dirty-and-behind fixtures are refused for the right reason, not by
    // accident of some earlier rule.
    for name in ["behind-dirty", "behind-untracked"] {
        let (_, why) = p
            .skipped
            .iter()
            .find(|(id, _)| name_of(id.path(), &set.scan_root) == name)
            .unwrap_or_else(|| panic!("{name} missing from the plan"));
        assert_eq!(*why, SkipReason::DirtyWorktree, "{name}");
    }

    println!("{}", p.render());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_plan_covers_every_fixture_with_a_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let snaps = snapshots(&set.scan_root).await;
    let p = plan(
        &Action::Fetch { prune: true, tags: false },
        &snaps,
        &Selection::All,
        SystemTime::now(),
        &policy(),
    );

    // Fetch is the permissive action: eligible for exactly those with a remote,
    // regardless of worktree state or a half-finished operation.
    let want: usize = snaps.iter().filter(|s| !s.remotes.is_empty()).count();
    assert_eq!(p.eligible.len(), want);
    assert!(want >= 8, "the fixture set should have several tracked repositories");
    assert!(p.skipped.iter().all(|(_, why)| *why == SkipReason::NoRemote));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_filter_selection_narrows_the_plan_and_the_header_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let snaps = snapshots(&set.scan_root).await;

    let sel = Selection::parse("behind:>0", None).unwrap();
    let p =
        plan(&Action::Pull { mode: PullMode::Rebase }, &snaps, &sel, SystemTime::now(), &policy());

    assert!(p.selected() < snaps.len(), "the filter should have narrowed it");
    assert_eq!(p.considered, snaps.len());
    let rendered = p.render();
    assert!(
        rendered.contains(&format!("{} of {} selected", p.selected(), snaps.len())),
        "{rendered}"
    );
    // Every selected repository really is behind — the filter and the
    // preconditions read the same snapshot.
    for (id, _) in &p.eligible {
        let s = snaps.iter().find(|s| &s.id == id).unwrap();
        assert!(s.upstream.as_ref().and_then(|u| u.behind()).is_some_and(|b| b > 0));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn planning_the_fixture_set_is_free_even_after_probing_it_was_not() {
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let snaps = snapshots(&set.scan_root).await;
    let policy = policy();
    let sel = Selection::All;

    let mut times = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        let p = plan(
            &Action::Pull { mode: PullMode::Rebase },
            &snaps,
            &sel,
            SystemTime::now(),
            &policy,
        );
        times.push(start.elapsed());
        assert_eq!(p.selected(), snaps.len());
    }
    times.sort();
    let median = times[times.len() / 2];
    // The budget is 5 ms for a hundred repositories; this set is smaller, so
    // the bar is the same and the margin is larger.
    assert!(median < Duration::from_millis(5), "median {median:?}");
    eprintln!("plan over {} real snapshots: median {median:?}", snaps.len());
}
