use crate::{Action, LogLine, Oid, RepoId, SkipReason, StepRun};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Identifier for one job. Allocated by the engine monotonically, so it also
/// orders jobs by the moment they were created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", ts(type = "number"))]
pub struct JobId(pub u64);

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Identifier for one batch — one user gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", ts(type = "number"))]
pub struct BatchId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "j{}", self.0)
    }
}

impl std::fmt::Display for BatchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "b{}", self.0)
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Who asked for this job.
///
/// The background fetch scheduler is the only producer of `Background`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobOrigin {
    User,
    Background,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum JobState {
    Queued,
    Running,
    Ok,
    Failed { code: i32 },
    Cancelled,
    Skipped { why: SkipReason },
}

impl JobState {
    /// Has this job reached a state it will not leave?
    pub fn is_terminal(&self) -> bool {
        !matches!(self, JobState::Queued | JobState::Running)
    }

    /// Did anything actually run against the repository? `Skipped` did not.
    pub fn ran(&self) -> bool {
        matches!(self, JobState::Ok | JobState::Failed { .. } | JobState::Cancelled)
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobState::Queued => f.write_str("queued"),
            JobState::Running => f.write_str("running"),
            JobState::Ok => f.write_str("ok"),
            JobState::Failed { code } => write!(f, "failed ({code})"),
            JobState::Cancelled => f.write_str("cancelled"),
            JobState::Skipped { why } => write!(f, "skipped: {why}"),
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// One action against one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    /// `None` for a background job: the fetch scheduler's jobs belong to no
    /// user batch.
    pub batch: Option<BatchId>,
    pub origin: JobOrigin,
    pub repo: RepoId,
    /// Resolved for **this** repository, not the batch's template.
    pub action: Action,
    pub state: JobState,
    /// Populated from [`Action::steps`] when the job is created.
    pub steps: Vec<StepRun>,
    /// `HEAD` immediately before a mutating job. `None` for a non-mutating
    /// action or an unborn branch.
    pub head_before: Option<Oid>,
    /// Where the job left `HEAD`.
    pub head_after: Option<Oid>,
    /// The branch checked out when the job was planned. `None` for a detached HEAD.
    pub branch_before: Option<String>,
    /// The whole transcript, interleaved and ordered. `StepRun::log` indexes
    /// into it.
    pub log: Vec<LogLine>,
    #[serde(with = "crate::serde_time::option")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub started_at: Option<SystemTime>,
    #[serde(with = "crate::serde_time::option")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub finished_at: Option<SystemTime>,
}

impl Job {
    /// A queued job with its steps laid out and nothing run yet.
    pub fn queued(
        id: JobId,
        batch: Option<BatchId>,
        origin: JobOrigin,
        repo: RepoId,
        action: Action,
    ) -> Self {
        let steps =
            action.steps().into_iter().map(|s| StepRun::pending(s, crate::Pass::Forward)).collect();
        Self {
            id,
            batch,
            origin,
            repo,
            action,
            state: JobState::Queued,
            steps,
            head_before: None,
            head_after: None,
            branch_before: None,
            log: Vec::new(),
            started_at: None,
            finished_at: None,
        }
    }

