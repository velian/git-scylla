//! The four things every verb does before it can do its own: parse a
//! selection, run a scan to completion, size the semaphores, print JSON.
//! What a verb does with the result is its own.

use git_scylla_core::RepoSnapshot;
use git_scylla_engine::{EngineHandle, Limits, ScanOutcome, Selection};
use serde::Serialize;
use std::path::PathBuf;
use std::process::ExitCode;

/// Cannot run at all, as against "ran and found nothing".
pub const CANNOT_RUN: u8 = 3;

/// Parse a `--select` argument, or select everything.
pub fn selection(expr: Option<&str>) -> Result<Selection, ExitCode> {
    let Some(expr) = expr else { return Ok(Selection::All) };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Selection::parse(expr, home.as_deref()).map_err(|e| {
        eprintln!("error: bad --select: {e}");
        ExitCode::from(CANNOT_RUN)
    })
}

/// Walk and probe the roots, waiting for the scan to settle.
pub async fn scan(
    handle: &EngineHandle,
    roots: &[PathBuf],
    nested: bool,
) -> Result<ScanOutcome, ExitCode> {
    let outcome = handle.scan_to_completion(roots.to_vec(), nested).await.map_err(|e| {
        eprintln!("error: {e}");
        ExitCode::from(CANNOT_RUN)
    })?;
    for e in &outcome.errors {
        eprintln!("error: {e}");
    }
    Ok(outcome)
}

/// The concurrency limits, with the flags applied over the defaults.
///
/// `--concurrency 0` is clamped to 1: as a semaphore size, 0 deadlocks rather
/// than running nothing.
pub fn limits(concurrency: Option<usize>, per_host: Option<usize>) -> Limits {
    let defaults = Limits::default();
    Limits {
        network: concurrency.unwrap_or(defaults.network).max(1),
        per_host: per_host.unwrap_or(defaults.per_host).max(1),
        ..defaults
    }
}

/// A pretty-printed `--json` document on stdout.
pub fn emit_json<T: Serialize>(value: &T) -> Result<(), ExitCode> {
    match serde_json::to_string_pretty(value) {
        Ok(s) => {
            println!("{s}");
            Ok(())
        }
        Err(e) => {
            eprintln!("error: could not serialize: {e}");
            Err(ExitCode::from(CANNOT_RUN))
        }
    }
}

/// True when the scan found nothing and also could not read something.
pub fn found_nothing_fatally(outcome: &ScanOutcome) -> bool {
    outcome.snapshots.is_empty() && !outcome.errors.is_empty()
}

/// Everything the selection matches, in the order the engine reports.
pub fn matching<'a>(snapshots: &'a [RepoSnapshot], selection: &Selection) -> Vec<&'a RepoSnapshot> {
    snapshots.iter().filter(|s| selection.contains(s)).collect()
}
