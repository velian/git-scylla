//! `git-scylla fetch --daemon` and `git-scylla status`.

use crate::{common, render};
use git_scylla_core::{FetchSchedule, JobOrigin, JobState, RepoSnapshot};
use git_scylla_engine::{CacheMode, Config, Engine, Event, FetchPolicy};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

pub struct DaemonArgs {
    pub roots: Vec<PathBuf>,
    pub nested: bool,
    /// Per-repository interval. Overrides the default 15 minutes.
    pub interval: Option<u64>,
    pub concurrency: Option<usize>,
    pub per_host: Option<usize>,
}

/// Run the scheduler in the foreground, logging every decision. Runs until
/// Ctrl-C.
pub async fn run(args: DaemonArgs) -> ExitCode {
    let fetch = FetchPolicy {
        interval: args.interval.map(Duration::from_secs).unwrap_or(FetchPolicy::default().interval),
        ..FetchPolicy::default()
    };
    let engine = Engine::start(Config {
        nested: args.nested,
        limits: common::limits(args.concurrency, args.per_host),
        fetch: fetch.clone(),
        cache: CacheMode::ReadWrite,
        ..Default::default()
    });
    let h = engine.handle();
    let mut events = h.subscribe();

    eprintln!("scanning {:?} — nothing fetches until the scan settles", args.roots);
    let outcome = match common::scan(&h, &args.roots, args.nested).await {
        Ok(o) => o,
        Err(code) => return code,
    };
    if outcome.snapshots.is_empty() {
        eprintln!("no repositories found under {:?}", args.roots);
        engine.shutdown().await;
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "watching {} repositories, every {} \u{00b1}{}%\n",
        outcome.snapshots.len(),
        git_scylla_core::duration::brief(fetch.interval),
        fetch.jitter_pct
    );
    print_schedule(&outcome.snapshots, SystemTime::now());

    // Ctrl-C stops the daemon rather than killing the process, so an
    // in-flight `git fetch` and its `ssh` exit cleanly.
    let interrupt = tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    tokio::pin!(interrupt);

    loop {
        tokio::select! {
            _ = &mut interrupt => {
                eprintln!("\nstopping — letting in-flight fetches finish");
                break;
            }
            received = events.recv() => match received {
                Ok(event) => log_event(&event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("(dropped {n} events)");
                }
                Err(_) => break,
            },
        }
    }
    engine.shutdown().await;
    ExitCode::SUCCESS
}

/// One line per background decision. User-initiated batches are not logged
/// here; their own progress output already shows them.
fn log_event(event: &Event) {
    match event {
        Event::JobStateChanged { origin: JobOrigin::Background, repo, state, .. } => {
            let word = match state {
                JobState::Queued => return,
                JobState::Running => "fetching",
                JobState::Ok => "ok",
                JobState::Failed { .. } => "failed",
                JobState::Cancelled => "cancelled",
                JobState::Skipped { .. } => return,
            };
            println!("{}  {:<28} {word}", stamp(), repo.name());
        }
        Event::ReposUpserted(snaps) => {
            for s in snaps {
                match &s.fetch.schedule {
                    FetchSchedule::BackingOff { until, failures } => println!(
                        "{}  {:<28} backing off ({failures}) until {}",
                        stamp(),
                        s.id.name(),
                        local(*until)
                    ),
                    FetchSchedule::Quarantined { last_error, .. } => println!(
                        "{}  {:<28} quarantined: {}",
                        stamp(),
                        s.id.name(),
                        last_error.lines().next().unwrap_or(last_error)
                    ),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// What the scheduler intends, before it does any of it.
fn print_schedule(snaps: &[RepoSnapshot], now: SystemTime) {
    let mut rows: Vec<(&str, String)> =
        snaps.iter().map(|s| (s.id.name(), render::fetch_cell(s, now))).collect();
    rows.sort();
    for (name, when) in rows {
        println!("{name:<30} {when}");
    }
    let _ = std::io::stdout().flush();
}

// ---- `git-scylla status` ------------------------------------------------

pub struct StatusArgs {
    pub roots: Vec<PathBuf>,
    pub nested: bool,
    pub select: Option<String>,
    /// Only repositories whose fetch health is not healthy.
    pub stale_only: bool,
    pub json: bool,
}

/// Fetch health per repository.
pub async fn status(args: StatusArgs) -> ExitCode {
    let selection = match common::selection(args.select.as_deref()) {
        Ok(s) => s,
        Err(code) => return code,
    };

    // CacheMode::Read: a one-shot command must not overwrite the daemon's
    // recorded fetch health with whatever roots were on its own command line.
    let engine =
        Engine::start(Config { nested: args.nested, cache: CacheMode::Read, ..Default::default() });
    let h = engine.handle();
    let outcome = match common::scan(&h, &args.roots, args.nested).await {
        Ok(o) => o,
        Err(code) => return code,
    };
    engine.shutdown().await;

    let now = SystemTime::now();
    let rows: Vec<&RepoSnapshot> = common::matching(&outcome.snapshots, &selection)
        .into_iter()
        .filter(|s| !args.stale_only || !healthy(s))
        .collect();

    if args.json {
        let out: Vec<_> = rows
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.id.name(),
                    "path": s.path,
                    "behind": s.upstream.as_ref().and_then(|u| u.behind()),
                    "fetch": s.fetch,
                    "health": render::fetch_cell(s, now),
                })
            })
            .collect();
        return match common::emit_json(&out) {
            Ok(()) => ExitCode::SUCCESS,
            Err(code) => code,
        };
    }

    if rows.is_empty() {
        println!(
            "{}",
            if args.stale_only { "every repository is up to date" } else { "nothing selected" }
        );
        return ExitCode::SUCCESS;
    }
    println!("{:<30} {:>8}  FETCH", "REPOSITORY", "BEHIND");
    for s in rows {
        let behind = match s.upstream.as_ref().and_then(|u| u.behind()) {
            Some(n) => n.to_string(),
            None => "-".to_string(),
        };
        println!("{:<30} {behind:>8}  {}", s.id.name(), render::fetch_cell(s, now));
    }
    ExitCode::SUCCESS
}

/// Is the scheduler content with this repository? A repository with no
/// remote (`Disabled`) counts as healthy.
fn healthy(s: &RepoSnapshot) -> bool {
    matches!(s.fetch.schedule, FetchSchedule::Due(_) | FetchSchedule::Disabled)
}

fn stamp() -> String {
    local(SystemTime::now())
}

/// Seconds-since-midnight UTC, which is enough to read a cycle by and needs no
/// date library.
fn local(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}
