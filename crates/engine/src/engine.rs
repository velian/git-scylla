//! The actor.
//!
//! A single `tokio::task` owns every mutable map. A walker, a probe pool, a job
//! scheduler, a watcher and a fetch tick all write to the same state, and
//! `Arc<Mutex<HashMap>>` under that load makes cancellation and ordering hard to
//! reason about. One task is a serialization point for free.
//!
//! Nothing here blocks. The walk runs on a blocking thread, probes and jobs run
//! as spawned tasks, and all of them report back through one internal channel
//! that the actor drains in the same loop as its commands.

use crate::plan::{plan, Plan};
use crate::policy::{after_attempt, due, manual_attempt, Attempt, FetchPolicy, Policy};
use crate::probe_traffic::{ProbeTraffic, Why};
use crate::runner::run_job;
use crate::sched::{Limits, Permits, Scheduler, Ticket};
use crate::Selection;
use git_scylla_core::{
    Action, Batch, BatchId, BatchSummary, FetchHealth, FetchSchedule, Job, JobId, JobOrigin,
    JobState, LogLine, RepoId, RepoSnapshot, SkipReason, Stream,
};
use git_scylla_discovery::{DiscoveryError, RepoFound, WalkOptions, Walker};
use git_scylla_probe::{
    GitCliProbe, Probe, ProbeRequest, RefAnswer, RefError, RefQuery, RefRequest,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

/// How long snapshot changes are gathered before the cache is rewritten.
const CACHE_DEBOUNCE: Duration = Duration::from_secs(2);

/// The least time between two probes of one repository, for requests that came
/// from watching rather than from knowing.
///
/// Handed to [`ProbeTraffic`] rather than read here: the rule that uses it
/// lives there, and a constant read from two modules is one that can be changed
/// in one of them.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Sent as a plain number, not ts-rs's default `bigint` for `u64`: Tauri's IPC
/// is JSON, `serde_json` writes a number, and JavaScript reads one. A generated
/// type that says `bigint` would be a lie of exactly the kind generating them is
/// meant to prevent. Session counters never approach 2^53.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", ts(type = "number"))]
pub struct ScanId(pub u64);

impl std::fmt::Display for ScanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// Commands into the actor.
pub enum Cmd {
    StartScan {
        roots: Vec<PathBuf>,
        nested: bool,
        reply: oneshot::Sender<ScanId>,
    },
    CancelScan {
        id: ScanId,
    },
    Plan {
        action: Action,
        sel: Selection,
        reply: oneshot::Sender<Plan>,
    },
    StartBatch {
        plan: Plan,
        origin: JobOrigin,
        reply: oneshot::Sender<BatchId>,
    },
    CancelBatch {
        id: BatchId,
    },
    RefreshRepo {
        id: RepoId,
    },
    /// A watcher's report. The engine disposes: a repository with a job in
    /// flight is not re-probed underneath it, whoever asked.
    Invalidate {
        what: git_scylla_watch::Invalidation,
    },
    /// Whether a watcher is covering the roots.
    ///
    /// The engine does not own the watcher — the shell does, and the CLI has
    /// none — so it has to be told. It changes what a snapshot's *age* means:
    /// with coverage, an old snapshot is one nothing has changed, and without
    /// it, one nobody has checked.
    SetWatched {
        covered: bool,
    },
    Snapshot {
        reply: oneshot::Sender<Vec<RepoSnapshot>>,
    },
    /// The ids matching a selection.
    ///
    /// Distinct from `Snapshot` because the filter box asks this on every
    /// debounced keystroke and wants only the ids: answering it by shipping
    /// every snapshot out of the actor and filtering on the far side cloned the
    /// whole working set to throw almost all of it away.
    Select {
        sel: Selection,
        reply: oneshot::Sender<Vec<RepoId>>,
    },
    JobLog {
        id: JobId,
        reply: oneshot::Sender<Vec<LogLine>>,
    },
    Jobs {
        batch: BatchId,
        reply: oneshot::Sender<Vec<Job>>,
    },
    /// What undoing a finished batch would do.
    ///
    /// A re-probe of every affected repository is requested first, because the
    /// guards are only as good as the snapshot they read. It is a *request*,
    /// not a wait: the staleness guard is what makes the freshness enforceable,
    /// so a plan computed before the probes land refuses by name rather than
    /// acting on old facts.
    PlanUndo {
        batch: BatchId,
        reply: oneshot::Sender<Plan>,
    },
    /// Run an undo. Marks the new batch, so it cannot itself be undone.
    StartUndo {
        batch: BatchId,
        plan: Plan,
        reply: oneshot::Sender<BatchId>,
    },
    /// The background jobs still retained, bounded by
    /// [`Config::background_history`].
    ///
    /// A drawer filtering on origin reads these from the event stream; this is
    /// for a surface that arrived late, and for the CLI's daemon.
    BackgroundJobs {
        reply: oneshot::Sender<Vec<Job>>,
    },
    /// Stop accepting commands and wind down.
    ///
    /// Explicit rather than "when the last handle drops": handles are cloned
    /// freely — the CLI holds one while a caller holds another — so dropping one
    /// says nothing about whether anyone still wants the engine.
    Shutdown,
}

/// Everything the actor publishes.
///
/// Serializable because the desktop bridge forwards these straight to the
/// webview. The TypeScript is generated from this enum; a hand-written mirror
/// would drift.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Event {
    ReposUpserted(Vec<RepoSnapshot>),
    /// Repositories that are no longer on disk.
    ///
    /// Only a watcher can observe one going away. A row for a directory the
    /// user deleted is worse than no row: every action offered on it will fail.
    ReposRemoved(Vec<RepoId>),
    /// Progress of one scan.
    ///
    /// Carries the `ScanId`: without it a consumer cannot tell two concurrent
    /// scans apart, and there are two the moment the user adds a root while one
    /// is running, or presses Refresh.
    ScanProgress {
        scan: ScanId,
        found: usize,
        probed: usize,
    },
    /// A scan finished, with whatever it could not read.
    ///
    /// `errors` are the places the walk could not read — a root that does not
    /// exist, or a directory macOS refused. Structured rather than stringified,
    /// because the UI has to distinguish "that path is wrong" from "I was not
    /// allowed to look": the second is almost always TCC, and saying so is the
    /// highest-value error message in the application.
    ScanDone {
        scan: ScanId,
        errors: Vec<DiscoveryError>,
    },
    /// One job moved. Emitted for **every** state a job reaches, `Queued`
    /// included, so the stream alone is enough to know a job exists.
    ///
    /// `batch` and `origin` are carried rather than left to be looked up. A
    /// consumer that has to ask the engine which batch a job belongs to cannot
    /// attribute the events that arrive before its answer does, and the ones a
    /// user batch emits arrive before `start_batch` has even returned its id.
    /// `origin` is here for the same reason: the drawer filters on it, and it
    /// filters jobs nobody asked for, so it cannot have learned about them from
    /// a call it made.
    JobStateChanged {
        id: JobId,
        batch: Option<BatchId>,
        origin: JobOrigin,
        repo: RepoId,
        state: JobState,
    },
    JobLogAppended {
        id: JobId,
        lines: Vec<LogLine>,
    },
    /// Every job in the batch has reached a terminal state.
    ///
    /// **This precedes the re-probes.** A completed job schedules exactly one
    /// re-probe of its repository, and those land afterwards as
    /// `ReposUpserted`. The two are deliberately not merged: the drawer should
    /// show its summary the moment the work is done rather than waiting on a
    /// `git status` per repository, and a caller that needs the settled state
    /// should watch `ReposUpserted` or ask again.
    BatchDone {
        id: BatchId,
        summary: BatchSummary,
    },
    /// A subscriber fell behind and events were dropped.
    ///
    /// Not produced by the actor — the broadcast channel reports it, and
    /// whoever forwards events turns it into this. It exists because dropping
    /// events silently leaves a UI showing stale rows forever with no way to
    /// know: the honest response is to say so and let the consumer re-read the
    /// snapshot.
    Lagged,
}

