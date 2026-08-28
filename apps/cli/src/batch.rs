//! The mutating verbs: scan, plan, confirm, execute, report.

use crate::common;
use crate::progress::Progress;
use crate::store::{self, LastRun};
use crate::BatchArgs;
use git_scylla_core::{Action, BatchId, BatchSummary, Job, JobOrigin, JobState, LogLine};
use git_scylla_engine::{Config, ConfirmGuard, Engine, EngineHandle, Event, Plan};
use serde::Serialize;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const JSON_LOG_LINES: usize = 200;

#[derive(Serialize)]
struct BatchResult {
    batch: BatchId,
    action: String,
    command: String,
    summary: BatchSummary,
    jobs: Vec<JobResult>,
}

#[derive(Serialize)]
struct JobResult {
    id: git_scylla_core::JobId,
    name: String,
    path: PathBuf,
    state: JobState,
    head_before: Option<git_scylla_core::Oid>,
    duration_ms: Option<u64>,
    log: Vec<LogLine>,
    log_truncated: bool,
}

pub async fn run(action: Action, args: BatchArgs) -> ExitCode {
    let selection = match common::selection(args.select.as_deref()) {
        Ok(s) => s,
        Err(code) => return code,
    };

    let engine = Engine::start(Config {
        nested: args.nested,
        limits: common::limits(args.concurrency, args.per_host),
        ..Default::default()
    });
    let handle = engine.handle();

    let outcome = match common::scan(&handle, &args.roots, args.nested).await {
        Ok(o) => o,
        Err(code) => return code,
    };
    if outcome.snapshots.is_empty() {
        engine.shutdown().await;
        if common::found_nothing_fatally(&outcome) {
            return ExitCode::from(common::CANNOT_RUN);
        }
        eprintln!("no repositories found under {:?}", args.roots);
        return ExitCode::SUCCESS;
    }

    let plan = match handle.plan(action.clone(), selection).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(common::CANNOT_RUN);
        }
    };

    // stdout for --dry-run, since it is the output; stderr otherwise, since
    // stdout is reserved for --json.
    if args.dry_run {
        print!("{}", plan.render());
        engine.shutdown().await;
        return ExitCode::SUCCESS;
    }
    eprint!("{}", plan.render());

    if plan.is_empty() {
        engine.shutdown().await;
        return ExitCode::SUCCESS;
    }

    if !args.yes {
        match confirm(&plan) {
            Confirmed::Yes => {}
            Confirmed::No => {
                eprintln!("cancelled");
                engine.shutdown().await;
                return ExitCode::from(2);
            }
            Confirmed::CannotAsk => {
                eprintln!(
                    "error: refusing to run without confirmation.\n\
                     stdin is not a terminal, so there is nobody to ask. Pass -y to \
                     confirm, or --dry-run to see the plan."
                );
                engine.shutdown().await;
                return ExitCode::from(common::CANNOT_RUN);
            }
        }
    }

    let (batch, summary, jobs) = execute(&handle, plan.clone()).await;
    engine.shutdown().await;

    store::save(&LastRun { batch, action: action.clone(), jobs: jobs.clone() });

    if args.json {
        let result = BatchResult {
            batch,
            action: action.label(),
            command: action.to_string(),
            summary,
            jobs: jobs.iter().map(job_result).collect(),
        };
        if let Err(code) = common::emit_json(&result) {
            return code;
        }
    } else {
        print_summary(&summary, &jobs);
    }

    ExitCode::from(summary.exit_code())
}

async fn execute(handle: &EngineHandle, plan: Plan) -> (BatchId, BatchSummary, Vec<Job>) {
    let total = plan.eligible.len();
    let mut events = handle.subscribe();
    let batch = handle.start_batch(plan, JobOrigin::User).await.expect("engine running");
    let mut progress = Progress::new(total);
    let mut summary = BatchSummary::default();

    // Ctrl-C cancels the batch rather than killing the process, so `git` and
    // any `ssh` it spawned exit cleanly instead of being orphaned.
    let interrupt = {
        let (handle, batch) = (handle.clone(), batch);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\ninterrupted — cancelling the batch and stopping git cleanly");
                let _ = handle.cancel_batch(batch).await;
            }
        })
    };

    loop {
        match events.recv().await {
            Ok(Event::JobStateChanged { repo, state, .. }) => match state {
                JobState::Running => progress.started(repo),
                s if s.is_terminal() => progress.finished(&repo, &s),
                _ => {}
            },
            Ok(Event::BatchDone { id, summary: s }) if id == batch => {
                summary = s;
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
    progress.erase();
    interrupt.abort();

    let jobs = handle.jobs(batch).await.unwrap_or_default();
    (batch, summary, jobs)
}

enum Confirmed {
    Yes,
    No,
    CannotAsk,
}

/// Ask before mutating. Never assumes yes.
fn confirm(plan: &Plan) -> Confirmed {
    if !std::io::stdin().is_terminal() {
        return Confirmed::CannotAsk;
    }
    let n = plan.eligible.len();
    let (question, accepts) = match plan.view().confirm_guard {
        Some(ConfirmGuard::TypeCount(count)) => (
            format!(
                "\nThis cannot be undone: the remote accepts the push and the \
                 overwritten commits are gone.\nType {count} to proceed, or \
                 anything else to stop: "
            ),
            Accepts::Exactly(count.to_string()),
        ),
        Some(ConfirmGuard::Acknowledge(what)) => {
            (format!("\n{what}.\nType 'yes' to proceed: "), Accepts::Yes)
        }
        None => (format!("\nProceed with {n} {}? [y/N] ", unit(n)), Accepts::YesOrY),
    };
    match ask(&question) {
        Some(answer) if accepts.met_by(&answer) => Confirmed::Yes,
        // Unreadable stdin is a no, not an error: no answer is not consent.
        _ => Confirmed::No,
    }
}

/// What a prompt will take for a yes. Anything else is a no.
enum Accepts {
    /// This exact string, case and all.
    Exactly(String),
    /// `yes`, in any case.
    Yes,
    /// `y` or `yes`, in any case.
    YesOrY,
}

impl Accepts {
    fn met_by(&self, answer: &str) -> bool {
        let answer = answer.trim();
        match self {
            Accepts::Exactly(want) => answer == want,
            Accepts::Yes => answer.eq_ignore_ascii_case("yes"),
            Accepts::YesOrY => matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"),
        }
    }
}

/// Put a question on stderr and read the answer. `None` if stdin is unreadable.
fn ask(question: &str) -> Option<String> {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line)
}

fn unit(n: usize) -> &'static str {
    if n == 1 {
        "repository"
    } else {
        "repositories"
    }
}

