//! The action engine: policy, planning, scheduling, job lifecycle.

pub mod engine;
pub mod plan;
pub mod policy;
pub mod probe_traffic;
pub mod runner;
pub mod sched;
pub mod selection;

pub use engine::{CacheMode, Cmd, Config, Engine, EngineHandle, Event, Gone, ScanId, ScanOutcome};
pub use plan::{
    plan, resolve, undo, ActionVariant, ConfirmGuard, Plan, PlanRow, PlanTemplate, PlanVariant,
    PlanView, RefAnswers, SkipGroup,
};
pub use policy::{
    after_attempt, due, evaluate, jitter, manual_attempt, sync_default_resolved, Attempt,
    Eligibility, FetchPolicy, Policy,
};
pub use probe_traffic::{ProbeTraffic, Why};
pub use sched::{Launch, Limits, Scheduler, Ticket};
pub use selection::Selection;
