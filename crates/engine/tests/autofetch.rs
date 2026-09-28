//! Automatic fetching, against the engine.

use git_scylla_core::{Action, FetchSchedule, JobOrigin, JobState, Network, Outage, SkipReason};
use git_scylla_engine::{
    Config, Engine, EngineHandle, Event, FetchPolicy, FixedRoute, Plan, Policy, Selection,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

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

struct World {
    dir: PathBuf,
    repos: PathBuf,
}

fn world(dir: &Path, n: usize) -> World {
    std::fs::create_dir_all(dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let (repos, scratch) = (dir.join("repos"), dir.join("scratch"));
    for p in [&repos, &scratch] {
        std::fs::create_dir_all(p).unwrap();
    }
    git(&dir, &["init", "--bare", "-b", "main", "origin.git"]);
    let origin = dir.join("origin.git");
    git(&scratch, &["clone", origin.to_str().unwrap(), "seed"]);
    let seed = scratch.join("seed");
    std::fs::write(seed.join("a.txt"), "one\n").unwrap();
    git(&seed, &["add", "a.txt"]);
    git(&seed, &["commit", "-m", "c1"]);
    git(&seed, &["push", "-u", "origin", "main"]);
    for i in 0..n {
        git(&repos, &["clone", origin.to_str().unwrap(), &format!("r{i:02}")]);
    }
    World { dir, repos }
}

impl World {
    /// Point every repository at a remote that fails the way a machine with no
    /// network fails.
    ///
    /// `ext::` runs a command instead of opening a socket, so the outage is
    /// real git output with no DNS, no socket and nothing to flake. Returns
    /// the URLs it replaced, so the network can be brought back.
    fn go_offline(&self) -> Vec<(PathBuf, String)> {
        let script = self.dir.join("offline.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"ssh: connect to host git.example port 22: Network is unreachable\" >&2\nexit 128\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut was = Vec::new();
        for repo in self.paths() {
            let out = Command::new("git")
                .args(["remote", "get-url", "origin"])
                .current_dir(&repo)
                .output()
                .unwrap();
            was.push((repo.clone(), String::from_utf8_lossy(&out.stdout).trim().to_string()));
            git(&repo, &["remote", "set-url", "origin", &format!("ext::{}", script.display())]);
        }
        was
    }

    /// Give every repository a remote that reads as *somewhere else* while
    /// still serving this disk.
    ///
    /// A clone from a path has no host, and a repository with no host is not
    /// held for want of a network — correctly, since it never needed one. To
    /// test what *is* held, the remotes have to look remote. `ext::` runs
    /// git's own `upload-pack`, so these fetch for real.
    fn hosted_remotes(&self) {
        let origin = self.dir.join("origin.git");
        for repo in self.paths() {
            let url = format!("ext::git upload-pack {}", origin.display());
            git(&repo, &["remote", "set-url", "origin", &url]);
        }
    }

    fn come_back(&self, was: &[(PathBuf, String)]) {
        for (repo, url) in was {
            git(repo, &["remote", "set-url", "origin", url]);
        }
    }

    fn paths(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(&self.repos)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        out.sort();
        out
    }

    fn advance(&self) {
        let seed = self.dir.join("scratch/seed");
        let n = std::fs::read_to_string(seed.join("a.txt")).unwrap().len();
        std::fs::write(seed.join("a.txt"), format!("{n}\n")).unwrap();
        git(&seed, &["commit", "-am", "next"]);
        git(&seed, &["push", "origin", "main"]);
    }
}

fn config(interval: Duration) -> Config {
    Config {
        extra_env: vec![
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
            // For `World::go_offline`. Inert for every other test here.
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "protocol.ext.allow".into()),
            ("GIT_CONFIG_VALUE_0".into(), "always".into()),
        ],
        probe_timeout: Duration::from_secs(20),
        policy: Policy { max_snapshot_age: Duration::from_secs(86_400), ..Default::default() },
        fetch: FetchPolicy { interval, jitter_pct: 0, ..FetchPolicy::default() },
        fetch_tick: Duration::from_millis(200),
        ..Default::default()
    }
}

async fn wait_for(
    h: &EngineHandle,
    within: Duration,
    pred: impl Fn(&[git_scylla_core::RepoSnapshot]) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if pred(&h.snapshot().await.unwrap()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn behind_catches_up_with_no_user_action_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    let engine = Engine::start(config(Duration::from_millis(300)));
    let h = engine.handle();

    let snaps = h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap().snapshots;
    assert_eq!(snaps.len(), 3);
    assert!(snaps.iter().all(|s| s.upstream.as_ref().unwrap().behind() == Some(0)));

    w.advance();

    assert!(
        wait_for(&h, Duration::from_secs(30), |snaps| {
            snaps.iter().all(|s| s.upstream.as_ref().and_then(|u| u.behind()) == Some(1))
        })
        .await,
        "the working set never noticed the push"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_initial_scan_issues_no_network_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 4);
    let engine = Engine::start(config(Duration::from_millis(200)));
    let h = engine.handle();

    let mut events = h.subscribe();
    let scan = h.start_scan(vec![w.repos.clone()], false).await.unwrap();
    loop {
        match events.recv().await.unwrap() {
            Event::ScanDone { scan: id, .. } if id == scan => break,
            Event::JobStateChanged { id, repo, .. } => {
                panic!("job {id:?} against {} ran before the scan settled", repo.name())
            }
            _ => continue,
        }
    }
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_records_its_outcome_against_the_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    let engine = Engine::start(config(Duration::from_millis(300)));
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    assert!(
        wait_for(&h, Duration::from_secs(20), |snaps| {
            snaps[0].fetch.last_success.is_some()
                && matches!(snaps[0].fetch.schedule, FetchSchedule::Due(_))
        })
        .await,
        "a successful background fetch left no trace on the repository"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_remote_backs_off_and_keeps_its_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    let repo = w.repos.join("r00");
    let nowhere = w.dir.join("not-a-repo");
    std::fs::create_dir_all(&nowhere).unwrap();
    git(&repo, &["remote", "set-url", "origin", nowhere.to_str().unwrap()]);

    let engine = Engine::start(config(Duration::from_millis(200)));
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    assert!(
        wait_for(&h, Duration::from_secs(20), |snaps| {
            matches!(snaps[0].fetch.schedule, FetchSchedule::BackingOff { .. })
        })
        .await,
        "a failing remote was not backed off"
    );
    let snaps = h.snapshot().await.unwrap();
    match &snaps[0].fetch.schedule {
        FetchSchedule::BackingOff { failures, .. } => assert!(*failures >= 1),
        other => panic!("{other:?}"),
    }
    assert!(snaps[0].fetch.last_attempt.is_some());
    assert!(snaps[0].fetch.last_success.is_none());
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_with_no_remote_is_disabled_rather_than_perpetually_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let solo = tmp.path().join("repos/solo");
    std::fs::create_dir_all(&solo).unwrap();
    git(&solo, &["init", "-b", "main", "."]);
    std::fs::write(solo.join("a.txt"), "a\n").unwrap();
    git(&solo, &["add", "a.txt"]);
    git(&solo, &["commit", "-m", "c1"]);

    let root = tmp.path().join("repos").canonicalize().unwrap();
    let engine = Engine::start(config(Duration::from_millis(200)));
    let h = engine.handle();
    h.scan_to_completion(vec![root], false).await.unwrap();

    tokio::time::sleep(Duration::from_secs(2)).await;
    let snaps = h.snapshot().await.unwrap();
    assert_eq!(snaps[0].fetch.schedule, FetchSchedule::Disabled);
    assert!(snaps[0].fetch.last_attempt.is_none(), "it was attempted anyway");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_fetching_yields_while_a_user_batch_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    let engine = Engine::start(config(Duration::from_millis(150)));
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap().snapshots;

    let stall = Action::Custom {
        args: vec!["-c".into(), "alias.stall=!sh -c 'sleep 3'".into(), "stall".into()],
        network: true,
        mutating: true,
    };
    let plan = Plan {
        action: stall.clone(),
        eligible: snaps.iter().map(|s| (s.id.clone(), stall.clone())).collect(),
        skipped: vec![],
        considered: snaps.len(),
        warning: None,
    };

    let mut events = h.subscribe();
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    let mut background = Vec::new();
    loop {
        match events.recv().await.unwrap() {
            Event::BatchDone { id, .. } if id == batch => break,
            Event::JobStateChanged { origin: JobOrigin::Background, repo, .. } => {
                background.push(repo)
            }
            _ => continue,
        }
    }
    assert!(background.is_empty(), "background work ran during a user batch: {background:?}");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_off_switch_means_nothing_fetches() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 2);
    let engine = Engine::start(Config {
        fetch: FetchPolicy { enabled: false, ..config(Duration::from_millis(100)).fetch },
        ..config(Duration::from_millis(100))
    });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();
    w.advance();

    tokio::time::sleep(Duration::from_secs(2)).await;
    let snaps = h.snapshot().await.unwrap();
    assert!(snaps.iter().all(|s| s.fetch.last_attempt.is_none()), "something fetched anyway");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_users_own_fetch_clears_a_quarantine() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    let engine = Engine::start(Config {
        fetch: FetchPolicy { enabled: false, ..FetchPolicy::default() },
        ..config(Duration::from_secs(900))
    });
    let h = engine.handle();
    let snaps = h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap().snapshots;
    let id = snaps[0].id.clone();

    let plan = h
        .plan(
            Action::Fetch { prune: true, tags: false },
            git_scylla_engine::Selection::ids([id.clone()]),
        )
        .await
        .unwrap();
    assert_eq!(plan.eligible.len(), 1);
    let batch = h.start_batch(plan, JobOrigin::User).await.unwrap();
    let mut events = h.subscribe();
    while !matches!(events.recv().await, Ok(Event::BatchDone { id: b, .. }) if b == batch) {
        if h.jobs(batch).await.unwrap().iter().all(|j| j.state == JobState::Ok) {
            break;
        }
    }

    assert!(
        wait_for(&h, Duration::from_secs(10), move |snaps| {
            snaps.iter().any(|s| s.id == id && s.fetch.last_success.is_some())
        })
        .await,
        "a manual fetch left no trace"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_fetch_takes_the_ordinary_post_job_reprobe_path() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    let engine = Engine::start(config(Duration::from_millis(300)));
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();
    w.advance();

    assert!(
        wait_for(&h, Duration::from_secs(20), |snaps| {
            snaps[0].upstream.as_ref().and_then(|u| u.behind()) == Some(1)
        })
        .await,
        "the fetch happened but the row never caught up"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shrinking_the_interval_pulls_a_scheduled_fetch_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 1);
    let engine = Engine::start(config(Duration::from_secs(60)));
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    assert!(
        wait_for(&h, Duration::from_secs(20), |snaps| snaps[0].fetch.last_success.is_some()).await,
        "the first background fetch never completed"
    );

    h.set_fetch_interval(Duration::from_millis(300)).await.unwrap();
    w.advance();

    assert!(
        wait_for(&h, Duration::from_secs(10), |snaps| {
            snaps[0].upstream.as_ref().and_then(|u| u.behind()) == Some(1)
        })
        .await,
        "the shrunk interval was not honored until the original 60s interval had passed"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_transcripts_are_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 2);
    let engine =
        Engine::start(Config { background_history: 3, ..config(Duration::from_millis(120)) });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    tokio::time::sleep(Duration::from_secs(4)).await;
    let kept = h.background_jobs().await.unwrap();
    assert!(kept.len() <= 3, "kept {} background transcripts, bound is 3", kept.len());
    assert!(!kept.is_empty(), "the bound evicted everything");
    engine.shutdown().await;
}

/// Count the background fetches the engine starts over `window`.
async fn background_starts(h: &EngineHandle, window: Duration) -> usize {
    let mut events = h.subscribe();
    let mut started = 0;
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return started;
        }
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(Event::JobStateChanged {
                origin: JobOrigin::Background,
                state: JobState::Queued,
                ..
            })) => started += 1,
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => return started,
        }
    }
}

