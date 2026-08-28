//! Domain types for git-scylla. Pure: no I/O and no subprocesses.
//!
//! Everything here is a fact about a repository or a job, or a derivation from
//! those facts. Needing the filesystem puts a thing in `discovery` or `probe`;
//! needing to schedule or wait puts it in `engine`.
//!
//! A few constructors stamp a value with the wall clock. Nothing here *decides*
//! from a clock — preconditions and fetch policy take `now` as an argument,
//! which is what keeps them testable.

mod action;
mod badge;
mod detail;
pub mod duration;
mod explain;
mod fetch;
mod filter;
mod id;
mod job;
mod log;
pub mod serde_time;

pub use serde_time::duration as serde_duration;

mod skip;
mod snapshot;
pub mod template;
pub mod version;

pub use action::{
    undoability, Action, Pass, PullMode, ResetMode, Step, StepRun, StepState, SyncPlan, Undoable,
};
pub use badge::Badge;
pub use detail::{RepoDetail, StashEntry, Tag};
pub use explain::{explain, Explanation, FailureKind};
pub use fetch::{FetchHealth, FetchSchedule, FetchStatus};
pub use filter::{Filter, FilterError, Term};
pub use id::{Oid, OidError, RepoId};
pub use job::{Batch, BatchId, BatchSummary, Job, JobId, JobOrigin, JobState};
pub use log::{LogLine, Stream};
pub use skip::SkipReason;
pub use snapshot::{
    AheadBehind, Head, InProgress, ProbeOutcome, Remote, RepoKind, RepoSnapshot, Upstream, WorkTree,
};
