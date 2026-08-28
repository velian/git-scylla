//! The actor: a single `tokio::task` owning every mutable map — snapshots,
//! jobs, batches, scans, the scheduler, probe bookkeeping.
//!
//! The actor loop never blocks. The walk runs on a blocking thread; probes and
//! jobs run as spawned tasks. All report back through one internal channel the
//! actor drains alongside its commands.

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
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Serialized as a plain number, not ts-rs's default `bigint` for `u64`: Tauri's
/// IPC is JSON, and session counters never approach 2^53.
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
    /// Whether a watcher is covering the roots. Changes what a snapshot's age
    /// means: with coverage, an old snapshot is one nothing has changed;
    /// without it, one nobody has checked.
    SetWatched {
        covered: bool,
    },
    Snapshot {
        reply: oneshot::Sender<Vec<RepoSnapshot>>,
    },
    /// The ids matching a selection. Distinct from `Snapshot`: returns only
    /// ids, for callers that would otherwise clone and filter the whole
    /// working set.
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
    /// What undoing a finished batch would do. Requests a re-probe of every
    /// affected repository first, without waiting: a plan computed before the
    /// probes land is refused by the staleness guard rather than acting on old
    /// facts.
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
    /// [`Config::background_history`]. For a surface that subscribed late, or
    /// has none — the CLI's daemon.
    BackgroundJobs {
        reply: oneshot::Sender<Vec<Job>>,
    },
    /// Stop accepting commands and wind down.
    Shutdown,
}

/// Everything the actor publishes. Serializable: forwarded straight to the
/// webview, and the TypeScript bindings are generated from this enum.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Event {
    ReposUpserted(Vec<RepoSnapshot>),
    /// Repositories that are no longer on disk. Only a watcher can observe
    /// one going away.
    ReposRemoved(Vec<RepoId>),
    /// Progress of one scan. Carries the `ScanId`, since two scans can run at
    /// once.
    ScanProgress {
        scan: ScanId,
        found: usize,
        probed: usize,
    },
    /// A scan finished, with whatever it could not read. `errors` are
    /// structured, not stringified, so a caller can distinguish an invalid
    /// path from a permissions refusal.
    ScanDone {
        scan: ScanId,
        errors: Vec<DiscoveryError>,
    },
    /// One job moved. Emitted for every state a job reaches, `Queued`
    /// included, so the stream alone is enough to know a job exists. Carries
    /// `batch` and `origin` directly: events for a batch can arrive before
    /// `start_batch` returns its id.
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
    /// Every job in the batch has reached a terminal state. Precedes the
    /// re-probes: each job schedules one, and those land afterwards as
    /// `ReposUpserted`.
    BatchDone {
        id: BatchId,
        summary: BatchSummary,
    },
    /// A subscriber fell behind and events were dropped. Not produced by the
    /// actor: the broadcast channel reports lag, and the forwarder turns it
    /// into this. A consumer should re-read the snapshot.
    Lagged,
}

/// Results flowing back from spawned work.
enum Internal {
    /// `scan` is `None` for a targeted discovery pass, not a full scan.
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
    /// How often the fetch scheduler looks at the working set. Not the
    /// interval between a repository's fetches — that is
    /// `FetchPolicy::interval`.
    pub fetch_tick: Duration,
    /// Completed background jobs kept before the oldest is evicted.
    pub background_history: usize,
    /// How this engine uses the startup cache.
    pub cache: CacheMode,
}

/// What an engine does with the startup cache. Three states because reading
/// and writing are wanted separately: `git-scylla status` reads the cache but
/// must never write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Neither read nor written.
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

    /// Scan and wait for it to finish. Subscribes before sending, so a scan
    /// that completes immediately cannot race the subscription.
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
    /// Everything the walk could not read. An empty scan with non-empty
    /// `errors` is a permissions or configuration problem, not an empty
    /// working set.
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
    /// In-flight jobs run to completion rather than being abandoned
    /// mid-`git`. Use [`EngineHandle::cancel_batch`] first to stop them.
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
    /// Repositories accepted by this scan. A set, not a counter: the walk can
    /// report a repository more than once.
    accepted: HashSet<RepoId>,
    /// Accepted but not yet probed. A set, not a counter: two concurrent
    /// scans must not inflate each other's count. Cleared by any re-probe of
    /// the repository, not only one this scan triggered.
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