/// Results flowing back from spawned work.
enum Internal {
    /// `scan` is `None` for a targeted discovery pass: a repository appearing
    /// under a watched root is an upsert, not a scan, and reporting progress for
    /// it would put a bar on screen nobody asked for.
    Found {
        scan: Option<ScanId>,
        found: RepoFound,
    },
    WalkFinished {
        scan: Option<ScanId>,
        errors: Vec<DiscoveryError>,
    },
    Probed(Box<RepoSnapshot>),
    JobFinished {
        id: JobId,
        outcome: Box<crate::runner::JobOutcome>,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub limits: Limits,
    pub policy: Policy,
    pub nested: bool,
    /// Maximum directory depth to descend, or unlimited.
    pub max_depth: Option<usize>,
    /// Deadline for one probe. Separate from a job's: a probe is one
    /// `git status` and must never be the thing that stalls the grid.
    pub probe_timeout: Duration,
    /// Applied to every child. Empty in production; tests use it to pin
    /// `GIT_CONFIG_GLOBAL` so a developer's `~/.gitconfig` cannot change a
    /// result.
    pub extra_env: Vec<(OsString, OsString)>,
    /// When to fetch, without being asked.
    pub fetch: FetchPolicy,
    /// How often the fetch scheduler looks at the working set.
    ///
    /// Not the interval between a repository's fetches — that is
    /// `FetchPolicy::interval`. This is only how finely the schedule is
    /// sampled, so it wants to be small against the interval and large against
    /// nothing in particular.
    pub fetch_tick: Duration,
    /// Completed background jobs kept before the oldest is evicted.
    ///
    /// A process that runs forever needs a ceiling on what it keeps, and a
    /// fifteen-minute cycle over a hundred repositories is four hundred
    /// transcripts an hour.
    pub background_history: usize,
    /// How this engine uses the startup cache.
    pub cache: CacheMode,
}

/// What an engine does with the startup cache.
///
/// Three states rather than a bool, because reading and writing are wanted
/// separately. `git-scylla status` has to *read* the daemon's recorded fetch
/// health — that is the whole point of answering "why does this say 3 behind"
/// from a terminal — but it must not *write*, or a one-shot command would
/// overwrite the application's cache with whatever roots happened to be on its
/// command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Neither read nor written. The mutating verbs, which scan for themselves
    /// and have no use for a previous run's view.
    #[default]
    Off,
    /// Served at launch, never written.
    Read,
    /// Served and maintained.
    ReadWrite,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            policy: Policy::default(),
            nested: false,
            max_depth: None,
            probe_timeout: Duration::from_secs(2),
            extra_env: Vec::new(),
            fetch: FetchPolicy::default(),
            fetch_tick: Duration::from_secs(30),
            background_history: 200,
            cache: CacheMode::Off,
        }
    }
}

/// A handle to a running engine.
#[derive(Clone)]
pub struct EngineHandle {
    cmd: mpsc::Sender<Cmd>,
    events: broadcast::Sender<Event>,
}

/// Sending failed because the actor is gone.
#[derive(Debug, thiserror::Error)]
#[error("the engine has stopped")]
pub struct Gone;

impl EngineHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub async fn send(&self, cmd: Cmd) -> Result<(), Gone> {
        self.cmd.send(cmd).await.map_err(|_| Gone)
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Cmd) -> Result<T, Gone> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx)).await?;
        rx.await.map_err(|_| Gone)
    }

    pub async fn start_scan(&self, roots: Vec<PathBuf>, nested: bool) -> Result<ScanId, Gone> {
        self.ask(|reply| Cmd::StartScan { roots, nested, reply }).await
    }

    pub async fn cancel_scan(&self, id: ScanId) -> Result<(), Gone> {
        self.send(Cmd::CancelScan { id }).await
    }

    pub async fn plan(&self, action: Action, sel: Selection) -> Result<Plan, Gone> {
        self.ask(|reply| Cmd::Plan { action, sel, reply }).await
    }

    pub async fn start_batch(&self, plan: Plan, origin: JobOrigin) -> Result<BatchId, Gone> {
        self.ask(|reply| Cmd::StartBatch { plan, origin, reply }).await
    }

    pub async fn cancel_batch(&self, id: BatchId) -> Result<(), Gone> {
        self.send(Cmd::CancelBatch { id }).await
    }

    pub async fn refresh_repo(&self, id: RepoId) -> Result<(), Gone> {
        self.send(Cmd::RefreshRepo { id }).await
    }

    /// Say whether a watcher is covering the roots. See [`Cmd::SetWatched`].
    pub async fn set_watched(&self, covered: bool) -> Result<(), Gone> {
        self.send(Cmd::SetWatched { covered }).await
    }

    /// Report what a watcher saw. See [`Cmd::Invalidate`].
    pub async fn invalidate(&self, what: git_scylla_watch::Invalidation) -> Result<(), Gone> {
        self.send(Cmd::Invalidate { what }).await
    }

    /// Every repository the engine holds, as the watcher wants them.
    pub async fn watched(&self) -> Result<Vec<git_scylla_watch::Watched>, Gone> {
        Ok(self
            .snapshot()
            .await?
            .into_iter()
            .map(|s| git_scylla_watch::Watched {
                id: s.id,
                path: s.path,
                bare: matches!(s.kind, git_scylla_core::RepoKind::Bare),
            })
            .collect())
    }

    pub async fn snapshot(&self) -> Result<Vec<RepoSnapshot>, Gone> {
        self.ask(|reply| Cmd::Snapshot { reply }).await
    }

    /// The ids of the repositories a selection matches.
    pub async fn select(&self, sel: Selection) -> Result<Vec<RepoId>, Gone> {
        self.ask(|reply| Cmd::Select { sel, reply }).await
    }

    pub async fn job_log(&self, id: JobId) -> Result<Vec<LogLine>, Gone> {
        self.ask(|reply| Cmd::JobLog { id, reply }).await
    }

    pub async fn jobs(&self, batch: BatchId) -> Result<Vec<Job>, Gone> {
        self.ask(|reply| Cmd::Jobs { batch, reply }).await
    }

    /// What undoing this batch would do. See [`Cmd::PlanUndo`].
    pub async fn plan_undo(&self, batch: BatchId) -> Result<Plan, Gone> {
        self.ask(|reply| Cmd::PlanUndo { batch, reply }).await
    }

    /// Run an undo of `batch`.
    pub async fn start_undo(&self, batch: BatchId, plan: Plan) -> Result<BatchId, Gone> {
        self.ask(|reply| Cmd::StartUndo { batch, plan, reply }).await
    }

    /// The background jobs still retained, oldest first.
    pub async fn background_jobs(&self) -> Result<Vec<Job>, Gone> {
        self.ask(|reply| Cmd::BackgroundJobs { reply }).await
    }

    /// Scan and wait for it to finish.
    ///
    /// Subscribes **before** sending, so a scan that completes immediately —
    /// an empty root, a cached tree — cannot finish between the two and leave
    /// the caller waiting for an event that already happened.
    pub async fn scan_to_completion(
        &self,
        roots: Vec<PathBuf>,
        nested: bool,
    ) -> Result<ScanOutcome, Gone> {
        let mut events = self.subscribe();
        let id = self.start_scan(roots, nested).await?;
        let errors = loop {
            match events.recv().await {
                Ok(Event::ScanDone { scan, errors }) if scan == id => break errors,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Err(Gone),
            }
        };
        Ok(ScanOutcome { snapshots: self.snapshot().await?, errors })
    }
}

/// What one scan produced.
#[derive(Debug, Clone)]
pub struct ScanOutcome {
    pub snapshots: Vec<RepoSnapshot>,
    /// Everything the walk could not read. An empty scan with a non-empty
    /// `errors` is a configuration or permissions problem, not an empty working
    /// set, and must never be presented as one.
    pub errors: Vec<DiscoveryError>,
}

/// A running engine. Dropping this stops the actor.
pub struct Engine {
    handle: EngineHandle,
    task: tokio::task::JoinHandle<()>,
}

impl Engine {
    pub fn start(config: Config) -> Self {
        Self::with_probe(config, Arc::new(GitCliProbe::new()))
    }

    /// For tests: the probe is the only I/O seam the engine has.
    pub fn with_probe(config: Config, probe: Arc<dyn Probe>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (event_tx, _) = broadcast::channel(4096);
        let handle = EngineHandle { cmd: cmd_tx, events: event_tx.clone() };
        let (actor, internal_rx) = Actor::new(config, probe, event_tx);
        let task = tokio::spawn(actor.run(cmd_rx, internal_rx));
        Self { handle, task }
    }

    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }

    /// Stop accepting commands and wait for in-flight work to finish.
    ///
    /// In-flight jobs are allowed to complete rather than being abandoned
    /// mid-`git`: a half-applied pull is worse than a slow exit. Use
    /// [`EngineHandle::cancel_batch`] first to stop them.
    pub async fn shutdown(self) {
        let Engine { handle, task } = self;
        let _ = handle.send(Cmd::Shutdown).await;
        drop(handle);
        let _ = task.await;
    }
}

