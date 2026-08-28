//! Executing one job: the step loop, compensation, and `head_before`.
//!
//! Everything here goes through `crates/exec`, so every spawned `git` inherits
//! the hardened environment, its own process group and a deadline.

use git_scylla_core::{Action, JobState, LogLine, Oid, Pass, Step, StepRun, StepState};
use git_scylla_exec::{GitCommand, Stop};
use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// What running a job produced.
pub struct JobOutcome {
    pub state: JobState,
    pub steps: Vec<StepRun>,
    pub log: Vec<LogLine>,
    pub head_before: Option<Oid>,
    /// Where the job left `HEAD`. Read after the steps, for the same reason
    /// `head_before` is read before them.
    pub head_after: Option<Oid>,
}

/// Run every step of `action` against `repo`.
///
/// The step loop is general even though almost every action has one step:
/// exercising the multi-step path routinely is what keeps it from being dead
/// code that first runs in anger.
pub async fn run_job(
    repo: &Path,
    action: &Action,
    per_step_timeout: Duration,
    cancel: &CancellationToken,
    extra_env: &[(OsString, OsString)],
) -> JobOutcome {
    let mut log: Vec<LogLine> = Vec::new();

    // Before anything mutates. This one value is what makes undo real.
    let head_before =
        if action.is_mutating() { head_of(repo, cancel, extra_env).await } else { None };

    let steps = action.steps();
    let mut runs: Vec<StepRun> = Vec::new();
    let mut failure: Option<JobState> = None;
    // How much of the forward pass got far enough to owe a cleanup.
    let mut completed = 0usize;

    for (i, step) in steps.iter().enumerate() {
        if failure.is_some() {
            // An earlier step failed. `NotRun` rather than `Cancelled`: the
            // distinction is why StepState exists separately from JobState.
            runs.push(StepRun {
                step: step.clone(),
                pass: Pass::Forward,
                state: StepState::NotRun,
                log: log.len()..log.len(),
            });
            continue;
        }
        let (state, range) =
            run_step(repo, step, per_step_timeout, cancel, extra_env, &mut log).await;
        let failed = !matches!(state, StepState::Ok);
        runs.push(StepRun {
            step: step.clone(),
            pass: Pass::Forward,
            state: state.clone(),
            log: range,
        });
        if failed {
            failure = Some(match state {
                StepState::Cancelled => JobState::Cancelled,
                StepState::Failed { code } => JobState::Failed { code },
                // A timeout leaves no exit code; report it as a failure with the
                // conventional signal-terminated code rather than inventing a
                // new JobState the plan cannot render.
                _ => JobState::Failed { code: -1 },
            });
        } else {
            completed = i + 1;
        }
    }

    // After the forward pass, and **whether or not it failed**.
    // A step that opened something owes the close either way: `Commit` declares
    // no compensation and is unaffected, while a sync owes its `stash pop` on
    // the success path more often than on the failure one.
    //
    // Placed here rather than inside the loop so the transcript reads in the
    // order things happened: every forward step, including the ones that never
    // ran, and then the cleanup.
    let before_cleanup = runs.len();
    compensate(repo, &steps[..completed], per_step_timeout, extra_env, &mut log, &mut runs).await;

    // A cleanup that failed is a job that failed, even when every forward step
    // worked. Found by running a sync rather than by reading it: a `stash pop`
    // that conflicts leaves markers in a tracked file, and the job reported
    // `ok` because only the forward pass was consulted. "Succeeded" is not
    // something to say about a repository the user has to go and repair.
    if failure.is_none() {
        failure = runs[before_cleanup..].iter().find_map(|r| match r.state {
            StepState::Failed { code } => Some(JobState::Failed { code }),
            _ => None,
        });
    }

    let state = failure.unwrap_or(JobState::Ok);
    // Only for a job that has something to undo. A `Fetch` cannot move HEAD, and
    // spending a `rev-parse` per repository per fetch cycle to learn that again
    // is exactly the kind of cost automatic fetching cannot carry.
    let head_after = if action.is_mutating() && state == JobState::Ok {
        head_of(repo, cancel, extra_env).await
    } else {
        None
    };
    JobOutcome { state, steps: runs, log, head_before, head_after }
}

async fn run_step(
    repo: &Path,
    step: &Step,
    timeout: Duration,
    cancel: &CancellationToken,
    extra_env: &[(OsString, OsString)],
    log: &mut Vec<LogLine>,
) -> (StepState, std::ops::Range<usize>) {
    let start = log.len();
    let cmd = GitCommand::new(repo).args(&step.argv).cancel_with(cancel.clone()).envs(extra_env);
    match cmd.run(Instant::now() + timeout).await {
        Ok(out) => {
            log.extend(out.log);
            let state = match out.stop {
                Stop::Cancelled => StepState::Cancelled,
                Stop::TimedOut => StepState::Failed { code: -1 },
                Stop::Exited => match out.code {
                    Some(0) => StepState::Ok,
                    Some(code) => StepState::Failed { code },
                    None => StepState::Failed { code: -1 },
                },
            };
            (state, start..log.len())
        }
        Err(e) => {
            // git could not be started at all. Recorded in the transcript, or
            // the job would fail with nothing to read.
            log.push(LogLine::notice(e.to_string()));
            (StepState::Failed { code: -1 }, start..log.len())
        }
    }
}