    /// A job that never ran, and why.
    pub fn skipped(
        id: JobId,
        batch: Option<BatchId>,
        origin: JobOrigin,
        repo: RepoId,
        action: Action,
        why: SkipReason,
    ) -> Self {
        let now = SystemTime::now();
        Self {
            state: JobState::Skipped { why },
            started_at: Some(now),
            finished_at: Some(now),
            ..Self::queued(id, batch, origin, repo, action)
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        let (start, end) = (self.started_at?, self.finished_at?);
        end.duration_since(start).ok()
    }

    /// The transcript lines belonging to one step. Clamped rather than panicking
    /// on an out-of-range index.
    pub fn step_log(&self, step: &StepRun) -> &[LogLine] {
        let end = step.log.end.min(self.log.len());
        let start = step.log.start.min(end);
        &self.log[start..end]
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// One user gesture: an action, a selection, and the jobs it produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    pub id: BatchId,
    /// The **template** the plan was built from. Each job carries its own
    /// resolved action, which may differ per repository.
    pub action: Action,
    pub origin: JobOrigin,
    pub jobs: Vec<JobId>,
    /// The batch this one undoes, if it is an undo.
    #[serde(default)]
    pub undoes: Option<BatchId>,
    #[serde(with = "crate::serde_time")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub started_at: SystemTime,
    #[serde(with = "crate::serde_time::option")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub finished_at: Option<SystemTime>,
}

impl Batch {
    pub fn duration(&self) -> Option<Duration> {
        self.finished_at?.duration_since(self.started_at).ok()
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// How a batch turned out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchSummary {
    pub ok: usize,
    pub failed: usize,
    pub skipped: usize,
    pub cancelled: usize,
    /// Still queued or running. Non-zero only for a summary taken mid-flight.
    pub pending: usize,
    #[serde(with = "crate::serde_time::duration")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub duration: Duration,
}

impl BatchSummary {
    pub fn of<'a>(jobs: impl IntoIterator<Item = &'a Job>, duration: Duration) -> Self {
        let mut s = Self { duration, ..Default::default() };
        for job in jobs {
            match &job.state {
                JobState::Ok => s.ok += 1,
                JobState::Failed { .. } => s.failed += 1,
                JobState::Skipped { .. } => s.skipped += 1,
                JobState::Cancelled => s.cancelled += 1,
                JobState::Queued | JobState::Running => s.pending += 1,
            }
        }
        s
    }

    pub fn total(&self) -> usize {
        self.ok + self.failed + self.skipped + self.cancelled + self.pending
    }

    /// Nothing failed, was cancelled, or is still pending.
    pub fn is_clean_sweep(&self) -> bool {
        self.failed == 0 && self.cancelled == 0 && self.pending == 0
    }

    /// The tally as one sentence: `31 ok, 3 failed, 13 skipped in 4.2s`.
    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        for (n, label) in [
            (self.ok, "ok"),
            (self.failed, "failed"),
            (self.skipped, "skipped"),
            (self.cancelled, "cancelled"),
            (self.pending, "pending"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {label}"));
            }
        }
        if parts.is_empty() {
            parts.push("nothing to do".into());
        }
        format!("{} in {:.1}s", parts.join(", "), self.duration.as_secs_f64())
    }

    /// Exit code for the CLI: `0` all-ok-or-skipped, `1` any failure, `2`
    /// cancelled.
    pub fn exit_code(&self) -> u8 {
        if self.failed > 0 {
            1
        } else if self.cancelled > 0 {
            2
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Pass, PullMode, StepState};

    fn repo() -> RepoId {
        RepoId::from_canonical("/r")
    }

    fn job(id: u64, state: JobState) -> Job {
        Job {
            state,
            ..Job::queued(
                JobId(id),
                Some(BatchId(1)),
                JobOrigin::User,
                repo(),
                Action::Fetch { prune: true, tags: false },
            )
        }
    }

    #[test]
    fn a_queued_job_already_knows_the_commands_it_will_run() {
        let j = Job::queued(
            JobId(1),
            Some(BatchId(1)),
            JobOrigin::User,
            repo(),
            Action::Commit { message: "m".into(), stage_all: true, no_verify: false },
        );
        assert_eq!(j.steps.len(), 2);
        assert_eq!(j.steps[0].step.argv, ["add", "-A"]);
        assert_eq!(j.steps[1].step.argv, ["commit", "-m", "m"]);
        assert!(j.steps.iter().all(|s| s.state == StepState::Pending));
        assert!(j.steps.iter().all(|s| s.pass == Pass::Forward));
        assert_eq!(j.state, JobState::Queued);
        assert!(j.head_before.is_none(), "recorded at execution, not at planning");
    }

    #[test]
    fn a_skipped_job_is_terminal_and_carries_its_reason() {
        let j = Job::skipped(
            JobId(1),
            Some(BatchId(1)),
            JobOrigin::User,
            repo(),
            Action::Pull { mode: PullMode::FfOnly },
            SkipReason::NoUpstream,
        );
        assert!(j.state.is_terminal());
        assert!(!j.state.ran(), "a skip did not touch the repository");
        assert_eq!(j.state.to_string(), "skipped: no upstream configured");
        assert!(j.duration().is_some());
    }

    #[test]
    fn terminal_and_ran_are_different_questions() {
        assert!(!JobState::Queued.is_terminal());
        assert!(!JobState::Running.is_terminal());
        for s in [
            JobState::Ok,
            JobState::Failed { code: 1 },
            JobState::Cancelled,
            JobState::Skipped { why: SkipReason::UpToDate },
        ] {
            assert!(s.is_terminal(), "{s:?}");
        }
        assert!(!JobState::Skipped { why: SkipReason::UpToDate }.ran());
        assert!(JobState::Cancelled.ran());
    }

    #[test]
    fn a_summary_tallies_rather_than_verdicts() {
        let jobs = vec![
            job(1, JobState::Ok),
            job(2, JobState::Ok),
            job(3, JobState::Failed { code: 128 }),
            job(4, JobState::Skipped { why: SkipReason::UpToDate }),
            job(5, JobState::Skipped { why: SkipReason::NoUpstream }),
            job(6, JobState::Cancelled),
            job(7, JobState::Running),
        ];
        let s = BatchSummary::of(&jobs, Duration::from_secs(3));
        assert_eq!((s.ok, s.failed, s.skipped, s.cancelled, s.pending), (2, 1, 2, 1, 1));
        assert_eq!(s.total(), 7);
        assert!(!s.is_clean_sweep());
    }

    #[test]
    fn a_run_with_only_skips_is_a_clean_sweep() {
        let jobs =
            vec![job(1, JobState::Ok), job(2, JobState::Skipped { why: SkipReason::UpToDate })];
        let s = BatchSummary::of(&jobs, Duration::ZERO);
        assert!(s.is_clean_sweep());
        assert_eq!(s.exit_code(), 0);
    }

    #[test]
    fn exit_codes_rank_failure_above_cancellation() {
        let failed = BatchSummary { failed: 1, cancelled: 1, ..Default::default() };
        assert_eq!(failed.exit_code(), 1, "a failure is more informative than a cancellation");
        let cancelled = BatchSummary { cancelled: 1, ..Default::default() };
        assert_eq!(cancelled.exit_code(), 2);
        assert_eq!(BatchSummary::default().exit_code(), 0);
    }

    #[test]
    fn an_empty_batch_summarises_to_nothing_rather_than_panicking() {
        let s = BatchSummary::of(std::iter::empty(), Duration::ZERO);
        assert_eq!(s.total(), 0);
        assert!(s.is_clean_sweep());
        assert_eq!(s.exit_code(), 0);
    }

    #[test]
    fn a_step_log_range_past_the_end_is_clamped_not_a_panic() {
        let mut j = job(1, JobState::Ok);
        j.log = vec![LogLine::notice("only line")];
        let mut step = StepRun::pending(crate::Step::simple(vec!["fetch".into()]), Pass::Forward);
        step.log = 5..900;
        assert!(j.step_log(&step).is_empty());
        step.log = 0..900;
        assert_eq!(j.step_log(&step).len(), 1);
    }

    #[test]
    fn ids_display_compactly_for_a_cli() {
        assert_eq!(JobId(37).to_string(), "j37");
        assert_eq!(BatchId(2).to_string(), "b2");
    }

    #[test]
    fn a_job_survives_json_with_millisecond_timestamps() {
        let mut j = job(1, JobState::Failed { code: 128 });
        j.log = vec![LogLine::new(crate::Stream::Stderr, "fatal: nope")];
        j.steps[0].state = StepState::Failed { code: 128 };
        j.steps[0].log = 0..1;
        j.started_at = Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123));
        j.finished_at = Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_456));