fn print_summary(summary: &BatchSummary, jobs: &[Job]) {
    println!();
    println!("{}", summary.render());

    let failed: Vec<&Job> =
        jobs.iter().filter(|j| matches!(j.state, JobState::Failed { .. })).collect();
    if !failed.is_empty() {
        println!("\nfailed:");
        for job in &failed {
            match git_scylla_core::explain(&job.log) {
                Some(e) => {
                    println!("  {:<30} {}", job.repo.name(), e.kind);
                    println!("  {:<30} {}", "", e.evidence);
                    if let Some(remedy) = &e.remedy {
                        println!("  {:<30} \u{2192} {remedy}", "");
                    }
                }
                None => println!("  {:<30} failed with no output", job.repo.name()),
            }
            println!("  {:<30} git-scylla log {}", "", job.id.0);
        }
    }
}

fn job_result(job: &Job) -> JobResult {
    let truncated = job.log.len() > JSON_LOG_LINES;
    let log = if truncated {
        // Keep the tail: a failed job's explanation is at the end.
        job.log[job.log.len() - JSON_LOG_LINES..].to_vec()
    } else {
        job.log.clone()
    };
    JobResult {
        id: job.id,
        name: job.repo.name().to_string(),
        path: job.repo.path().to_path_buf(),
        state: job.state.clone(),
        head_before: job.head_before.clone(),
        duration_ms: job.duration().map(|d| d.as_millis() as u64),
        log,
        log_truncated: truncated,
    }
}

/// `git-scylla log <JOB_ID>`.
pub fn print_log(job_id: u64) -> ExitCode {
    let Some(run) = store::load() else {
        eprintln!(
            "no transcripts available. They are written by the last `fetch` or \
             `pull` run in this session's state directory."
        );
        return ExitCode::from(common::CANNOT_RUN);
    };
    let Some(job) = run.jobs.iter().find(|j| j.id.0 == job_id) else {
        eprintln!("no job {job_id} in the last run. Jobs from that run:");
        for j in &run.jobs {
            eprintln!("  {:>4}  {:<30} {}", j.id.0, j.repo.name(), j.state);
        }
        return ExitCode::from(common::CANNOT_RUN);
    };

    println!("job {} — {} — {}", job.id.0, job.repo, job.state);
    println!("{}", job.action);
    if let Some(head) = &job.head_before {
        println!("HEAD before: {head}");
    }
    println!();
    if job.log.is_empty() {
        println!("(no output)");
    }
    for line in &job.log {
        // Timestamp relative to the job's start.
        let at = job
            .started_at
            .and_then(|s| line.at.duration_since(s).ok())
            .map(|d| format!("{:>7.3}s", d.as_secs_f64()))
            .unwrap_or_else(|| "       ".into());
        println!("{at} {} {}", line.stream, line.text);
    }
    ExitCode::SUCCESS
}

/// Every distinct repository the last run touched, for `log` with no argument.
pub fn list_jobs() -> ExitCode {
    let Some(run) = store::load() else {
        eprintln!("no transcripts available");
        return ExitCode::from(common::CANNOT_RUN);
    };
    println!("last run: batch {} — {}", run.batch, run.action.label());
    for j in &run.jobs {
        println!("  {:>4}  {:<30} {}", j.id.0, j.repo.name(), j.state);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_push_takes_the_count_and_nothing_else() {
        let accepts = Accepts::Exactly("31".to_string());
        assert!(accepts.met_by("31"));
        assert!(accepts.met_by("  31\n"));
        assert!(!accepts.met_by("y"));
        assert!(!accepts.met_by("yes"));
        assert!(!accepts.met_by("30"));
        assert!(!accepts.met_by(""));
    }

    #[test]
    fn an_acknowledgement_takes_the_word_but_not_the_letter() {
        assert!(Accepts::Yes.met_by("yes"));
        assert!(Accepts::Yes.met_by("YES\n"));
        assert!(!Accepts::Yes.met_by("y"));
        assert!(!Accepts::Yes.met_by(""));
    }

    #[test]
    fn an_ordinary_plan_takes_either_but_still_nothing_else() {
        assert!(Accepts::YesOrY.met_by("y"));
        assert!(Accepts::YesOrY.met_by("Yes"));
        assert!(!Accepts::YesOrY.met_by("sure"));
        assert!(!Accepts::YesOrY.met_by(""));
    }
}