/// Every `NetworkChanged` on `events` over `window`.
async fn network_changes(
    mut events: tokio::sync::broadcast::Receiver<Event>,
    window: Duration,
) -> Vec<Network> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return seen;
        }
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(Event::NetworkChanged(n))) => seen.push(n),
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => return seen,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_network_stops_the_app_asking_again() {
    // Forty repositories cannot each discover the same closed lid. Once one
    // fetch has said the machine cannot get out, automatic fetching is held,
    // and a single repository is asked at the recheck interval.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 4);
    w.go_offline();
    let engine = Engine::start(Config {
        fetch: FetchPolicy {
            backoff: [Duration::from_millis(50); 4],
            // High enough that nothing stops fetching by being quarantined —
            // whatever holds it here has to be the outage.
            quarantine_after: 1000,
            recheck: Duration::from_secs(5),
            ..config(Duration::from_millis(100)).fetch
        },
        ..config(Duration::from_millis(100))
    });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    let started = background_starts(&h, Duration::from_secs(3)).await;
    assert!(started <= 8, "{started} fetches in three seconds with no network");
    assert!(
        h.snapshot().await.unwrap().iter().any(|s| s.fetch.last_attempt.is_some()),
        "nothing was tried at all, so nothing could have learned the network was down"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_network_never_quarantines_a_repository() {
    // Quarantine is for a repository that keeps refusing, and it is the user
    // who has to lift it — one per repository, by hand. Spending it on a
    // closed lid would silence a whole working set for a reason none of them
    // had any part in.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 4);
    w.go_offline();
    let engine = Engine::start(Config {
        fetch: FetchPolicy {
            backoff: [Duration::from_millis(50); 4],
            quarantine_after: 2,
            recheck: Duration::from_millis(200),
            ..config(Duration::from_millis(100)).fetch
        },
        ..config(Duration::from_millis(100))
    });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    tokio::time::sleep(Duration::from_secs(3)).await;

    let snaps = h.snapshot().await.unwrap();
    let quarantined: Vec<&str> = snaps
        .iter()
        .filter(|s| matches!(s.fetch.schedule, FetchSchedule::Quarantined { .. }))
        .map(|s| s.id.name())
        .collect();
    assert!(quarantined.is_empty(), "the outage quarantined {quarantined:?}");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fetching_resumes_when_the_network_comes_back() {
    // Nothing tells the app the network returned, so it has to keep one
    // question open: the recheck is the whole reason the hold is not a stop.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    let was = w.go_offline();
    let engine = Engine::start(Config {
        fetch: FetchPolicy {
            // The one fetch that established the verdict counted, so that
            // repository is backing off like any other failure. Short, so the
            // test is not waiting out a minute of it.
            backoff: [Duration::from_millis(50); 4],
            recheck: Duration::from_millis(200),
            ..config(Duration::from_millis(100)).fetch
        },
        ..config(Duration::from_millis(100))
    });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    assert!(
        wait_for(&h, Duration::from_secs(10), |snaps| {
            snaps.iter().filter(|s| s.fetch.last_attempt.is_some()).count() >= 2
        })
        .await,
        "the outage was never noticed"
    );

    w.come_back(&was);
    w.advance();

    assert!(
        wait_for(&h, Duration::from_secs(20), |snaps| {
            snaps.iter().all(|s| s.fetch.last_success.is_some())
        })
        .await,
        "fetching never resumed: {:#?}",
        h.snapshot().await.unwrap().iter().map(|s| s.fetch.clone()).collect::<Vec<_>>()
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_route_off_the_machine_nothing_is_even_spawned() {
    // The half a failed fetch cannot do: with no route there is nothing to
    // learn from trying, so nothing is tried, and no repository ends up with a
    // failure against its name for a fact about the machine.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 4);
    w.hosted_remotes();
    let route = FixedRoute::new(false);
    let engine = Engine::start(Config {
        routes: Arc::new(route.clone()),
        ..config(Duration::from_millis(100))
    });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    let started = background_starts(&h, Duration::from_secs(2)).await;
    assert_eq!(started, 0, "{started} fetches were spawned with no route off the machine");

    let snaps = h.snapshot().await.unwrap();
    assert!(snaps.iter().all(|s| s.fetch.last_attempt.is_none()), "something was attempted anyway");
    assert!(h.network().await.unwrap().is_down(), "the verdict never noticed");
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_refuses_once_rather_than_every_repository_failing() {
    // What the user sees before confirming: one reason, on every row that
    // needed a network, instead of forty transcripts of the same thing.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    w.hosted_remotes();
    let engine = Engine::start(Config {
        routes: Arc::new(FixedRoute::new(false)),
        fetch: FetchPolicy { enabled: false, ..config(Duration::from_secs(900)).fetch },
        ..config(Duration::from_secs(900))
    });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    let plan = h.plan(Action::Fetch { prune: true, tags: false }, Selection::All).await.unwrap();
    assert!(plan.eligible.is_empty(), "a fetch was planned with no route");
    // Automatic fetching is off, and the verdict still has to agree with the
    // plan: it is what the user is shown as the reason.
    assert_eq!(h.network().await.unwrap().outage(), Some(Outage::NoRoute));
    assert_eq!(plan.skipped.len(), 3);
    assert!(
        plan.skipped.iter().all(|(_, why)| *why == SkipReason::NoNetwork),
        "{:?}",
        plan.skipped
    );

    // And local work is untouched: the gate is about remotes, not about mood.
    let local = h.plan(Action::Stash { include_untracked: false }, Selection::All).await.unwrap();
    assert!(
        !local.skipped.iter().any(|(_, why)| *why == SkipReason::NoNetwork),
        "a stash was refused for want of a network"
    );
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_returning_route_starts_everything_again_without_a_fetch_to_prove_it() {
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    w.hosted_remotes();
    let route = FixedRoute::new(false);
    let engine = Engine::start(Config {
        routes: Arc::new(route.clone()),
        ..config(Duration::from_millis(100))
    });
    let h = engine.handle();
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(h.network().await.unwrap().is_down());

    route.set(true);

    assert!(
        wait_for(&h, Duration::from_secs(10), |snaps| {
            snaps.iter().all(|s| s.fetch.last_success.is_some())
        })
        .await,
        "plugging the machine back in did not restart fetching"
    );
    assert!(!h.network().await.unwrap().is_down());
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_on_this_disk_keeps_fetching_with_no_route_at_all() {
    // The gate is about remotes, not about mood. A clone from a path on this
    // machine is as reachable with the wifi off as with it on, and refusing it
    // would be refusing work that was never going to fail.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 2);
    let engine = Engine::start(Config {
        routes: Arc::new(FixedRoute::new(false)),
        ..config(Duration::from_millis(100))
    });
    let h = engine.handle();
    let changes = tokio::spawn(network_changes(h.subscribe(), Duration::from_secs(2)));
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();
    w.advance();

    assert!(
        wait_for(&h, Duration::from_secs(10), |snaps| {
            snaps.iter().all(|s| s.fetch.last_success.is_some())
        })
        .await,
        "a path remote was held for want of a network"
    );

    // And its successes say nothing about the network: the verdict went down
    // once and stayed there, rather than being lifted by every local fetch.
    // (The one change may land before the subscription does.)
    let changes = changes.await.unwrap();
    assert!(changes.len() <= 1, "the verdict flapped: {changes:?}");
    assert_eq!(h.network().await.unwrap().outage(), Some(Outage::NoRoute));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_on_this_disk_cannot_answer_for_the_network() {
    // A captive portal, and one repository cloned from a path. The path
    // fetches keep working, and none of them is allowed to lift the verdict —
    // which would send every hosted repository at the portal again.
    let tmp = tempfile::tempdir().unwrap();
    let w = world(tmp.path(), 3);
    let was = w.go_offline();
    w.come_back(&was[..1]);
    let engine = Engine::start(Config {
        fetch: FetchPolicy {
            backoff: [Duration::from_millis(50); 4],
            quarantine_after: 1000,
            recheck: Duration::from_millis(200),
            ..config(Duration::from_millis(100)).fetch
        },
        ..config(Duration::from_millis(100))
    });
    let h = engine.handle();
    let changes = tokio::spawn(network_changes(h.subscribe(), Duration::from_secs(3)));
    h.scan_to_completion(vec![w.repos.clone()], false).await.unwrap();

    let changes = changes.await.unwrap();
    assert!(!changes.is_empty(), "the outage was never noticed");
    assert!(changes.iter().all(|n| n.is_down()), "a path remote lifted the verdict: {changes:?}");
    assert_eq!(h.network().await.unwrap().outage(), Some(Outage::Unreachable));
    engine.shutdown().await;
}