struct ScanRun {
    stop: Arc<AtomicBool>,
    walk_done: bool,
    errors: Vec<DiscoveryError>,
    /// Repositories this scan accepted. Paths that vanished between discovery
    /// and canonicalization are **not** counted: counting one would leave
    /// `pending` permanently non-empty and the scan would never settle.
    ///
    /// A set for the same reason `pending` is. As a counter it double-counted a
    /// repository the walk reported twice, while `pending` deduplicated it — so
    /// the two disagreed and the progress readout claimed more repositories
    /// than the scan had found.
    accepted: HashSet<RepoId>,
    /// Accepted but not yet probed.
    ///
    /// A set of ids rather than a counter, because a counter cannot be
    /// attributed. Two concurrent scans incrementing one shared counter each see
    /// the other's work, and a job's re-probe inflates both — which is how a
    /// scan reports itself complete while its own rows are still missing.
    ///
    /// A re-probe triggered by something else still clears an entry here, and
    /// that is correct: the scan is waiting for the repository to have been
    /// read, not for its own subprocess in particular.
    pending: HashSet<RepoId>,
}

impl ScanRun {
    fn settled(&self) -> bool {
        self.walk_done && self.pending.is_empty()
    }

    fn probed(&self) -> usize {
        self.accepted.len() - self.pending.len()
    }
}

struct BatchRun {
    batch: Batch,
    cancel: CancellationToken,
    outstanding: usize,
}

/// How far a job got before it reached a terminal state.
///
/// Deliberately not the `reprobe: bool` this started as. Two questions hang off
/// it — does this job still hold its repository, and did anything on disk move
/// — and they are not the same question: a job cancelled out of the queue
/// answers no to both, while a job launched that never spawned `git` answers
/// yes to the first. A bool could only ever say one of them, and the one it did
/// not say released a repository that a *different* job was running in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    /// It ran `git`, however that ended.
    Ran,
    /// It was launched — and so took its repository — but never spawned `git`.
    NotStarted,
    /// It was cancelled while still queued, so it never took its repository.
    NeverLaunched,
}

struct Actor {
    config: Config,
    probe: Arc<dyn Probe>,
    events: broadcast::Sender<Event>,
    internal: mpsc::Sender<Internal>,
    probe_slots: Arc<Semaphore>,

    snapshots: HashMap<RepoId, RepoSnapshot>,
    found: HashMap<RepoId, RepoFound>,
    jobs: HashMap<JobId, Job>,
    batches: HashMap<BatchId, BatchRun>,
    scans: HashMap<ScanId, ScanRun>,

    sched: Scheduler,
    /// Permits are held here rather than inside the job task, so the actor
    /// releases capacity at a point it controls.
    held: HashMap<JobId, Permits>,
    /// Who is owed a probe, who has one in flight, and when each last started.
    ///
    /// Its own module because the rules are subtle, each of them was a bug
    /// first, and none of them needs anything the actor owns — see
    /// [`crate::probe_traffic`].
    traffic: ProbeTraffic,

    /// Is a watcher covering the roots?
    watched: bool,

    /// Has any scan settled yet?
    ///
    /// The gate on the whole fetch scheduler. **The initial scan never touches
    /// the network**, and a launch that fetches eighty repositories at once is
    /// exactly what the pacing exists to avoid.
    scan_settled: bool,
    /// Completed background jobs, oldest first, for eviction.
    background_done: VecDeque<JobId>,

    /// Repositories seeded from the startup cache and not yet confirmed by a
    /// scan. Emptied when the first full scan settles, at which point anything
    /// still in it was not found and is dropped.
    from_cache: HashSet<RepoId>,
    /// Whether the cached rows have been published to subscribers yet.
    cache_served: bool,
    /// Snapshots have moved since the cache was last written.
    cache_dirty: bool,
    /// The roots the cache is for. Known only once a scan is asked for.
    cache_roots: Vec<PathBuf>,

    next_job: u64,
    next_batch: u64,
    next_scan: u64,
}