/// How far a job got before it reached a terminal state. Two independent
/// facts follow from it: whether the job still holds its repository, and
/// whether anything on disk may have moved.
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
    /// Who is owed a probe, who has one in flight, and when each last
    /// started. See [`crate::probe_traffic`].
    traffic: ProbeTraffic,

    /// Is a watcher covering the roots?
    watched: bool,

    /// Has any scan settled yet? Gates the fetch scheduler: the initial scan
    /// never touches the network.
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
    /// Returns the actor and the receiving half of its internal channel;
    /// `run` consumes the receiver.
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
        // The cache's write clock; writes are debounced, not per-change.
        let mut cache_tick = tokio::time::interval(CACHE_DEBOUNCE);
        cache_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The fetch scheduler's own clock; `due` itself is pure and stateless.
        // Never coarser than the interval being sampled, or a short interval
        // silently stops meaning anything.
        let tick =
            self.config.fetch_tick.min(self.config.fetch.interval).max(Duration::from_millis(50));
        let mut fetch_tick = tokio::time::interval(tick);
        fetch_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            self.pump();
            if !accepting && self.is_quiet() {
                // Last chance to write the cache before exit.
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
        // No listener is normal for the CLI between batches.
        let _ = self.events.send(event);
    }

    // ---- commands ------------------------------------------------------

    /// Async only because planning asks the probe a ref question. Awaiting
    /// here yields, so probes and jobs on other tasks keep moving while a
    /// command blocks.
    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::StartScan { roots, nested, reply } => {
                // Roots are only known now, and nothing subscribes before a
                // surface exists.
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
                // Applied to already-held snapshots too, not only future ones.
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
                // Sorted: HashMap order is not stable across calls.
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

        // Skips announced first, so events show the batch's full shape before
        // anything runs.
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
            // From the plan's snapshot, already guaranteed current. Undo
            // needs the branch a checkout came from; a fresh `rev-parse` here
            // would cost every commit and pull to serve only that.
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

    /// Move repositories that lack the given ref out of a checkout plan.
    ///
    /// Only `Some(false)` skips. A ref carrying revision syntax, or a
    /// repository the probe could not read, is left eligible to try rather
    /// than refused.
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

    /// Ask the probe one ref question about every repository still eligible,
    /// in one call. Answers are keyed by id, not position: a repository with
    /// no entry is accounted for as a skip by the caller.
    async fn ask_refs(
        &self,
        p: &Plan,
        query: RefQuery,
    ) -> HashMap<RepoId, Result<RefAnswer, RefError>> {
        let mut ids = Vec::with_capacity(p.eligible.len());
        let mut reqs = Vec::with_capacity(p.eligible.len());
        for (id, _) in &p.eligible {
            // No `found` entry means no git directory to read — treated as a
            // skip.
            let Some(found) = self.found.get(id) else { continue };
            // Only the DefaultBranch pass needs remotes; empty is fine for
            // the others.
            let remotes = self
                .snapshots
                .get(id)
                .map(|s| s.remotes.iter().map(|r| r.name.clone()).collect())
                .unwrap_or_default();
            ids.push(id.clone());
            reqs.push(RefRequest { git_dir: found.git_dir.clone(), remotes });
        }
        // Cloned so the borrow of self ends before the await.
        let probe = Arc::clone(&self.probe);
        ids.into_iter().zip(probe.refs(reqs, query).await).collect()
    }

    /// Rewrite a plan's eligible entries, moving what cannot be resolved to
    /// its skips. Shared by the three resolution passes below.
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
    /// The branch has to come from `refs/` — cold data, read once per plan
    /// rather than once per row — since `main` versus `master` is not uniform
    /// across a working set.
    async fn resolve_default_branches(&self, p: &mut Plan) {
        let Action::SyncDefault { mode, .. } = p.action else { return };
        let answers = self.ask_refs(p, RefQuery::DefaultBranch).await;
        self.refine(p, |actor, id, _| {
            let Some(snap) = actor.snapshots.get(id) else {
                return Err(SkipReason::SnapshotStale);
            };
            let default = match answers.get(id) {
                Some(Ok(RefAnswer::DefaultBranch(Some(name)))) => name.clone(),
                // A definite answer: this repository has no trunk.
                Some(Ok(RefAnswer::DefaultBranch(None))) => {
                    return Err(SkipReason::NoDefaultBranch)
                }
                // Unreadable or no answer. Not `NoDefaultBranch`: this
                // repository may still have one.
                _ => return Err(SkipReason::SnapshotStale),
            };
            // The precondition already refused a detached HEAD.
            let Some(back_to) = snap.branch().map(str::to_string) else {
                return Err(SkipReason::DetachedHead);
            };
            // Already on the default branch with a dirty tree: there is no
            // switch to make, so this is a plain pull on a dirty tree,
            // refused like any other.
            if default == back_to && !snap.is_clean() {
                return Err(SkipReason::DirtyWorktree);
            }
            let plan = git_scylla_core::SyncPlan {
                default,
                back_to,
                // Tracked work only; untracked files are left in place.
                stash: snap.work.staged > 0 || snap.work.modified > 0,
            };
            Ok(Action::SyncDefault { mode, plan: Some(plan) })
        });
    }

    /// Give every repository in a tag plan the name derived from *its* local
    /// tags, read from `refs/` once per plan. The list is local: nothing here
    /// fetches, so a derived name may already exist on the remote.
    async fn resolve_tag_names(&self, p: &mut Plan) {
        let Action::DevTag { ref channel, bump, ref push, .. } = p.action else { return };
        let (channel, template_push) = (channel.clone(), push.clone());
        let answers = self.ask_refs(p, RefQuery::Tags).await;
        self.refine(p, |_, id, action| {
            // The remote is already resolved by plan::resolve; only the name
            // is missing.
            let push = match action {
                Action::DevTag { push, .. } => push,
                _ => template_push.clone(),
            };
            // Unreadable is skipped, not treated as "no tags" — an empty
            // list would derive a name already in use.
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
        // Never undo an undo — one level only.
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

    /// Start a background fetch of everything whose slot has come round. A
    /// repository dropped here is a skipped tick, not a failed fetch —
    /// recording a failure would push a healthy repository toward quarantine
    /// for being busy.
    fn fetch_due(&mut self) {
        if !self.scan_settled || !self.config.fetch.enabled {
            return;
        }
        // Suspended while a user batch is running.
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

    /// One fetch, with no batch and no plan sheet. `Fetch` is the only
    /// action exempt from the plan-confirm flow: it cannot touch a worktree,
    /// move `HEAD`, or create local history.
    fn start_background_fetch(&mut self, repo: RepoId) {
        const ACTION: Action = Action::Fetch { prune: true, tags: false };
        let id = self.alloc_job();
        let job = Job::queued(id, None, JobOrigin::Background, repo, ACTION);
        self.enqueue(job, true);
    }

    /// Tell everyone a job exists, in whatever state it was created in.
    /// Announced on creation, not on first run, so a queued job is visible
    /// before the scheduler admits it.
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
    /// Shared by user batches and background fetches; the three steps must
    /// stay together.
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

    /// Record what a finished fetch did to a repository's schedule. A user
    /// fetch clears backoff and quarantine regardless of outcome.
    fn record_fetch(&mut self, job: &Job) {
        if !matches!(job.action, Action::Fetch { .. }) {
            return;
        }
        let outcome = match &job.state {
            JobState::Ok => Attempt::Ok,
            JobState::Failed { .. } => Attempt::Failed(first_error(job)),
            // Cancelled or skipped: nothing was attempted, so nothing is
            // recorded.
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

    /// Keep the newest `background_history` background transcripts, evicting
    /// the oldest.
    fn evict_background(&mut self, id: JobId) {
        self.background_done.push_back(id);
        while self.background_done.len() > self.config.background_history {
            if let Some(old) = self.background_done.pop_front() {
                self.jobs.remove(&old);
            }
        }
    }

    /// Publish what a previous run knew, once, before the scan starts. Rows
    /// are stale by construction, so `SnapshotStale` refuses every action on
    /// them until the scan replaces them. `found` is not seeded: a cached
    /// snapshot is visible but not runnable until the scan confirms it.
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
            // Marked explicitly; a cache written moments ago is still
            // unwatched.
            snap.from_cache = true;
            self.from_cache.insert(snap.id.clone());
            self.snapshots.insert(snap.id.clone(), snap.clone());
        }
        tracing::debug!(rows = repos.len(), "served the startup cache");
        self.emit(Event::ReposUpserted(repos));
    }

    /// Write the cache, if anything has moved since the last write. Called
    /// from a timer, not on every change, to avoid rewriting on every
    /// snapshot update.
    fn flush_cache(&mut self) {
        if self.config.cache != CacheMode::ReadWrite
            || !self.cache_dirty
            || self.cache_roots.is_empty()
        {
            return;
        }
        self.cache_dirty = false;
        // Excludes rows the scan has not yet confirmed.
        let repos: Vec<RepoSnapshot> =
            self.snapshots.values().filter(|s| !self.from_cache.contains(&s.id)).cloned().collect();
        if let Err(e) =
            git_scylla_store::Cache::new(self.cache_roots.clone(), repos, SystemTime::now()).save()
        {
            // Costs a slower next launch, nothing else — a warning, not an
            // error a caller hears.
            tracing::warn!(%e, "could not write the startup cache");
        }
    }

    /// Act on what a watcher saw. Every path goes through `request_probe`,
    /// which honours the busy marker: a watcher cannot re-probe a repository
    /// underneath a running job.
    fn on_invalidation(&mut self, what: git_scylla_watch::Invalidation) {
        use git_scylla_watch::Invalidation;
        match what {
            Invalidation::Repos(ids) => {
                for id in ids {
                    // Ignored for a repository the engine no longer holds.
                    if self.found.contains_key(&id) {
                        self.traffic.note(&id, Why::Observed);
                    }
                }
            }
            Invalidation::Gone(ids) => self.remove_repos(ids),
            Invalidation::Discover(path) => self.discover(path),
            Invalidation::Rescan => {
                // The engine does not keep the scan roots; re-walking what
                // it already holds is the closest it can get on its own.
                let roots: Vec<PathBuf> = self.found.values().map(|f| f.path.clone()).collect();
                for root in roots {
                    self.discover(root);
                }
            }
        }
    }

    /// Drop repositories that are no longer on disk. Removes from both
    /// `snapshots` and `found` — a queued job must have nowhere to run.
    fn remove_repos(&mut self, ids: Vec<RepoId>) {
        let removed: Vec<RepoId> = ids
            .into_iter()
            .filter(|id| {
                // A repository with a job in flight is left alone; the job
                // reports its own failure.
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

    /// Walk one subtree and upsert whatever it finds. No `ScanRun`: this
    /// should make a repository appear, not put a scan on screen. Nested is
    /// forced on, since the subtree is usually the repository's own root.
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

    /// Every snapshot, ordered by path. `snapshots` is a `HashMap`; anything
    /// reading it directly gets a different order each time.
    fn sorted_snapshots(&self) -> Vec<RepoSnapshot> {
        let mut snaps: Vec<RepoSnapshot> = self.snapshots.values().cloned().collect();
        snaps.sort_by(|a, b| a.path.cmp(&b.path));
        snaps
    }

    /// Which host this repository's network work contends for: the
    /// upstream's remote if there is one, else `origin`, else the first. A
    /// concurrency bucket, not a correctness input.
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
                // Unreachable in practice, but the busy marker was set
                // inside `launchable` and must be released regardless.
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

        // Everything owed a probe that may start now, decided by
        // `probe_traffic`. `can_start` must cover every reason a probe could
        // fail to spawn: a repository handed back is marked probing, and must
        // actually start or the engine is never idle again.
        let can_start = |r: &RepoId| !self.sched.is_busy(r) && self.found.contains_key(r);
        for repo in self.traffic.take_ready(Instant::now(), can_start) {
            self.spawn_probe(&repo);
        }
    }

    /// Ask for a probe because something is known to have changed.
    /// Remembered, not run immediately — started by the next `pump`.
    /// Collapses with any other pending request for the same repository.
    fn request_probe(&mut self, repo: &RepoId) {
        // A repository this actor does not hold is never owed a probe: what
        // is owed is deferred, never dropped, and would keep the engine from
        // ever being idle.
        if !self.found.contains_key(repo) {
            return;
        }
        self.traffic.note(repo, Why::Definite);
    }

    /// Start a probe that [`ProbeTraffic::take_ready`] has already admitted
    /// and marked as probing. This only has to spawn.
    fn spawn_probe(&mut self, repo: &RepoId) {
        let Some(found) = self.found.get(repo).cloned() else {
            // Unreachable in practice; the probing marker must still be
            // released.
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
                // The id is never re-derived: re-canonicalizing could
                // disagree with what the probe reports, leaving `pending`
                // permanently non-empty.
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
                // Debug, not warn: the caller gets these through `ScanDone`.
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
            // Ran `git`: the repository may have moved, so re-read it once.
            Settled::Ran => {
                self.sched.finished(repo);
                self.request_probe(repo);
            }
            // Took its repository but never spawned `git`: nothing on disk
            // moved, so skip the re-probe.
            Settled::NotStarted => self.sched.finished(repo),
            // Never held its repository; nothing to release.
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

    /// A scan is done when its walk has finished and every repository it
    /// accepted has been probed. Not gated on in-flight probes globally: an
    /// unrelated re-probe has nothing to do with this scan's own completion.
    fn settle_scans(&mut self) {
        let done: Vec<ScanId> =
            self.scans.iter().filter(|(_, r)| r.settled()).map(|(id, _)| *id).collect();
        for id in done {
            let errors = self.scans.remove(&id).map(|r| r.errors).unwrap_or_default();
            // After the scan, not before: a cached row is only known gone
            // once the walk finished.
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

/// Keep the engine's fetch bookkeeping across a re-probe. `FetchHealth` is
/// engine-maintained, not probed; the probe's freshly-derived "due now"
/// would otherwise reset backoff and quarantine on every re-probe. The
/// probe's value wins only when it reports `Disabled` — no remotes to fetch
/// from, which the engine cannot derive on its own.
fn carried_health(previous: Option<&RepoSnapshot>, probed: FetchHealth) -> FetchHealth {
    if probed.schedule == FetchSchedule::Disabled {
        return probed;
    }
    match previous {
        // A repository that gained a remote takes the probe's fresh schedule.
        Some(prev) if prev.fetch.schedule != FetchSchedule::Disabled => prev.fetch.clone(),
        _ => probed,
    }
}

/// The first line git wrote to stderr — almost always the `fatal:` that
/// explains the failure, not a trailing hint.
fn first_error(job: &Job) -> &str {
    job.log
        .iter()
        .find(|l| l.stream == Stream::Stderr && !l.text.trim().is_empty())
        .map(|l| l.text.as_str())
        .unwrap_or("the fetch failed with no output")
}

/// An undo plan with nothing in it. Returned for a batch the engine has
/// forgotten, and for one that is itself an undo.
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
