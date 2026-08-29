//! Every action against every fixture, as a table.
//!
//! The cross product of every `Action` variant against every fixture snapshot.
//! This table *is* the specification of the tool's safety.
//!
//! Regenerate with `UPDATE_ELIGIBILITY_TABLE=1 cargo test -p git-scylla-engine`
//! and **read the diff**.

use git_scylla_core::SkipReason;
use git_scylla_core::{Action, PullMode, RepoSnapshot, SyncPlan};
use git_scylla_discovery::{WalkOptions, Walker};
use git_scylla_engine::{evaluate, sync_default_resolved, Eligibility, Policy};
use git_scylla_probe::{GitCliProbe, Probe, ProbeRequest};
use git_scylla_testkit::FixtureSet;
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

const GOLDEN: &str = include_str!("eligibility_table.txt");

fn columns() -> Vec<(&'static str, Action)> {
    vec![
        ("fetch", Action::Fetch { prune: true, tags: false }),
        ("pull-ff", Action::Pull { mode: PullMode::FfOnly }),
        ("pull-rb", Action::Pull { mode: PullMode::Rebase }),
        ("pull-mg", Action::Pull { mode: PullMode::Merge }),
        ("push", Action::Push { set_upstream: None, force_with_lease: false }),
        ("push-u", Action::Push { set_upstream: Some("origin".into()), force_with_lease: false }),
        ("push-lease", Action::Push { set_upstream: None, force_with_lease: true }),
        ("checkout", Action::Checkout { rev: "main".into(), create: false }),
        ("checkout-b", Action::Checkout { rev: "wip".into(), create: true }),
        ("commit", Action::Commit { message: "m".into(), stage_all: false, no_verify: false }),
        ("commit-a", Action::Commit { message: "m".into(), stage_all: true, no_verify: false }),
        ("stash", Action::Stash { include_untracked: false }),
        ("stash-u", Action::Stash { include_untracked: true }),
        ("pop", Action::StashPop),
        ("custom", Action::Custom { args: vec!["gc".into()], network: true, mutating: true }),
        ("sync", Action::SyncDefault { mode: PullMode::FfOnly, plan: None }),
        (
            "devtag",
            Action::DevTag {
                channel: "dev".into(),
                bump: git_scylla_core::version::Bump::Minor,
                name: None,
                push: Some("origin".into()),
            },
        ),
        (
            "sync-on-trunk",
            Action::SyncDefault {
                mode: PullMode::FfOnly,
                plan: Some(SyncPlan {
                    default: "main".into(),
                    back_to: "main".into(),
                    stash: false,
                }),
            },
        ),
        (
            "sync-off-trunk",
            Action::SyncDefault {
                mode: PullMode::FfOnly,
                plan: Some(SyncPlan {
                    default: "main".into(),
                    back_to: "wip".into(),
                    stash: false,
                }),
            },
        ),
        (
            "undo",
            Action::Reset {
                to: git_scylla_core::Oid::parse("0123456").expect("static oid"),
                mode: git_scylla_core::ResetMode::Hard,
            },
        ),
    ]
}

fn verdict(action: &Action, snap: &RepoSnapshot, now: SystemTime, policy: &Policy) -> Eligibility {
    let first = evaluate(action, snap, now, policy);
    match (&first, action) {
        (Eligibility::Eligible, Action::SyncDefault { plan: Some(p), .. }) => {
            sync_default_resolved(snap, p)
        }
        _ => first,
    }
}

fn code(e: &Eligibility) -> String {
    match e {
        Eligibility::Eligible => "ok".into(),
        Eligibility::Skip(r) => match r {
            SkipReason::UpToDate => "utd".into(),
            SkipReason::NoUpstream => "noup".into(),
            SkipReason::UpstreamGone => "gone".into(),
            SkipReason::DirtyWorktree => "dirty".into(),
            SkipReason::OperationInProgress(op) => format!("op:{op}"),
            SkipReason::DetachedHead => "detach".into(),
            SkipReason::UnbornBranch => "unborn".into(),
            SkipReason::BareRepo => "bare".into(),
            SkipReason::Diverged => "diverg".into(),
            SkipReason::NoRemote => "norem".into(),
            SkipReason::SnapshotStale => "stale".into(),
            SkipReason::NotSelected => "notsel".into(),
            SkipReason::RefNotFound(_) => "noref".into(),
            SkipReason::NoStash => "nostash".into(),
            SkipReason::NoDefaultBranch => "nodef".into(),
            SkipReason::NotUndoable(_) => "noundo".into(),
            SkipReason::HeadMoved => "moved".into(),
        },
    }
}