impl Actor {
    /// Returns the actor and the receiving half of its internal channel.
    ///
    /// Both halves are built here because the sender has to live in the struct,
    /// and `run` consumes the receiver — so it is handed back rather than
    /// smuggled through a field nobody else should touch.
    fn new(
        config: Config,
        probe: Arc<dyn Probe>,
        events: broadcast::Sender<Event>,
    ) -> (Self, mpsc::Receiver<Internal>) {
        let (internal, internal_rx) = mpsc::channel(4096);
        let sched = Scheduler::new(config.limits.clone());
        let probe_slots = Arc::new(Semaphore::new(config.limits.local.max(1)));
        let actor = Self {
            config,
            probe,
            events,
            internal,
            probe_slots,
            snapshots: HashMap::new(),
            found: HashMap::new(),
            jobs: HashMap::new(),
            batches: HashMap::new(),
            scans: HashMap::new(),
            sched,
            held: HashMap::new(),
            traffic: ProbeTraffic::new(PROBE_INTERVAL),
            watched: false,
            scan_settled: false,
            background_done: VecDeque::new(),
            from_cache: HashSet::new(),
            cache_served: false,
            cache_dirty: false,
            cache_roots: Vec::new(),
            next_job: 1,
            next_batch: 1,
            next_scan: 1,
        };
        (actor, internal_rx)
    }

    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<Cmd>,
        mut internal_rx: mpsc::Receiver<Internal>,
    ) {
        let mut accepting = true;
        // The cache's clock. Debounced rather than written per change: a batch
        // of forty pulls produces forty snapshot updates in a second, and
        // rewriting the file for each is thrash for something nobody reads
        // until the next launch.
        let mut cache_tick = tokio::time::interval(CACHE_DEBOUNCE);
        cache_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The fetch scheduler's clock, and the only clock it has: `due` itself
        // is pure and stateless.
        //
        // Never coarser than the interval it is sampling. A repository due
        // every two seconds cannot be noticed by a scheduler that looks every
        // thirty, and the failure is silent: the configured interval simply
        // stops meaning anything. Found by running the daemon with a short
        // interval and watching nothing happen.
        let tick =
            self.config.fetch_tick.min(self.config.fetch.interval).max(Duration::from_millis(50));
        let mut fetch_tick = tokio::time::interval(tick);
        fetch_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            self.pump();
            if !accepting && self.is_quiet() {
                // Whatever the last work produced still belongs in the cache,
                // and shutdown is the last chance to write it.
                self.flush_cache();
                break;
            }
            tokio::select! {
                _ = cache_tick.tick() => self.flush_cache(),
                _ = fetch_tick.tick(), if accepting => self.fetch_due(),
                cmd = cmd_rx.recv(), if accepting => match cmd {
                    Some(Cmd::Shutdown) | None => accepting = false,
                    Some(cmd) => self.on_cmd(cmd).await,
                },
                msg = internal_rx.recv() => match msg {
                    Some(msg) => self.on_internal(msg).await,
                    None => break,
                },
            }
        }
    }

    /// Nothing running, nothing queued, no probe outstanding.
    fn is_quiet(&self) -> bool {
        self.sched.is_idle() && self.traffic.is_idle() && self.scans.is_empty()
    }

    fn emit(&self, event: Event) {
        // An error means nobody is listening, which is normal for the CLI
        // between batches.
        let _ = self.events.send(event);
    }

    // ---- commands ------------------------------------------------------

    /// Async only because planning asks the probe a ref question.
    ///
    /// Every other arm is synchronous and stays that way. Awaiting here does
    /// stop the actor serving the next command until the plan is resolved — but
    /// it *yields*, so the probes and jobs on other tasks keep moving, which is
    /// exactly what a blocking `refs/` walk on this task took away from them.
    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::StartScan { roots, nested, reply } => {
                // Here rather than at construction, for two reasons: the roots
                // are what a cache is *for* and only arrive now, and nothing is
                // subscribed to the event channel until a surface has been
                // built — cached rows emitted before that would be published to
                // nobody and the warm launch would show an empty grid.
                self.serve_cache(&roots);
                let id = ScanId(self.next_scan);
                self.next_scan += 1;
                let walker = Walker::new(roots).options(WalkOptions {
                    nested: nested || self.config.nested,
                    max_depth: self.config.max_depth,
                });
                let stop = walker.cancel_flag();
                self.scans.insert(
                    id,
                    ScanRun {
                        stop,
                        walk_done: false,
                        errors: Vec::new(),
                        accepted: HashSet::new(),
                        pending: HashSet::new(),
                    },
                );

                let (found_tx, mut found_rx) = mpsc::unbounded_channel();
                let internal = self.internal.clone();
                // Blocking filesystem work, off the runtime's worker threads so
                // probes can start while it is still going.
                let walk = tokio::task::spawn_blocking(move || walker.walk(found_tx));
                tokio::spawn(async move {
                    while let Some(f) = found_rx.recv().await {
                        if internal
                            .send(Internal::Found { scan: Some(id), found: f })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    let errors = match walk.await {
                        Ok((_, errs)) => errs,
                        Err(e) => vec![DiscoveryError::Unreadable {
                            path: PathBuf::new(),
                            reason: format!("the walk task failed: {e}"),
                        }],
                    };
                    let _ = internal.send(Internal::WalkFinished { scan: Some(id), errors }).await;
                });
                let _ = reply.send(id);
            }

            Cmd::CancelScan { id } => {
                if let Some(scan) = self.scans.get(&id) {
                    scan.stop.store(true, Ordering::Relaxed);
                }
            }

            Cmd::Plan { action, sel, reply } => {
                let snaps = self.sorted_snapshots();
                let mut p = plan(&action, &snaps, &sel, SystemTime::now(), &self.config.policy);
                self.narrow_to_existing_refs(&mut p).await;
                self.resolve_default_branches(&mut p).await;
                self.resolve_tag_names(&mut p).await;
                let _ = reply.send(p);
            }

            Cmd::StartBatch { plan, origin, reply } => {
                let id = self.start_batch(plan, origin);
                let _ = reply.send(id);
            }

            Cmd::CancelBatch { id } => self.cancel_batch(id),

            Cmd::RefreshRepo { id } => self.request_probe(&id),

            Cmd::Invalidate { what } => self.on_invalidation(what),

            Cmd::SetWatched { covered } => {
                self.watched = covered;
                // Applied to what is already held, not only to what arrives
                // next: the coverage claim is about the repositories, and rows
                // discovered before the watcher started are the same rows.
                let moved: Vec<RepoSnapshot> = self
                    .snapshots
                    .values_mut()
                    .filter(|s| s.watched != covered)
                    .map(|s| {
                        s.watched = covered;
                        s.clone()
                    })
                    .collect();
                if !moved.is_empty() {
                    self.emit(Event::ReposUpserted(moved));
                }
            }

            Cmd::Snapshot { reply } => {
                let _ = reply.send(self.sorted_snapshots());
            }

            Cmd::Select { sel, reply } => {
                // Sorted for the same reason `Snapshot` is: a `HashMap`'s order
                // is not one, and a list that reshuffles between two identical
                // calls is a list nobody can diff.
                let mut matched: Vec<&RepoSnapshot> =
                    self.snapshots.values().filter(|s| sel.contains(s)).collect();
                matched.sort_by(|a, b| a.path.cmp(&b.path));
                let _ = reply.send(matched.into_iter().map(|s| s.id.clone()).collect());
            }

            Cmd::JobLog { id, reply } => {
                let log = self.jobs.get(&id).map(|j| j.log.clone()).unwrap_or_default();
                let _ = reply.send(log);
            }

            // Handled in `run`, which owns the flag.
            Cmd::Shutdown => {}

            Cmd::PlanUndo { batch, reply } => {
                let _ = reply.send(self.plan_undo(batch));
            }

            Cmd::StartUndo { batch, plan, reply } => {
                let id = self.start_batch(plan, JobOrigin::User);
                if let Some(run) = self.batches.get_mut(&id) {
                    run.batch.undoes = Some(batch);
                }
                let _ = reply.send(id);
            }

            Cmd::BackgroundJobs { reply } => {
                let jobs: Vec<Job> = self
                    .background_done
                    .iter()
                    .filter_map(|id| self.jobs.get(id).cloned())
                    .collect();
                let _ = reply.send(jobs);
            }

            Cmd::Jobs { batch, reply } => {
                let mut jobs: Vec<Job> = self
                    .batches
                    .get(&batch)
                    .map(|b| {
                        b.batch.jobs.iter().filter_map(|id| self.jobs.get(id).cloned()).collect()
                    })
                    .unwrap_or_default();
                jobs.sort_by_key(|j| j.id);
                let _ = reply.send(jobs);
            }
        }
    }

    fn start_batch(&mut self, plan: Plan, origin: JobOrigin) -> BatchId {
        let id = BatchId(self.next_batch);
        self.next_batch += 1;
        let cancel = CancellationToken::new();
        let mut job_ids = Vec::new();

        // Skips first, so a caller reading events in order sees the full shape
        // of the batch before anything starts running.
        for (repo, why) in &plan.skipped {
            let job_id = self.alloc_job();
            let job = Job::skipped(
                job_id,
                Some(id),
                origin,
                repo.clone(),
                plan.action.clone(),
                why.clone(),
            );
            self.announce(&job);
            self.jobs.insert(job_id, job);
            job_ids.push(job_id);
        }

        let mut outstanding = 0;
        for (repo, action) in &plan.eligible {
            let job_id = self.alloc_job();
            let mut job = Job::queued(job_id, Some(id), origin, repo.clone(), action.clone());
            // Stamped from the snapshot the plan was built against, which the
            // staleness precondition already guarantees is current. Undoing a
            // checkout needs the branch it came *from*, and a second
            // `rev-parse` would be paid on every commit and pull to serve only
            // this.
            job.branch_before =
                self.snapshots.get(repo).and_then(|s| s.branch()).map(str::to_string);
            self.enqueue(job, action.is_network());
            job_ids.push(job_id);
            outstanding += 1;
        }

        self.batches.insert(
            id,
            BatchRun {
                batch: Batch {
                    id,
                    action: plan.action.clone(),
                    origin,
                    jobs: job_ids,
                    started_at: SystemTime::now(),
                    finished_at: None,
                    undoes: None,
                },
                cancel,
                outstanding,
            },
        );
        if outstanding == 0 {
            self.finish_batch(id);
        }
        id
    }

    /// Move repositories that do not have the ref out of a checkout plan.
    ///
    /// The one eligibility question a snapshot cannot answer. A ref list is cold
    /// data, deliberately off the path that has to finish in under a second for
    /// a hundred repositories, so it is read here from the filesystem and only
    /// for the plan that needs it.
    ///
    /// Not a subprocess: `git rev-parse` per repository per plan is a cost the
    /// GUI would pay every time somebody proposes an action. The probe reads
    /// `refs/` and `packed-refs`, and **declines to answer** for anything
    /// carrying revision syntax — those are let through to try, because
    /// refusing a checkout that would have worked is worse than a job that
    /// fails with a good message.
    ///
    /// **Only `Some(false)` skips**, and that is the whole rule. A repository
    /// the probe could not read is let through for the same reason a revision
    /// expression is: `RefNotFound` would be a claim about this repository's
    /// refs, and an unreadable git directory is not evidence for it.
    async fn narrow_to_existing_refs(&self, p: &mut Plan) {
        let Action::Checkout { create: false, ref rev } = p.action else { return };
        let answers = self.ask_refs(p, RefQuery::Exists { rev: rev.clone() }).await;
        self.refine(p, |_, id, action| {
            let Action::Checkout { rev, .. } = &action else { return Ok(action) };
            match answers.get(id) {
                Some(Ok(RefAnswer::Exists(Some(false)))) => {
                    Err(SkipReason::RefNotFound(rev.clone()))
                }
                _ => Ok(action),
            }
        });
    }

    /// Ask the probe one ref question about every repository still eligible.
    ///
    /// **One call for the plan, not one per row.** That is what takes a hundred
    /// `refs/` walks off this task: the adapter gets the whole batch and hands
    /// it to a blocking pool in one go, and the actor awaits instead of pinning
    /// a runtime worker while it reads directories.
    ///
    /// Keyed by id on the way back rather than by position. `refine` walks the
    /// plan again afterwards, and lining two vectors up by index across that
    /// gap is exactly the bookkeeping that used to drop a repository into
    /// neither list. An id with no entry — no `found` to read, or an adapter
    /// that answered short — is one the caller accounts for as a skip, which is
    /// the safe direction to be wrong in.
    async fn ask_refs(
        &self,
        p: &Plan,
        query: RefQuery,
    ) -> HashMap<RepoId, Result<RefAnswer, RefError>> {
        let mut ids = Vec::with_capacity(p.eligible.len());
        let mut reqs = Vec::with_capacity(p.eligible.len());
        for (id, _) in &p.eligible {
            // No `found` is no git directory to read, which is the same "we do
            // not know what is in there" every one of these passes already
            // treats as a skip.
            let Some(found) = self.found.get(id) else { continue };
            // Only `DefaultBranch` reads these, and only that pass has a
            // snapshot to insist on. Defaulting to empty here keeps the other
            // two working for a repository that has been found but not yet
            // probed, which is what they do today.
            let remotes = self
                .snapshots
                .get(id)
                .map(|s| s.remotes.iter().map(|r| r.name.clone()).collect())
                .unwrap_or_default();
            ids.push(id.clone());
            reqs.push(RefRequest { git_dir: found.git_dir.clone(), remotes });
        }
        // Cloned so the borrow of `self` ends before the await: the actor owns
        // every map here, and holding one across a yield point is how the
        // single-task invariant starts costing more than it buys.
        let probe = Arc::clone(&self.probe);
        ids.into_iter().zip(probe.refs(reqs, query).await).collect()
    }

    /// Rewrite a plan's eligible entries, moving what cannot be resolved to its
    /// skips.
    ///
    /// The three passes around this are all one shape: ask the filesystem one
    /// question per repository, and either keep the row with a resolved action
    /// or account for it as a skip. Written out three times, the take-loop-
    /// reassign was three chances to drop a row into neither list — and a
    /// repository in neither list is the one thing a plan exists to make
    /// impossible.
    fn refine(
        &self,
        p: &mut Plan,
        resolve: impl Fn(&Self, &RepoId, Action) -> Result<Action, SkipReason>,
    ) {
        let mut kept = Vec::with_capacity(p.eligible.len());
        for (id, action) in std::mem::take(&mut p.eligible) {
            match resolve(self, &id, action) {
                Ok(action) => kept.push((id, action)),
                Err(why) => p.skipped.push((id, why)),
            }
        }
        p.eligible = kept;
    }

    /// Give every repository in a sync plan its own default branch.
    ///
    /// The second eligibility question a snapshot cannot answer, and the reason
    /// `Action::SyncDefault` carries an `Option` rather than a branch name:
    /// `main` versus `master` is not uniform across a working set, and the
    /// branch has to come from `refs/` — cold data, off the path that probes a
    /// hundred repositories in under a second.
    ///
    /// The rest of the resolution rides along because this is the one place
    /// that has both halves in hand: the snapshot says which branch to come
    /// back to and whether anything is in the way, the filesystem says where to
    /// go. Splitting them would mean a partially-resolved action existing in
    /// between, which is what the `Option` exists to forbid.
    async fn resolve_default_branches(&self, p: &mut Plan) {
        let Action::SyncDefault { mode, .. } = p.action else { return };
        let answers = self.ask_refs(p, RefQuery::DefaultBranch).await;
        self.refine(p, |actor, id, _| {
            let Some(snap) = actor.snapshots.get(id) else {
                return Err(SkipReason::SnapshotStale);
            };
            let default = match answers.get(id) {
                Some(Ok(RefAnswer::DefaultBranch(Some(name)))) => name.clone(),
                // A real answer of "no trunk I can name".
                Some(Ok(RefAnswer::DefaultBranch(None))) => {
                    return Err(SkipReason::NoDefaultBranch)
                }
                // Unreadable, or no answer at all. **Not** `NoDefaultBranch`:
                // this repository may well have one, and a plan that says it
                // does not is a false sentence the user is about to act on.
                _ => return Err(SkipReason::SnapshotStale),
            };
            // The precondition already refused a detached HEAD, which is the
            // case with no branch to come back to.
            let Some(back_to) = snap.branch().map(str::to_string) else {
                return Err(SkipReason::DetachedHead);
            };
            // Standing on the default branch already, with work in the tree.
            //
            // There is no switch to make, so the stash has nothing to clear out
            // of the way — and stashing purely so a pull can run on a dirty tree
            // is `git pull --autostash` under another name, which is refused
            // everywhere else. `Pull`'s own precondition is inherited rather
            // than restated: with no switch, this *is* a pull.
            //
            // Found by running it, not by reading it. The pop is otherwise
            // incapable of conflicting — it goes back onto the same tree it came
            // from, because the branch it was taken on never moved — and this is
            // the one arrangement where it does not. The symptom was a sync that
            // left conflict markers in a tracked file and reported `ok`.
            if default == back_to && !snap.is_clean() {
                return Err(SkipReason::DirtyWorktree);
            }
            let plan = git_scylla_core::SyncPlan {
                default,
                back_to,
                // Tracked work only. Untracked files are left where they are:
                // they rarely block a switch, and sweeping a build directory
                // into a stash entry to survive a fast-forward is a trade
                // nobody asked for.
                stash: snap.work.staged > 0 || snap.work.modified > 0,
            };
            Ok(Action::SyncDefault { mode, plan: Some(plan) })
        });
    }

    /// Give every repository in a tag plan the name derived from *its* tags.
    ///
    /// The third question a snapshot cannot answer. The plan must read "create
    /// `v2.4.0-dev.3`" per repository, because "create the next dev tag" is not
    /// something a user can check before confirming it forty times.
    ///
    /// The arithmetic is pure; this only supplies the tag list, read from
    /// `refs/` once per plan rather than once per row.
    ///
    /// **The tag list is the local one**, and that is worth being plain about:
    /// nothing here fetches, so a repository whose tags are behind the remote
    /// derives a name somebody else may already have taken. That failure is
    /// safe by construction rather than by luck — the steps publish before
    /// creating anything locally, and a name already on the remote is rejected
    /// with `(already exists)` — but it is a real failure, and
    /// `FailureKind::TagExists` is what tells the user to fetch tags and try
    /// again.
    async fn resolve_tag_names(&self, p: &mut Plan) {
        let Action::DevTag { ref channel, bump, ref push, .. } = p.action else { return };
        let (channel, template_push) = (channel.clone(), push.clone());
        let answers = self.ask_refs(p, RefQuery::Tags).await;
        self.refine(p, |_, id, action| {
            // The remote is already resolved per repository by `plan::resolve`;
            // only the name is missing, so the resolved action's own `push` is
            // the one to keep.
            let push = match action {
                Action::DevTag { push, .. } => push,
                _ => template_push.clone(),
            };
            // A repository that could not be read is skipped rather than
            // treated as having no tags. The difference is not cosmetic: an
            // empty list derives `dev.1`, so the plan would offer to cut a name
            // this repository may well have used already.
            let Some(Ok(RefAnswer::Tags(have))) = answers.get(id) else {
                return Err(SkipReason::SnapshotStale);
            };
            let name = git_scylla_core::version::next_dev_tag(have, &channel, bump);
            Ok(Action::DevTag { channel: channel.clone(), bump, name: Some(name), push })
        });
    }

    /// Compute an undo plan for a finished batch.
    fn plan_undo(&mut self, batch: BatchId) -> Plan {
        let Some(run) = self.batches.get(&batch) else {
            return empty_undo();
        };
        // **Never undo an undo.** One level, explicit, recent: a stack would
        // need a history the tool deliberately does not keep, and a second undo
        // of the same work is a `reset --hard` whose target nobody chose.
        if run.batch.undoes.is_some() {
            return empty_undo();
        }
        let jobs: Vec<Job> =
            run.batch.jobs.iter().filter_map(|id| self.jobs.get(id).cloned()).collect();
        // Requested, not awaited — see `Cmd::PlanUndo`.
        for job in &jobs {
            self.request_probe(&job.repo);
        }
        let snaps = self.sorted_snapshots();
        crate::plan::undo(&jobs, &snaps, SystemTime::now(), &self.config.policy)
    }

    /// Start a background fetch of everything whose slot has come round.
    ///
    /// **The scheduler proposes; the engine disposes.** `due` is pure and holds
    /// no state, so everything it cannot know is decided here — and a repository
    /// dropped for any of those reasons is a *skipped tick*, not a failed fetch.
    /// Recording a failure would push a healthy repository toward quarantine for
    /// being busy.
    fn fetch_due(&mut self) {
        if !self.scan_settled || !self.config.fetch.enabled {
            return;
        }
        // Suspended while the user is running something. Their batch has the
        // permits and the priority; adding background network work to it is how
        // a fetch cycle becomes something the user can feel.
        if self.user_batch_in_flight() {
            return;
        }
        let snaps = self.sorted_snapshots();
        let due = due(SystemTime::now(), &snaps, &self.config.fetch, &self.config.policy);
        for repo in due {
            if self.sched.is_busy(&repo)
                || self.queued_for(&repo)
                || !self.found.contains_key(&repo)
            {
                continue;
            }
            self.start_background_fetch(repo);
        }
    }

    /// Is a user batch still running?
    fn user_batch_in_flight(&self) -> bool {
        self.batches.values().any(|b| b.batch.origin == JobOrigin::User && b.outstanding > 0)
    }

    /// Is a job for this repository already waiting?
    fn queued_for(&self, repo: &RepoId) -> bool {
        self.jobs.values().any(|j| j.repo == *repo && !j.state.is_terminal())
    }

    /// One fetch, with no batch and no plan sheet.
    ///
    /// The only action allowed to skip the plan-confirm flow, and the exemption
    /// is closed to it: `Fetch` cannot touch a worktree, move `HEAD` or create
    /// local history. Nothing else inherits this by analogy.
    fn start_background_fetch(&mut self, repo: RepoId) {
        const ACTION: Action = Action::Fetch { prune: true, tags: false };
        let id = self.alloc_job();
        let job = Job::queued(id, None, JobOrigin::Background, repo, ACTION);
        self.enqueue(job, true);
    }

    /// Tell everyone a job exists, in whatever state it was created in.
    ///
    /// Announced on creation and not on first run. Without this a queued job is
    /// invisible until it starts, and a drawer showing a batch of forty would
    /// fill in from nothing as the scheduler let jobs through — which is
    /// exactly the state the user wants to see.
    fn announce(&self, job: &Job) {
        self.emit(Event::JobStateChanged {
            id: job.id,
            batch: job.batch,
            origin: job.origin,
            repo: job.repo.clone(),
            state: job.state.clone(),
        });
    }

    /// Announce a runnable job, record it, and hand it to the scheduler.
    ///
    /// One shape for a user batch's jobs and for a background fetch, because
    /// the three steps are not independent: a job in `self.jobs` that never
    /// reached the scheduler never runs, and one the scheduler holds but
    /// nothing announced is a row that appears out of nowhere when it finishes.
    fn enqueue(&mut self, job: Job, network: bool) {
        self.announce(&job);
        let ticket = Ticket {
            job: job.id,
            repo: job.repo.clone(),
            host: self.host_of(&job.repo),
            class: job.origin,
            network,
        };
        self.jobs.insert(job.id, job);
        self.sched.enqueue(ticket);
    }

    /// Record what a finished fetch did to a repository's schedule.
    ///
    /// A **user** fetch is the reset button: it clears backoff and quarantine
    /// whatever the outcome, because the user asking is the only thing that
    /// restarts a quarantined repository.
    fn record_fetch(&mut self, job: &Job) {
        if !matches!(job.action, Action::Fetch { .. }) {
            return;
        }
        let outcome = match &job.state {
            JobState::Ok => Attempt::Ok,
            JobState::Failed { .. } => Attempt::Failed(first_error(job)),
            // Cancelled or skipped: nothing was attempted, so nothing is
            // recorded. A dropped attempt must not count toward backoff.
            _ => return,
        };
        let Some(snap) = self.snapshots.get(&job.repo) else { return };
        let now = SystemTime::now();
        let health = match job.origin {
            JobOrigin::User => {
                manual_attempt(&snap.fetch, &job.repo, now, outcome, &self.config.fetch)
            }
            JobOrigin::Background => {
                after_attempt(&snap.fetch, &job.repo, now, outcome, &self.config.fetch)
            }
        };
        if let Some(snap) = self.snapshots.get_mut(&job.repo) {
            snap.fetch = health;
            self.cache_dirty = true;
        }
    }

    /// Keep the newest `background_history` background transcripts.
    ///
    /// Unbounded retention is fine for a session of user batches and is not
    /// fine for a process that fetches a hundred repositories every fifteen
    /// minutes for a week.
    fn evict_background(&mut self, id: JobId) {
        self.background_done.push_back(id);
        while self.background_done.len() > self.config.background_history {
            if let Some(old) = self.background_done.pop_front() {
                self.jobs.remove(&old);
            }
        }
    }

    /// Publish what a previous run knew, once, before the scan starts.
    ///
    /// The rows are stale by construction: each carries the `probed_at` of the
    /// run that wrote it, so `RepoSnapshot::is_stale` is true and the
    /// `SnapshotStale` precondition refuses every action on them until the scan
    /// replaces them. That is the whole safety argument for showing cached data
    /// at all — it is visible, and it is unusable.
    ///
    /// `found` is deliberately **not** seeded. A cached snapshot is something to
    /// look at, not something to run against, and the entry that would let a job
    /// launch belongs to the scan that confirms the repository still exists.
    fn serve_cache(&mut self, roots: &[PathBuf]) {
        self.cache_roots = roots.to_vec();
        if self.config.cache == CacheMode::Off || self.cache_served {
            return;
        }
        self.cache_served = true;
        let Some(cache) = git_scylla_store::Cache::load_for(roots) else { return };
        if cache.repos.is_empty() {
            return;
        }
        let mut repos = cache.repos;
        for snap in &mut repos {
            // Marked at the door rather than inferred from age downstream: a
            // cache written five seconds ago still describes a repository that
            // nothing watched while the application was closed.
            snap.from_cache = true;
            self.from_cache.insert(snap.id.clone());
            self.snapshots.insert(snap.id.clone(), snap.clone());
        }
        tracing::debug!(rows = repos.len(), "served the startup cache");
        self.emit(Event::ReposUpserted(repos));
    }

    /// Write the cache, if anything has moved since the last write.
    ///
    /// Called from a timer rather than on every change: a batch of forty pulls
    /// produces forty snapshot updates in a second, and rewriting the file for
    /// each is disk thrash for a file nobody reads until the next launch.
    fn flush_cache(&mut self) {
        if self.config.cache != CacheMode::ReadWrite
            || !self.cache_dirty
            || self.cache_roots.is_empty()
        {
            return;
        }
        self.cache_dirty = false;
        // Nothing still owed a first probe: a cached row that the scan has not
        // yet confirmed would otherwise be written back out as though this run
        // had seen it, and it would then survive a working set it is no longer
        // part of.
        let repos: Vec<RepoSnapshot> =
            self.snapshots.values().filter(|s| !self.from_cache.contains(&s.id)).cloned().collect();
        if let Err(e) =
            git_scylla_store::Cache::new(self.cache_roots.clone(), repos, SystemTime::now()).save()
        {
            // A cache that cannot be written costs a slower launch and nothing
            // else, so it is a warning rather than anything a caller hears.
            tracing::warn!(%e, "could not write the startup cache");
        }
    }

    /// Act on what a watcher saw.
    ///
    /// **The watcher proposes; the engine disposes** — the same rule the fetch
    /// scheduler follows. Every path here goes through `request_probe`, which
    /// honours the busy marker, so a watcher cannot re-probe a repository
    /// underneath a running job however many events it saw.
    fn on_invalidation(&mut self, what: git_scylla_watch::Invalidation) {
        use git_scylla_watch::Invalidation;
        match what {
            Invalidation::Repos(ids) => {
                for id in ids {
                    // Silently ignored for a repository the engine does not
                    // hold: the watcher's index is rebuilt on scan completion
                    // and can briefly name one this actor has already dropped.
                    if self.found.contains_key(&id) {
                        self.traffic.note(&id, Why::Observed);
                    }
                }
            }
            Invalidation::Gone(ids) => self.remove_repos(ids),
            Invalidation::Discover(path) => self.discover(path),
            Invalidation::Rescan => {
                // Nothing local can be trusted, and the engine does not keep
                // the roots — a scan is told them. Re-walking what it already
                // holds is the closest it can get on its own, and it is enough:
                // a repository that appeared while history was lost is found by
                // the `Discover` path or by the user's Refresh.
                let roots: Vec<PathBuf> = self.found.values().map(|f| f.path.clone()).collect();
                for root in roots {
                    self.discover(root);
                }
            }
        }
    }

    /// Drop repositories that are no longer on disk.
    ///
    /// The only thing in the engine that shrinks a map. A row for a directory
    /// the user deleted is worse than no row, because every action offered on
    /// it will fail — and `found` has to go too, or a queued job would still
    /// have somewhere to run.
    fn remove_repos(&mut self, ids: Vec<RepoId>) {
        let removed: Vec<RepoId> = ids
            .into_iter()
            .filter(|id| {
                // A repository with a job in flight is left alone: the job is
                // about to fail on its own and report why, which is a better
                // account than a row vanishing mid-batch.
                let busy = self.sched.is_busy(id);
                if !busy {
                    self.snapshots.remove(id);
                    self.found.remove(id);
                    self.traffic.forget(id);
                }
                !busy
            })
            .collect();
        if !removed.is_empty() {
            self.cache_dirty = true;
            self.emit(Event::ReposRemoved(removed));
        }
    }

    /// Walk one subtree and upsert whatever it finds.
    ///
    /// No `ScanRun`, so no progress and no `ScanDone`: cloning a repository
    /// should make it appear, not put a scan on screen. Nested is forced on,
    /// because the subtree being discovered is usually the repository itself
    /// and its own root would otherwise prune it.
    fn discover(&mut self, path: PathBuf) {
        let walker = Walker::new(vec![path])
            .options(WalkOptions { nested: self.config.nested, max_depth: self.config.max_depth });
        let (found_tx, mut found_rx) = mpsc::unbounded_channel();
        let internal = self.internal.clone();
        let walk = tokio::task::spawn_blocking(move || walker.walk(found_tx));
        tokio::spawn(async move {
            while let Some(f) = found_rx.recv().await {
                if internal.send(Internal::Found { scan: None, found: f }).await.is_err() {
                    return;
                }
            }
            let errors = walk.await.map(|(_, errs)| errs).unwrap_or_default();
            let _ = internal.send(Internal::WalkFinished { scan: None, errors }).await;
        });
    }

    /// Every snapshot, ordered by path.
    ///
    /// **Ordered, always.** `snapshots` is a `HashMap`, so anything reading it
    /// directly gets a different order every time — and a plan whose eligible
    /// and skipped lists reshuffle between two identical runs is a plan nobody
    /// trusts. `Plan` once read the map directly, which is how a plan sheet
    /// could list the same twenty repositories in a different order each time it
    /// was opened.
    fn sorted_snapshots(&self) -> Vec<RepoSnapshot> {
        let mut snaps: Vec<RepoSnapshot> = self.snapshots.values().cloned().collect();
        snaps.sort_by(|a, b| a.path.cmp(&b.path));
        snaps
    }

    /// Which host this repository's network work contends for.
    ///
    /// The upstream's remote if there is one, else `origin`, else the first.
    /// Inexact by design: a concurrency bucket, never a correctness input.
    fn host_of(&self, repo: &RepoId) -> Option<String> {
        let snap = self.snapshots.get(repo)?;
        let preferred = snap.upstream.as_ref().map(|u| u.remote.as_str()).unwrap_or("origin");
        snap.remotes
            .iter()
            .find(|r| r.name == preferred)
            .or_else(|| snap.remotes.first())
            .and_then(|r| r.host.clone())
    }

    fn cancel_batch(&mut self, id: BatchId) {
        let Some(run) = self.batches.get(&id) else { return };
        // Kills the process group of everything running under this batch;
        // queued jobs never start.
        run.cancel.cancel();
        let queued =
            self.sched.drain_queued(|t| self.jobs.get(&t.job).and_then(|j| j.batch) == Some(id));
        for ticket in queued {
            self.set_job_state(ticket.job, JobState::Cancelled);
            self.job_settled(ticket.job, &ticket.repo, Settled::NeverLaunched);
        }
    }

    fn alloc_job(&mut self) -> JobId {
        let id = JobId(self.next_job);
        self.next_job += 1;
        id
    }

    // ---- launching -----------------------------------------------------

    fn pump(&mut self) {
        for launch in self.sched.launchable() {
            let Some(job) = self.jobs.get(&launch.job) else {
                // No job record for a scheduled ticket. Unreachable today —
                // nothing removes from `self.jobs` — but the busy marker was set
                // inside `launchable`, so failing to hand it back would leave the
                // repository busy forever and the engine never idle.
                self.sched.finished(&launch.repo);
                continue;
            };
            let (id, repo, action) = (job.id, job.repo.clone(), job.action.clone());
            let Some(found) = self.found.get(&repo).cloned() else {
                // Discovered and then forgotten. Nothing to run against.
                self.set_job_state(id, JobState::Failed { code: -1 });
                self.job_settled(id, &repo, Settled::NotStarted);
                continue;
            };
            let cancel = job
                .batch
                .and_then(|b| self.batches.get(&b))
                .map(|b| b.cancel.clone())
                .unwrap_or_default();

            self.held.insert(id, launch.permits);
            self.set_job_state(id, JobState::Running);
            if let Some(j) = self.jobs.get_mut(&id) {
                j.started_at = Some(SystemTime::now());
            }

            let internal = self.internal.clone();
            let env = self.config.extra_env.clone();
            let timeout = launch.timeout;
            tokio::spawn(async move {
                let outcome = run_job(&found.path, &action, timeout, &cancel, &env).await;
                let _ =
                    internal.send(Internal::JobFinished { id, outcome: Box::new(outcome) }).await;
            });
        }

        // Everything owed a probe that may start now. One call: what is owed,
        // in what order, and how long a watcher's report has to wait are all
        // decided in `probe_traffic`, and none of it needs anything here.
        //
        // `can_start` answers for every reason this loop could fail to act, not
        // only for a job holding the repository — a repository handed back has
        // been marked as probing, so one that cannot be spawned would stay
        // marked and the engine would never be idle again.
        let can_start = |r: &RepoId| !self.sched.is_busy(r) && self.found.contains_key(r);
        for repo in self.traffic.take_ready(Instant::now(), can_start) {
            self.spawn_probe(&repo);
        }
    }

    /// Ask for a probe because something is known to have changed.
    ///
    /// Remembered rather than run: it is started by the next `pump`, which the
    /// run loop reaches before it waits again. Collapses with any other request
    /// for the same repository, so a pull that writes a thousand files still
    /// costs exactly one re-probe.
    fn request_probe(&mut self, repo: &RepoId) {
        // Nothing is owed to a repository this actor does not hold, and noting
        // one would keep the engine from ever being idle: what is owed is never
        // dropped, only deferred. `Cmd::RefreshRepo` carries whatever id a
        // surface sends, which need not name anything that was discovered.
        //
        // The watcher's path guards the same way, for its own reason — see
        // `on_invalidation`.
        if !self.found.contains_key(repo) {
            return;
        }
        self.traffic.note(repo, Why::Definite);
    }

    /// Start a probe that [`ProbeTraffic::take_ready`] has already admitted.
    ///
    /// The bookkeeping is done: the repository is marked as probing and the
    /// requests owed to it are cleared. This only has to spawn.
    fn spawn_probe(&mut self, repo: &RepoId) {
        let Some(found) = self.found.get(repo).cloned() else {
            // Unreachable — `can_start` checked exactly this, and nothing runs
            // between it and here. Handed back rather than dropped anyway: the
            // probing marker is set, and failing to release it would leave the
            // engine permanently un-idle, the same way `pump` hands a ticket
            // back to the scheduler above.
            self.traffic.finished(repo);
            return;
        };
        let (probe, internal, slots) =
            (Arc::clone(&self.probe), self.internal.clone(), Arc::clone(&self.probe_slots));
        let timeout = self.config.probe_timeout;
        tokio::spawn(async move {
            let _permit = slots.acquire().await.expect("probe semaphore");
            let snap =
                probe.probe(ProbeRequest { found, deadline: Instant::now() + timeout }).await;
            let _ = internal.send(Internal::Probed(Box::new(snap))).await;
        });
    }

    // ---- results -------------------------------------------------------

    async fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Found { scan, found } => {
                // The id comes from discovery and is never re-derived here.
                // Canonicalizing again could disagree with what the probe will
                // report — a path can vanish in between — and the mismatch would
                // leave `pending` permanently non-empty, so the scan would never
                // settle and `shutdown` would never return.
                let id = found.id.clone();
                self.found.insert(id.clone(), found);
                if let Some(run) = scan.and_then(|s| self.scans.get_mut(&s)) {
                    run.accepted.insert(id.clone());
                    run.pending.insert(id.clone());
                }
                self.request_probe(&id);
                if let Some(scan) = scan {
                    self.emit_scan_progress(scan);
                }
            }

            Internal::WalkFinished { scan, errors } => {
                // Logged at debug, not warn: the errors are now returned to
                // the caller through `ScanDone`, and whoever presents them
                // should not have to compete with a duplicate on stderr.
                for e in &errors {
                    tracing::debug!(error = %e, "discovery");
                }
                if let Some(run) = scan.and_then(|s| self.scans.get_mut(&s)) {
                    run.walk_done = true;
                    run.errors = errors;
                }
                self.settle_scans();
            }

            Internal::Probed(snap) => {
                self.traffic.finished(&snap.id);
                let mut snap = *snap;
                // Clears this repository from whichever scans were waiting on
                // it, and only those.
                let waiting: Vec<ScanId> = self
                    .scans
                    .iter_mut()
                    .filter_map(|(id, run)| run.pending.remove(&snap.id).then_some(*id))
                    .collect();
                self.from_cache.remove(&snap.id);
                snap.fetch = carried_health(self.snapshots.get(&snap.id), snap.fetch);
                snap.watched = self.watched;
                self.snapshots.insert(snap.id.clone(), snap.clone());
                self.cache_dirty = true;
                self.emit(Event::ReposUpserted(vec![snap]));
                for scan in waiting {
                    self.emit_scan_progress(scan);
                }
                self.settle_scans();
            }

            Internal::JobFinished { id, outcome } => {
                let Some(job) = self.jobs.get_mut(&id) else { return };
                let repo = job.repo.clone();
                job.steps = outcome.steps;
                job.head_before = outcome.head_before;
                job.head_after = outcome.head_after;
                job.finished_at = Some(SystemTime::now());
                let lines = outcome.log;
                if !lines.is_empty() {
                    job.log.extend(lines.iter().cloned());
                    self.emit(Event::JobLogAppended { id, lines });
                }
                self.set_job_state(id, outcome.state);
                if let Some(job) = self.jobs.get(&id).cloned() {
                    self.record_fetch(&job);
                    if job.origin == JobOrigin::Background {
                        self.evict_background(id);
                    }
                }
                self.job_settled(id, &repo, Settled::Ran);
            }
        }
    }

    /// Release a job's resources and account for it in its batch.
    ///
    /// What gets released depends on how far the job got — see [`Settled`].
    fn job_settled(&mut self, id: JobId, repo: &RepoId, settled: Settled) {
        // Drop the permits *before* telling the scheduler, so the capacity is
        // visible to the very next pump.
        self.held.remove(&id);
        match settled {
            // Ran `git`, so the repository may have moved: release it and
            // re-read it, exactly once.
            Settled::Ran => {
                self.sched.finished(repo);
                self.request_probe(repo);
            }
            // Took its repository but never spawned `git`. Release it; nothing
            // on disk moved, so a re-probe would be pure cost.
            Settled::NotStarted => self.sched.finished(repo),
            // Cancelled out of the queue, so it never held its repository —
            // the busy marker is set inside `Scheduler::launchable` and
            // nowhere else. Handing back a marker that belongs to a *different*
            // job would free the repository out from under it, and two `git`
            // processes in one repository is the failure the marker exists to
            // prevent.
            Settled::NeverLaunched => {}
        }
        if let Some(batch_id) = self.jobs.get(&id).and_then(|j| j.batch) {
            if let Some(run) = self.batches.get_mut(&batch_id) {
                run.outstanding = run.outstanding.saturating_sub(1);
                if run.outstanding == 0 {
                    self.finish_batch(batch_id);
                }
            }
        }
    }

    fn finish_batch(&mut self, id: BatchId) {
        let Some(run) = self.batches.get_mut(&id) else { return };
        run.batch.finished_at = Some(SystemTime::now());
        let duration = run.batch.duration().unwrap_or_default();
        let jobs: Vec<&Job> = run.batch.jobs.iter().filter_map(|j| self.jobs.get(j)).collect();
        let summary = BatchSummary::of(jobs, duration);
        self.emit(Event::BatchDone { id, summary });
    }

    fn set_job_state(&mut self, id: JobId, state: JobState) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.state = state.clone();
            if state.is_terminal() && job.finished_at.is_none() {
                job.finished_at = Some(SystemTime::now());
            }
            let (repo, batch, origin) = (job.repo.clone(), job.batch, job.origin);
            self.emit(Event::JobStateChanged { id, batch, origin, repo, state });
        }
    }

    fn emit_scan_progress(&self, scan: ScanId) {
        if let Some(run) = self.scans.get(&scan) {
            self.emit(Event::ScanProgress {
                scan,
                found: run.accepted.len(),
                probed: run.probed(),
            });
        }
    }

    /// A scan is done when its walk has finished and every repository **it**
    /// accepted has been probed.
    ///
    /// Deliberately not gated on the global set of in-flight probes: an
    /// unrelated re-probe from a running batch has nothing to do with whether
    /// this scan has finished reading the repositories it found.
    fn settle_scans(&mut self) {
        let done: Vec<ScanId> =
            self.scans.iter().filter(|(_, r)| r.settled()).map(|(id, _)| *id).collect();
        for id in done {
            let errors = self.scans.remove(&id).map(|r| r.errors).unwrap_or_default();
            // **After** the scan, not before. A cached row the walk did not
            // reach is only known to be gone once the walk has finished;
            // dropping it earlier would blank rows about to be confirmed, which
            // is the flicker the cache exists to remove.
            if !self.from_cache.is_empty() {
                let unconfirmed: Vec<RepoId> =
                    std::mem::take(&mut self.from_cache).into_iter().collect();
                self.remove_repos(unconfirmed);
            }
            // Nothing fetches until the initial scan has settled.
            self.scan_settled = true;
            self.emit(Event::ScanDone { scan: id, errors });
        }
    }
}

