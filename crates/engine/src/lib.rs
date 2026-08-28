//! The action engine: policy, planning, scheduling, job lifecycle.
//!
//! No knowledge of Tauri and no knowledge of a terminal. The CLI and the GUI
//! are both thin surfaces over this.
//!
//! `policy` is the pure half — no I/O, no clock of its own, every input an
//! argument. That is what makes the safety rules exhaustively testable, and it
//! is where the correctness of a bulk tool actually lives.
//!
//! `sched` and `probe_traffic` are the same bargain applied to the actor's own
//! bookkeeping: both hold state, neither holds a clock or a runtime, and both
//! are tested by stating a situation rather than by producing one.

pub mod engine;
pub mod plan;
pub mod policy;
pub mod probe_traffic;
pub mod runner;
pub mod sched;
pub mod selection;

pub use engine::{CacheMode, Cmd, Config, Engine, EngineHandle, Event, Gone, ScanId, ScanOutcome};
pub use plan::{
    plan, undo, ActionVariant, ConfirmGuard, Plan, PlanRow, PlanVariant, PlanView, SkipGroup,
};
pub use policy::{
    after_attempt, due, evaluate, jitter, manual_attempt, Attempt, Eligibility, FetchPolicy, Policy,
};
pub use probe_traffic::{ProbeTraffic, Why};
pub use sched::{Launch, Limits, Scheduler, Ticket};
pub use selection::Selection;