async fn probe_fixtures(scan_root: &std::path::Path) -> BTreeMap<String, RepoSnapshot> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let walker = Walker::new(vec![scan_root.to_path_buf()])
        .options(WalkOptions { nested: true, max_depth: None });
    let root = scan_root.to_path_buf();
    let walk = tokio::task::spawn_blocking(move || walker.walk(tx));

    let probe = GitCliProbe::hermetic();
    let mut out = BTreeMap::new();
    while let Some(found) = rx.recv().await {
        let snap = probe
            .probe(ProbeRequest { found, deadline: Instant::now() + Duration::from_secs(30) })
            .await;
        let name = snap.path.strip_prefix(&root).unwrap().to_string_lossy().into_owned();
        out.insert(name, snap);
    }
    walk.await.unwrap();
    out
}

fn render(snapshots: &BTreeMap<String, RepoSnapshot>) -> String {
    let cols = columns();
    let policy = Policy { max_snapshot_age: Duration::from_secs(86_400), ..Default::default() };
    let now = SystemTime::now();

    let name_w = snapshots.keys().map(|k| k.len()).max().unwrap_or(10).max("repository".len());
    let widths: Vec<usize> = cols
        .iter()
        .map(|(name, action)| {
            let longest = snapshots
                .values()
                .map(|s| code(&evaluate(action, s, now, &policy)).len())
                .max()
                .unwrap_or(0);
            longest.max(name.len())
        })
        .collect();

    let mut out = String::new();
    out.push_str(&format!("{:name_w$}", "repository"));
    for (i, (name, _)) in cols.iter().enumerate() {
        out.push_str(&format!("  {:w$}", name, w = widths[i]));
    }
    out.push('\n');
    out.push_str(&"-".repeat(name_w));
    for w in &widths {
        out.push_str(&format!("  {}", "-".repeat(*w)));
    }
    out.push('\n');

    for (name, snap) in snapshots {
        out.push_str(&format!("{name:name_w$}"));
        for (i, (_, action)) in cols.iter().enumerate() {
            out.push_str(&format!(
                "  {:w$}",
                code(&verdict(action, snap, now, &policy)),
                w = widths[i]
            ));
        }
        out.push('\n');
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn the_eligibility_table_matches_the_checked_in_specification() {
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let snapshots = probe_fixtures(&set.scan_root).await;
    assert!(snapshots.len() >= 25, "expected the full fixture set, got {}", snapshots.len());

    let actual = render(&snapshots);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/eligibility_table.txt");

    if std::env::var_os("UPDATE_ELIGIBILITY_TABLE").is_some() {
        std::fs::write(&path, &actual).unwrap();
        eprintln!("wrote {}", path.display());
        return;
    }

    if actual.trim() != GOLDEN.trim() {
        let actual_path = path.with_extension("txt.actual");
        std::fs::write(&actual_path, &actual).unwrap();
        panic!(
            "the eligibility table changed.\n\n\
             Wrote the new table to {}\n\
             Diff it against {}\n\n\
             This table is the specification of the tool's safety. Read \
             every changed cell before accepting it: a fix for one repository \
             state routinely changes the answer for several others. To accept, \
             run with UPDATE_ELIGIBILITY_TABLE=1.",
            actual_path.display(),
            path.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_fixture_is_eligible_for_an_action_that_would_obviously_destroy_work() {
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let snapshots = probe_fixtures(&set.scan_root).await;
    let policy = Policy { max_snapshot_age: Duration::from_secs(86_400), ..Default::default() };
    let now = SystemTime::now();

    for (name, snap) in &snapshots {
        for (label, action) in columns() {
            let eligible = verdict(&action, snap, now, &policy).is_eligible();
            if !eligible {
                continue;
            }
            match &action {
                // Nothing that rewrites the worktree may run on a dirty one.
                Action::Pull { .. } | Action::Checkout { .. } => assert!(
                    snap.is_clean(),
                    "{label} is eligible for {name}, which is dirty: {:?}",
                    snap.work
                ),
                _ => {}
            }
            if let Some(op) = snap.op {
                assert!(
                    matches!(action, Action::Fetch { .. }),
                    "{label} is eligible for {name}, which is mid-{op}"
                );
            }
            if !snap.kind.has_worktree() {
                assert!(
                    matches!(action, Action::Fetch { .. } | Action::Custom { .. }),
                    "{label} is eligible for bare repository {name}"
                );
            }
        }
    }
}
