//! The actor: a single `tokio::task` owning every mutable map — snapshots,
//! jobs, batches, scans, the scheduler, probe bookkeeping.

use crate::plan::{plan, queries_for, Plan, PlanTemplate, RefAnswers};
use crate::policy::{after_attempt, due, manual_attempt, Attempt, FetchPolicy, Policy};
use crate::probe_traffic::{ProbeTraffic, Why};
use crate::runner::run_job;
use crate::sched::{Limits, Permits, Scheduler, Ticket};
use crate::Selection;
use git_scylla_core::{
    Action, Batch, BatchId, BatchSummary, FetchHealth, FetchSchedule, Job, JobId, JobOrigin,
    JobState, LogLine, RepoId, RepoSnapshot, Stream,
};
use git_scylla_discovery::{DiscoveryError, RepoFound, WalkOptions, Walker};
use git_scylla_probe::{GitCliProbe, Probe, ProbeRequest, RefQuery, RefRequest};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

const CACHE_DEBOUNCE: Duration = Duration::from_secs(2);

const PROBE_INTERVAL: Duration = Duration::from_secs(1);

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

pub enum Cmd {
    StartScan { roots: Vec<PathBuf>, nested: bool, reply: oneshot::Sender<ScanId> },
    CancelScan { id: ScanId },
    Plan { action: Action, sel: Selection, reply: oneshot::Sender<Plan> },
    StartBatch { plan: Plan, origin: JobOrigin, reply: oneshot::Sender<BatchId> },
    CancelBatch { id: BatchId },
    RefreshRepo { id: RepoId },
    Invalidate { what: git_scylla_watch::Invalidation },
    SetWatched { covered: bool },
    SetFetchInterval { interval: Duration },
    Snapshot { reply: oneshot::Sender<Vec<RepoSnapshot>> },
    Select { sel: Selection, reply: oneshot::Sender<Vec<RepoId>> },
    JobLog { id: JobId, reply: oneshot::Sender<Vec<LogLine>> },
    Jobs { batch: BatchId, reply: oneshot::Sender<Vec<Job>> },
    PlanUndo { batch: BatchId, reply: oneshot::Sender<Plan> },
    StartUndo { batch: BatchId, plan: Plan, reply: oneshot::Sender<BatchId> },
    BackgroundJobs { reply: oneshot::Sender<Vec<Job>> },
    Shutdown,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Event {
    ReposUpserted(Vec<RepoSnapshot>),
    ReposRemoved(Vec<RepoId>),
    ScanProgress {
        scan: ScanId,
        found: usize,
        probed: usize,
    },
    ScanDone {
        scan: ScanId,
        errors: Vec<DiscoveryError>,
    },
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
    BatchDone {
        id: BatchId,
        summary: BatchSummary,
    },
    Lagged,
}

enum Internal {
    Found { scan: Option<ScanId>, found: RepoFound },
    WalkFinished { scan: Option<ScanId>, errors: Vec<DiscoveryError> },
    Probed(Box<RepoSnapshot>),
    JobFinished { id: JobId, outcome: Box<crate::runner::JobOutcome> },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub limits: Limits,
    pub policy: Policy,
    pub nested: bool,
    pub max_depth: Option<usize>,
    pub probe_timeout: Duration,
    pub extra_env: Vec<(OsString, OsString)>,
    pub fetch: FetchPolicy,
    pub fetch_tick: Duration,
    pub background_history: usize,
    pub cache: CacheMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    #[default]
    Off,
    Read,
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

#[derive(Clone)]
pub struct EngineHandle {
    cmd: mpsc::Sender<Cmd>,
    events: broadcast::Sender<Event>,
}

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

    pub async fn set_watched(&self, covered: bool) -> Result<(), Gone> {
        self.send(Cmd::SetWatched { covered }).await
    }

    pub async fn set_fetch_interval(&self, interval: Duration) -> Result<(), Gone> {
        self.send(Cmd::SetFetchInterval { interval }).await
    }