/// Run the compensating commands of `completed`, newest first.
///
/// Not cancellable, deliberately: this *is* the cleanup. A cancellation that
/// aborted the compensation would leave exactly the half-finished state the
/// compensation exists to prevent.
///
/// **Stops at the first failure**, marking the rest `NotRun`. Each compensation
/// assumes the ones already run have put the repository back; a `stash pop`
/// after a failed `checkout` would apply the user's work to the branch they
/// were being moved *off*, which is worse than leaving it on the stack where
/// the transcript says it is.
async fn compensate(
    repo: &Path,
    completed: &[Step],
    timeout: Duration,
    extra_env: &[(OsString, OsString)],
    log: &mut Vec<LogLine>,
    runs: &mut Vec<StepRun>,
) {
    let mut stopped = false;
    for step in completed.iter().rev() {
        let Some(argv) = &step.compensate else { continue };
        if stopped {
            runs.push(StepRun {
                step: step.clone(),
                pass: Pass::Cleanup,
                state: StepState::NotRun,
                log: log.len()..log.len(),
            });
            continue;
        }
        let start = log.len();
        log.push(LogLine::notice(format!("cleanup: git {}", argv.join(" "))));
        let cmd = GitCommand::new(repo).args(argv).envs(extra_env);
        let state = match cmd.run(Instant::now() + timeout).await {
            Ok(out) => {
                let ok = out.success();
                let code = out.code.unwrap_or(-1);
                log.extend(out.log);
                if ok {
                    StepState::Ok
                } else {
                    StepState::Failed { code }
                }
            }
            Err(e) => {
                log.push(LogLine::notice(e.to_string()));
                StepState::Failed { code: -1 }
            }
        };
        stopped = !matches!(state, StepState::Ok);
        runs.push(StepRun {
            step: step.clone(),
            pass: Pass::Cleanup,
            state,
            log: start..log.len(),
        });
    }
}

/// `HEAD` as an oid, or `None` for an unborn branch.
///
/// `--verify` so an unborn `HEAD` exits non-zero instead of printing the string
/// `HEAD`, which would then be stored as a fake oid and offered as an undo
/// target.
async fn head_of(
    repo: &Path,
    cancel: &CancellationToken,
    extra_env: &[(OsString, OsString)],
) -> Option<Oid> {
    let cmd = GitCommand::new(repo)
        .args(["rev-parse", "--verify", "HEAD"])
        .cancel_with(cancel.clone())
        .envs(extra_env);
    // Short deadline: this is one ref lookup with no network and no locks. If it
    // cannot answer in five seconds the repository has bigger problems, and the
    // job's own deadline should not be spent here.
    let out = cmd.capture(Instant::now() + Duration::from_secs(5)).await.ok()?;
    if !out.success() {
        return None;
    }
    Oid::parse(String::from_utf8_lossy(&out.stdout).trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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

    fn env() -> Vec<(OsString, OsString)> {
        vec![
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
        ]
    }

    /// A repository with work stashed and `HEAD` on `main`, standing in for the
    /// state a sync's forward pass leaves behind.
    fn stashed_repo(dir: &Path) -> std::path::PathBuf {
        let repo = dir.join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(dir, &["init", "-b", "main", "r"]);
        std::fs::write(repo.join("a.txt"), "committed\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "c1"]);
        std::fs::write(repo.join("a.txt"), "work in progress\n").unwrap();
        git(&repo, &["stash", "push"]);
        repo
    }

    fn steps(back_to: &str) -> Vec<Step> {
        vec![
            Step::with_compensation(
                vec!["stash".into(), "push".into()],
                vec!["stash".into(), "pop".into()],
            ),
            Step::with_compensation(
                vec!["checkout".into(), "main".into()],
                vec!["checkout".into(), back_to.into()],
            ),
        ]
    }

    fn stash_count(repo: &Path) -> usize {
        let out = Command::new("git")
            .args(["stash", "list"])
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }

    #[tokio::test]
    async fn a_failed_cleanup_stops_the_ones_behind_it() {
        // The rule that keeps a sync from doing harm when it cannot finish. The
        // compensations run in reverse — switch back, *then* pop — and each
        // assumes the one before it worked. If the switch back fails the
        // repository is on the wrong branch, and popping there would apply the
        // user's work to a branch they were being moved off. Leaving it on the
        // stack is recoverable; applying it to the wrong branch is not.
        let tmp = tempfile::tempdir().unwrap();
        let repo = stashed_repo(tmp.path());
        assert_eq!(stash_count(&repo), 1);

        let mut log = Vec::new();
        let mut runs = Vec::new();
        compensate(
            &repo,
            &steps("no-such-branch"),
            Duration::from_secs(10),
            &env(),
            &mut log,
            &mut runs,
        )
        .await;

        assert_eq!(runs.len(), 2);
        assert!(matches!(runs[0].state, StepState::Failed { .. }), "{:?}", runs[0].state);
        assert_eq!(runs[1].state, StepState::NotRun, "the pop ran after the switch failed");
        assert_eq!(stash_count(&repo), 1, "the work was applied to the wrong branch");
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "committed\n");
    }

    #[tokio::test]
    async fn a_clean_cleanup_runs_all_of_them_newest_first() {
        // The positive control: without it the test above would pass on a
        // `compensate` that never ran anything at all.
        let tmp = tempfile::tempdir().unwrap();
        let repo = stashed_repo(tmp.path());

        let mut log = Vec::new();
        let mut runs = Vec::new();
        compensate(&repo, &steps("main"), Duration::from_secs(10), &env(), &mut log, &mut runs)
            .await;

        assert!(runs.iter().all(|r| r.state == StepState::Ok), "{runs:#?}");
        assert_eq!(stash_count(&repo), 0);
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "work in progress\n");
    }
}