        let once = serde_json::to_string(&j).unwrap();
        let back: Job = serde_json::from_str(&once).unwrap();

        assert_eq!(back.id, j.id);
        assert_eq!(back.state, j.state);
        assert_eq!(back.action, j.action);
        assert_eq!(back.steps, j.steps);
        assert_eq!(back.log[0].text, j.log[0].text);
        assert_eq!(back.started_at, j.started_at, "whole millis are exact");
        assert_eq!(back.finished_at, j.finished_at);

        assert_eq!(serde_json::to_string(&back).unwrap(), once);
    }

    #[test]
    fn durations_are_plain_millis_on_the_wire() {
        let s = BatchSummary { ok: 1, duration: Duration::from_millis(1500), ..Default::default() };
        let json = serde_json::to_value(s).unwrap();
        assert_eq!(json["duration"], 1500);
    }
    #[test]
    fn the_summary_reads_as_an_outcome_not_a_verdict() {
        let s = BatchSummary {
            ok: 31,
            failed: 3,
            skipped: 13,
            cancelled: 0,
            pending: 0,
            duration: Duration::from_millis(4230),
        };
        assert_eq!(s.render(), "31 ok, 3 failed, 13 skipped in 4.2s");
        assert!(!s.is_clean_sweep());
    }

    #[test]
    fn zero_counts_are_left_out_but_a_nonzero_one_never_is() {
        let clean = BatchSummary { ok: 2, duration: Duration::from_secs(1), ..Default::default() };
        assert_eq!(clean.render(), "2 ok in 1.0s");

        let midway = BatchSummary {
            ok: 1,
            pending: 4,
            duration: Duration::from_secs(2),
            ..Default::default()
        };
        assert_eq!(midway.render(), "1 ok, 4 pending in 2.0s");
    }

    #[test]
    fn a_batch_that_did_nothing_says_so_rather_than_rendering_an_empty_line() {
        let none = BatchSummary::default();
        assert_eq!(none.render(), "nothing to do in 0.0s");
    }
}