    pub async fn invalidate(&self, what: git_scylla_watch::Invalidation) -> Result<(), Gone> {
        self.send(Cmd::Invalidate { what }).await
    }

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

    pub async fn select(&self, sel: Selection) -> Result<Vec<RepoId>, Gone> {
        self.ask(|reply| Cmd::Select { sel, reply }).await
    }

    pub async fn job_log(&self, id: JobId) -> Result<Vec<LogLine>, Gone> {
        self.ask(|reply| Cmd::JobLog { id, reply }).await
    }

    pub async fn jobs(&self, batch: BatchId) -> Result<Vec<Job>, Gone> {
        self.ask(|reply| Cmd::Jobs { batch, reply }).await
    }

    pub async fn plan_undo(&self, batch: BatchId) -> Result<Plan, Gone> {
        self.ask(|reply| Cmd::PlanUndo { batch, reply }).await
    }

    pub async fn start_undo(&self, batch: BatchId, plan: Plan) -> Result<BatchId, Gone> {
        self.ask(|reply| Cmd::StartUndo { batch, plan, reply }).await
    }

    pub async fn background_jobs(&self) -> Result<Vec<Job>, Gone> {
        self.ask(|reply| Cmd::BackgroundJobs { reply }).await
    }

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

#[derive(Debug, Clone)]
pub struct ScanOutcome {
    pub snapshots: Vec<RepoSnapshot>,
    pub errors: Vec<DiscoveryError>,
}

pub struct Engine {
    handle: EngineHandle,
    task: tokio::task::JoinHandle<()>,
}

impl Engine {
    pub fn start(config: Config) -> Self {
        Self::with_probe(config, Arc::new(GitCliProbe::new()))
    }

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
    accepted: HashSet<RepoId>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    Ran,
    NotStarted,
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
    held: HashMap<JobId, Permits>,
    traffic: ProbeTraffic,

    watched: bool,

    scan_settled: bool,
    background_done: VecDeque<JobId>,

    from_cache: HashSet<RepoId>,
    cache_served: bool,
    cache_dirty: bool,
    cache_roots: Vec<PathBuf>,

