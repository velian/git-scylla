use crate::{common, render, Sort};
use git_scylla_core::RepoSnapshot;
use git_scylla_engine::{Config, Engine, Limits};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

pub struct Args {
    pub roots: Vec<PathBuf>,
    pub json: bool,
    pub nested: bool,
    pub timeout_ms: u64,
    pub concurrency: Option<usize>,
    pub sort: Sort,
    pub filter: Option<String>,
    pub max_depth: Option<usize>,
}

pub async fn run(args: Args) -> ExitCode {
    // `Selection` and not a bare `Filter`: `--filter` and `--select` are the
    // same grammar with two names, so `all` and `*` have to mean the same thing
    // here as they do on a mutating verb.
    let selection = match common::selection(args.filter.as_deref()) {
        Ok(s) => s,
        Err(code) => return code,
    };

    // The same engine the mutating verbs use, rather than a second walk-and-probe
    // pipeline of the CLI's own. Two of them meant two places to keep the probe
    // deadline, the concurrency and the re-probe rules in step — and that
    // orchestration belongs in a library crate, not in a surface.
    let engine = Engine::start(Config {
        nested: args.nested,
        max_depth: args.max_depth,
        probe_timeout: Duration::from_millis(args.timeout_ms),
        limits: Limits {
            local: args.concurrency.unwrap_or_else(|| Limits::default().local).max(1),
            ..Limits::default()
        },
        ..Default::default()
    });
    let handle = engine.handle();

    let outcome = match common::scan(&handle, &args.roots, args.nested).await {
        Ok(o) => o,
        Err(code) => return code,
    };
    engine.shutdown().await;

    // A root that could not be walked is a configuration error, and the only
    // thing `scan` treats as one. Finding nothing under a readable root is a
    // report, not a failure.
    if common::found_nothing_fatally(&outcome) {
        return ExitCode::from(common::CANNOT_RUN);
    }

    let mut rows: Vec<RepoSnapshot> =
        outcome.snapshots.into_iter().filter(|s| selection.contains(s)).collect();
    sort_rows(&mut rows, args.sort);

    if args.json {
        if let Err(code) = common::emit_json(&rows) {
            return code;
        }
    } else {
        render::table(&rows);
    }

    // `scan` is a report, not a check: it exits 0 even with nothing found and
    // even with failed probes, both of which are visible in the output. Non-zero
    // is reserved for being unable to run at all.
    ExitCode::SUCCESS
}

fn sort_rows(rows: &mut [RepoSnapshot], sort: Sort) {
    match sort {
        // Badge order is the priority order; path breaks ties so the output is
        // stable across runs.
        Sort::Badge => rows.sort_by(|a, b| a.badge().cmp(&b.badge()).then(a.path.cmp(&b.path))),
        Sort::Path => rows.sort_by(|a, b| a.path.cmp(&b.path)),
        Sort::Branch => rows.sort_by(|a, b| a.branch().cmp(&b.branch()).then(a.path.cmp(&b.path))),
    }
}