/// Keep the engine's fetch bookkeeping across a re-probe.
///
/// `FetchHealth` is engine-maintained, not probed — but the probe has to put
/// *something* there, and what it puts is "due now". Letting that win would
/// reset backoff and un-quarantine a broken remote on every re-probe, which with
/// a watcher running is every time a file changes: the quarantine would never
/// survive long enough to stop anything.
///
/// The probe's value is taken in exactly one case, because it is the one the
/// probe genuinely knows better: a repository whose remotes have gone has
/// nothing to fetch from, and `Disabled` is not a schedule the engine can
/// derive from a schedule.
fn carried_health(previous: Option<&RepoSnapshot>, probed: FetchHealth) -> FetchHealth {
    if probed.schedule == FetchSchedule::Disabled {
        return probed;
    }
    match previous {
        // ...and the converse: a repository that had no remote and now has one
        // takes the probe's fresh "due now".
        Some(prev) if prev.fetch.schedule != FetchSchedule::Disabled => prev.fetch.clone(),
        _ => probed,
    }
}

/// The first line git wrote to stderr, which is almost always the `fatal:` that
/// explains the failure.
///
/// First and not last: a fetch that fails on authentication says so up front and
/// then prints a hint about configuring credentials, and the hint is not the
/// reason.
fn first_error(job: &Job) -> &str {
    job.log
        .iter()
        .find(|l| l.stream == Stream::Stderr && !l.text.trim().is_empty())
        .map(|l| l.text.as_str())
        .unwrap_or("the fetch failed with no output")
}

/// An undo plan with nothing in it.
///
/// Returned for a batch the engine has forgotten and for one that is itself an
/// undo. Not a failure, and it does not need to be: `PlanView` renders "nothing
/// to do" and offers no confirm control, which is the right answer to both.
fn empty_undo() -> Plan {
    Plan {
        action: Action::Reset {
            to: git_scylla_core::Oid::parse("0000000").expect("static oid"),
            mode: git_scylla_core::ResetMode::Hard,
        },
        eligible: Vec::new(),
        skipped: Vec::new(),
        considered: 0,
        warning: None,
    }
}
