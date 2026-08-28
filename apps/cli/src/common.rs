//! The four things every verb does before it can do its own.
//!
//! Parse a selection, start a scan and wait for it, size the semaphores, print
//! JSON. Each was written out at every entry point, which is four places for
//! `--select` to reject an expression differently and four exit codes to keep in
//! step. The *decisions* after a scan still belong to each verb — `scan` treats
//! an unreadable root as fatal only when it found nothing, `fetch --daemon`
//! never does — so only the part that is genuinely the same is here.

use git_scylla_core::RepoSnapshot;
use git_scylla_engine::{EngineHandle, Limits, ScanOutcome, Selection};
use serde::Serialize;
use std::path::PathBuf;
use std::process::ExitCode;

/// Cannot run at all, as against "ran and found nothing".
///
/// Every verb uses this for a usage error, an unstartable engine and an
/// unserializable result alike: the distinction the exit code has to carry is
/// "could not run" versus "ran", and finer grades would be a contract nobody
/// asked for.
pub const CANNOT_RUN: u8 = 3;

/// Parse a `--select` argument, or select everything.
///
/// One grammar for `scan --filter`, every mutating verb's `--select`, and the
/// GUI's filter box: `Selection::parse` is the only parser, and `all` and `*`
/// mean everything wherever they are typed.
pub fn selection(expr: Option<&str>) -> Result<Selection, ExitCode> {
    let Some(expr) = expr else { return Ok(Selection::All) };
    // Passed in rather than read inside the parser, which stays a pure function
    // of its arguments so its tests stay honest.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Selection::parse(expr, home.as_deref()).map_err(|e| {
        eprintln!("error: bad --select: {e}");
        ExitCode::from(CANNOT_RUN)
    })
}

/// Walk and probe the roots, waiting for the scan to settle.
///
/// Discovery errors are reported here because every verb reports them
/// identically; what they *mean* is left to the caller, because that is the
/// part that differs.
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
/// `.max(1)` because `--concurrency 0` is a request to do nothing, which as a
/// semaphore size is a deadlock rather than an empty run.
pub fn limits(concurrency: Option<usize>, per_host: Option<usize>) -> Limits {
    let defaults = Limits::default();
    Limits {
        network: concurrency.unwrap_or(defaults.network).max(1),
        per_host: per_host.unwrap_or(defaults.per_host).max(1),
        ..defaults
    }
}

/// A `--json` document on stdout.
///
/// Pretty-printed because the alternative is a single enormous line, and `jq`
/// does not care either way. Stable field order comes from the struct
/// definitions; serde_json preserves it.
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

/// Report what a scan could not read, once the caller has decided it is fatal.
///
/// Finding nothing under a readable root is a report, not a failure. Finding
/// nothing *and* being unable to read something is a configuration problem, and
/// the two must never be told apart by the user squinting at stderr.
pub fn found_nothing_fatally(outcome: &ScanOutcome) -> bool {
    outcome.snapshots.is_empty() && !outcome.errors.is_empty()
}

/// Everything the selection matches, in the order the engine reports.
pub fn matching<'a>(snapshots: &'a [RepoSnapshot], selection: &Selection) -> Vec<&'a RepoSnapshot> {
    snapshots.iter().filter(|s| selection.contains(s)).collect()
}