    next_job: u64,
    next_batch: u64,
    next_scan: u64,
}

impl Actor {
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
        let mut cache_tick = tokio::time::interval(CACHE_DEBOUNCE);
        cache_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                    Some(cmd) => self.on_cmd(cmd),
                },
                msg = internal_rx.recv() => match msg {
                    Some(msg) => self.on_internal(msg).await,
                    None => break,
                },
            }
        }
    }

    fn is_quiet(&self) -> bool {
        self.sched.is_idle() && self.traffic.is_idle() && self.scans.is_empty()
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::StartScan { roots, nested, reply } => {
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
                let t = plan(&action, &snaps, &sel, SystemTime::now(), &self.config.policy);
                self.spawn_resolve(t, snaps, reply);
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

            Cmd::SetFetchInterval { interval } => {
                self.config.fetch.interval = interval;
                let now = SystemTime::now();
                let fetch = self.config.fetch.clone();
                let moved: Vec<RepoSnapshot> = self
                    .snapshots
                    .values_mut()
                    .filter_map(|s| match s.fetch.schedule {
                        FetchSchedule::Due(at) => {
                            let bound = fetch.next_due(&s.id, now);
                            (at > bound).then(|| {
                                s.fetch.schedule = FetchSchedule::Due(bound);
                                s.clone()
                            })
                        }
                        _ => None,
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
                let mut matched: Vec<&RepoSnapshot> =
                    self.snapshots.values().filter(|s| sel.contains(s)).collect();
                matched.sort_by(|a, b| a.path.cmp(&b.path));
                let _ = reply.send(matched.into_iter().map(|s| s.id.clone()).collect());
            }

            Cmd::JobLog { id, reply } => {
                let log = self.jobs.get(&id).map(|j| j.log.clone()).unwrap_or_default();
                let _ = reply.send(log);
            }

            Cmd::Shutdown => {}

            Cmd::PlanUndo { batch, reply } => {
                let t = self.plan_undo(batch);
                let snaps = self.sorted_snapshots();
                self.spawn_resolve(t, snaps, reply);
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

    fn ref_requests(&self, t: &PlanTemplate) -> Vec<(RefQuery, Vec<RepoId>, Vec<RefRequest>)> {
        let mut work = Vec::new();
        for (query, ids) in queries_for(t) {
            let mut asked = Vec::with_capacity(ids.len());
            let mut reqs = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(found) = self.found.get(&id) else { continue };
                let remotes = self
                    .snapshots
                    .get(&id)
                    .map(|s| s.remotes.iter().map(|r| r.name.clone()).collect())
                    .unwrap_or_default();
                reqs.push(RefRequest { per_worktree_dir: found.per_worktree_dir.clone(), remotes });
                asked.push(id);
            }
            if !reqs.is_empty() {
                work.push((query, asked, reqs));
            }
        }
        work
    }

    fn spawn_resolve(
        &self,
        t: PlanTemplate,
        snaps: Vec<RepoSnapshot>,
        reply: oneshot::Sender<Plan>,
    ) {
        let work = self.ref_requests(&t);
        let probe = Arc::clone(&self.probe);
        tokio::spawn(async move {
            let mut answers = RefAnswers::new();
            for (query, ids, reqs) in work {
                answers.extend(ids.into_iter().zip(probe.refs(reqs, query).await));
            }
            let _ = reply.send(crate::plan::resolve(t, &snaps, &answers));
        });
    }

    fn plan_undo(&mut self, batch: BatchId) -> PlanTemplate {
        let now = SystemTime::now();
        let Some(run) = self.batches.get(&batch) else {
            return crate::plan::no_undo(now, &self.config.policy);
        };
        if run.batch.undoes.is_some() {
            return crate::plan::no_undo(now, &self.config.policy);
        }
        let jobs: Vec<Job> =
            run.batch.jobs.iter().filter_map(|id| self.jobs.get(id).cloned()).collect();
        for job in &jobs {
            self.request_probe(&job.repo);
        }
        let snaps = self.sorted_snapshots();
        crate::plan::undo(&jobs, &snaps, now, &self.config.policy)
    }

    fn fetch_due(&mut self) {
        if !self.scan_settled || !self.config.fetch.enabled {
            return;
        }
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

    fn user_batch_in_flight(&self) -> bool {
        self.batches.values().any(|b| b.batch.origin == JobOrigin::User && b.outstanding > 0)
    }

    fn queued_for(&self, repo: &RepoId) -> bool {
        self.jobs.values().any(|j| j.repo == *repo && !j.state.is_terminal())
    }

    fn start_background_fetch(&mut self, repo: RepoId) {
        const ACTION: Action = Action::Fetch { prune: true, tags: false };
        let id = self.alloc_job();
        let job = Job::queued(id, None, JobOrigin::Background, repo, ACTION);
        self.enqueue(job, true);
    }

    fn announce(&self, job: &Job) {
        self.emit(Event::JobStateChanged {
            id: job.id,
            batch: job.batch,
            origin: job.origin,
            repo: job.repo.clone(),
            state: job.state.clone(),
        });
    }

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

    fn record_fetch(&mut self, job: &Job) {
        if !matches!(job.action, Action::Fetch { .. }) {
            return;
        }
        let outcome = match &job.state {
            JobState::Ok => Attempt::Ok,
            JobState::Failed { .. } => Attempt::Failed(first_error(job)),
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

    fn evict_background(&mut self, id: JobId) {
        self.background_done.push_back(id);
        while self.background_done.len() > self.config.background_history {
            if let Some(old) = self.background_done.pop_front() {
                self.jobs.remove(&old);
            }
        }
    }

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
            snap.from_cache = true;
            self.from_cache.insert(snap.id.clone());
            self.snapshots.insert(snap.id.clone(), snap.clone());
        }
        tracing::debug!(rows = repos.len(), "served the startup cache");
        self.emit(Event::ReposUpserted(repos));
    }

    fn flush_cache(&mut self) {
        if self.config.cache != CacheMode::ReadWrite
            || !self.cache_dirty
            || self.cache_roots.is_empty()
        {
            return;
        }
        self.cache_dirty = false;
        let repos: Vec<RepoSnapshot> =
            self.snapshots.values().filter(|s| !self.from_cache.contains(&s.id)).cloned().collect();
        if let Err(e) =
            git_scylla_store::Cache::new(self.cache_roots.clone(), repos, SystemTime::now()).save()
        {
            tracing::warn!(%e, "could not write the startup cache");
        }
    }

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
                let roots: Vec<PathBuf> = self.found.values().map(|f| f.path.clone()).collect();
                for root in roots {
                    self.discover(root);
                }
            }
        }
    }

    fn remove_repos(&mut self, ids: Vec<RepoId>) {
        let removed: Vec<RepoId> = ids
            .into_iter()
            .filter(|id| {
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

    fn sorted_snapshots(&self) -> Vec<RepoSnapshot> {
        let mut snaps: Vec<RepoSnapshot> = self.snapshots.values().cloned().collect();
        snaps.sort_by(|a, b| a.path.cmp(&b.path));
        snaps
    }

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

    fn pump(&mut self) {
        for launch in self.sched.launchable() {
            let Some(job) = self.jobs.get(&launch.job) else {
                self.sched.finished(&launch.repo);
                continue;
            };
            let (id, repo, action) = (job.id, job.repo.clone(), job.action.clone());
            let Some(found) = self.found.get(&repo).cloned() else {
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

        let can_start = |r: &RepoId| !self.sched.is_busy(r) && self.found.contains_key(r);
        for repo in self.traffic.take_ready(Instant::now(), can_start) {
            self.spawn_probe(&repo);
        }
    }

    fn request_probe(&mut self, repo: &RepoId) {
        if !self.found.contains_key(repo) {
            return;
        }
        self.traffic.note(repo, Why::Definite);
    }

    fn spawn_probe(&mut self, repo: &RepoId) {
        let Some(found) = self.found.get(repo).cloned() else {
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

    async fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Found { scan, found } => {
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

    fn job_settled(&mut self, id: JobId, repo: &RepoId, settled: Settled) {
        self.held.remove(&id);
        match settled {
            Settled::Ran => {
                self.sched.finished(repo);
                self.request_probe(repo);
            }
            Settled::NotStarted => self.sched.finished(repo),
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

    fn settle_scans(&mut self) {
        let done: Vec<ScanId> =
            self.scans.iter().filter(|(_, r)| r.settled()).map(|(id, _)| *id).collect();
        for id in done {
            let errors = self.scans.remove(&id).map(|r| r.errors).unwrap_or_default();
            if !self.from_cache.is_empty() {
                let unconfirmed: Vec<RepoId> =
                    std::mem::take(&mut self.from_cache).into_iter().collect();
                self.remove_repos(unconfirmed);
            }
            self.scan_settled = true;
            self.emit(Event::ScanDone { scan: id, errors });
        }
    }
}

fn carried_health(previous: Option<&RepoSnapshot>, probed: FetchHealth) -> FetchHealth {
    if probed.schedule == FetchSchedule::Disabled {
        return probed;
    }
    match previous {
        Some(prev) if prev.fetch.schedule != FetchSchedule::Disabled => prev.fetch.clone(),
        _ => probed,
    }
}

fn first_error(job: &Job) -> &str {
    job.log
        .iter()
        .find(|l| l.stream == Stream::Stderr && !l.text.trim().is_empty())
        .map(|l| l.text.as_str())
        .unwrap_or("the fetch failed with no output")
}
