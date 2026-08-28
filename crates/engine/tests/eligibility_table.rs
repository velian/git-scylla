//! Every action against every fixture, as a table.
//!
//! The cross product of every `Action` variant against every fixture snapshot.
//! This table *is* the specification of the tool's safety.
//!
//! So it is checked in as a table and not as assertions. A change to any rule
//! shows up as a diff a reviewer reads line by line, which is the point — the
//! danger with preconditions is not that one is wrong in isolation but that a
//! fix for one repository state quietly changes the answer for six others.
//!
//! Regenerate with `UPDATE_ELIGIBILITY_TABLE=1 cargo test -p git-scylla-engine`
//! and **read the diff**.

use git_scylla_core::SkipReason;
use git_scylla_core::{Action, PullMode, RepoSnapshot};
use git_scylla_discovery::{WalkOptions, Walker};
use git_scylla_engine::{evaluate, Eligibility, Policy};
use git_scylla_probe::{GitCliProbe, Probe, ProbeRequest};
use git_scylla_testkit::FixtureSet;
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

const GOLDEN: &str = include_str!("eligibility_table.txt");

/// The columns, in order. Each is one concrete `Action`.
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
        // The template, `plan: None` — which is what `evaluate` sees, because
        // the engine resolves the branch only after the preconditions have
        // already narrowed the set.
        ("sync", Action::SyncDefault { mode: PullMode::FfOnly, plan: None }),
        // The template again: the name is derived after the preconditions have
        // narrowed the set, so this is the shape `evaluate` actually sees.
        (
            "devtag",
            Action::DevTag {
                channel: "dev".into(),
                bump: git_scylla_core::version::Bump::Minor,
                name: None,
                push: Some("origin".into()),
            },
        ),
        // Undo's repair step. The commit is a stand-in — what the table is
        // asserting is which *repository states* admit a reset at all, and the
        // fixtures never point HEAD at this one.
        (
            "undo",
            Action::Reset {
                to: git_scylla_core::Oid::parse("0123456").expect("static oid"),
                mode: git_scylla_core::ResetMode::Hard,
            },
        ),
    ]
}

/// Short, greppable codes. Deliberately terse so the table stays readable at a
/// glance; the long form is `SkipReason`'s `Display`.
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
    // Staleness is a clock rule and has its own unit tests; holding it out here
    // keeps the table about the *action* rules rather than about how long the
    // fixture build took.
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
                code(&evaluate(action, snap, now, &policy)),
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
    // A blunt backstop under the table above, phrased as invariants rather than
    // as cells so that it survives the table being regenerated. If someone
    // accepts a bad diff, these still fail.
    let tmp = tempfile::tempdir().unwrap();
    let set = FixtureSet::build(tmp.path()).expect("fixtures");
    let snapshots = probe_fixtures(&set.scan_root).await;
    let policy = Policy { max_snapshot_age: Duration::from_secs(86_400), ..Default::default() };
    let now = SystemTime::now();

    for (name, snap) in &snapshots {
        for (label, action) in columns() {
            let eligible = evaluate(&action, snap, now, &policy).is_eligible();
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
            // Nothing but fetch may run mid-operation. Custom is blocked too,
            // by the universal rules, so fetch is alone here.
            if let Some(op) = snap.op {
                assert!(
                    matches!(action, Action::Fetch { .. }),
                    "{label} is eligible for {name}, which is mid-{op}"
                );
            }
            // Nothing that needs a worktree may run without one.
            if !snap.kind.has_worktree() {
                assert!(
                    matches!(action, Action::Fetch { .. } | Action::Custom { .. }),
                    "{label} is eligible for bare repository {name}"
                );
            }
        }
    }
}
