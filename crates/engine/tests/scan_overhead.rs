//! Where the time in a scan actually goes.
//!
//! Not a benchmark to keep: a diagnostic, kept because the scan has a budget
//! and "it feels fine" is not a measurement.

use git_scylla_core::{FetchHealth, Head, ProbeOutcome, RepoKind, RepoSnapshot, WorkTree};
use git_scylla_engine::{Config, Engine};
use git_scylla_probe::{BoxFuture, Probe, ProbeRequest};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// A probe that does no work at all, so what is left is the engine.
struct Instant0;

impl Probe for Instant0 {
    fn probe<'a>(&'a self, req: ProbeRequest) -> BoxFuture<'a, RepoSnapshot> {
        Box::pin(async move {
            RepoSnapshot {
                id: req.found.id.clone(),
                path: req.found.path.clone(),
                kind: RepoKind::Normal,
                head: Head::Branch("main".into()),
                head_oid: None,
                upstream: None,
                remotes: vec![],
                work: WorkTree::default(),
                op: None,
                stashes: 0,
                fetch: FetchHealth::disabled(),
                probed_at: SystemTime::now(),
                outcome: ProbeOutcome::Ok,
                from_cache: false,
                watched: false,
            }
        })
    }
}

fn make(dir: &std::path::Path, n: usize) -> std::path::PathBuf {
    let root = dir.join("repos");
    for i in 0..n {
        // A bare `.git` directory is enough for discovery; the probe is fake.
        std::fs::create_dir_all(root.join(format!("r{i:03}/.git"))).unwrap();
    }
    root.canonicalize().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_itself_costs_almost_nothing_for_a_hundred_repositories() {
    let tmp = tempfile::tempdir().unwrap();
    let root = make(tmp.path(), 100);

    let mut best = Duration::MAX;
    for _ in 0..5 {
        let engine = Engine::with_probe(Config::default(), Arc::new(Instant0));
        let start = Instant::now();
        let outcome = engine.handle().scan_to_completion(vec![root.clone()], false).await.unwrap();
        best = best.min(start.elapsed());
        assert_eq!(outcome.snapshots.len(), 100);
        engine.shutdown().await;
    }

    eprintln!("engine overhead for 100 repositories: {best:?}");
    // Generous, because this is a floor check and not a benchmark: if the
    // engine's own bookkeeping were the reason a scan missed its budget, it
    // would be nowhere near this.
    assert!(best < Duration::from_millis(100), "engine overhead alone is {best:?}");
}
