//! Deciding what may run right now.
//!
//! Three constraints, satisfied together without deadlocking each other:
//!
//! * **One job per repository, ever.** Two concurrent `git` processes in one
//!   repository contend for `index.lock`.
//! * **Global concurrency**, separately for network and local work.
//! * **Per-host concurrency** for network work, so many fetches against one
//!   host do not invite rate limiting or SSH `MaxSessions` refusals.
//!
//! Permits are acquired by the scheduler, before spawning, and moved into the
//! task so they release on drop — acquiring inside the task would let the
//! priority class and per-host cap become mere suggestions.

use git_scylla_core::{JobId, JobOrigin, RepoId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Concurrency and deadline limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Concurrent network jobs across all hosts. Eight: the constraint is the
    /// far end, which rate-limits or refuses SSH `MaxSessions` past that.
    pub network: usize,
    /// Concurrent local jobs. `num_cpus`, because local work is CPU- and
    /// disk-bound rather than latency-bound.
    pub local: usize,
    /// Concurrent network jobs against any single host.
    pub per_host: usize,
    pub network_timeout: Duration,
    /// Deadline for local jobs. Five seconds suits `checkout`/`stash`; a
    /// caller can raise it per-action for something like `commit`, whose
    /// hooks run the user's own test suite.
    pub local_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            network: 8,
            local: std::thread::available_parallelism().map_or(4, |n| n.get()),
            per_host: 3,
            network_timeout: Duration::from_secs(60),
            local_timeout: Duration::from_secs(5),
        }
    }
}

/// A job waiting to run, reduced to what scheduling needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub job: JobId,
    pub repo: RepoId,
    /// Bucket key for the per-host cap. `None` shares one bucket: a path
    /// remote and an unparseable URL contend with each other and nothing else.
    pub host: Option<String>,
    pub class: JobOrigin,
    pub network: bool,
}

/// A job cleared to start, with the permits that authorise it. Dropping this
/// without spawning releases the permits.
#[derive(Debug)]
pub struct Launch {
    pub job: JobId,
    /// Carried so a caller that decides not to start the job can still hand
    /// it back via [`Scheduler::finished`].
    pub repo: RepoId,
    pub timeout: Duration,
    pub permits: Permits,
}

/// Capacity held for the lifetime of one job.
#[derive(Debug)]
pub struct Permits {
    _global: OwnedSemaphorePermit,
    /// `None` for local work, which has no host.
    _host: Option<OwnedSemaphorePermit>,
}

pub struct Scheduler {
    limits: Limits,
    network: Arc<Semaphore>,
    local: Arc<Semaphore>,
    /// Created lazily as hosts are seen.
    hosts: HashMap<Option<String>, Arc<Semaphore>>,
    /// Ready to run, in arrival order, one queue per priority class.
    ready: [VecDeque<Ticket>; 2],
    /// Waiting only because their repository is busy.
    per_repo: HashMap<RepoId, VecDeque<Ticket>>,
    busy: HashSet<RepoId>,
}

fn class_index(class: JobOrigin) -> usize {
    match class {
        JobOrigin::User => 0,
        JobOrigin::Background => 1,
    }
}

impl Scheduler {
    pub fn new(limits: Limits) -> Self {
        Self {
            network: Arc::new(Semaphore::new(limits.network)),
            local: Arc::new(Semaphore::new(limits.local)),
            hosts: HashMap::new(),
            ready: [VecDeque::new(), VecDeque::new()],
            per_repo: HashMap::new(),
            busy: HashSet::new(),
            limits,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Accept a job. If its repository already has something in flight, the
    /// job goes to that repository's own queue instead of `ready` — making
    /// per-repository serialization a property of the data structure, not a
    /// check someone could forget.
    pub fn enqueue(&mut self, ticket: Ticket) {
        if self.busy.contains(&ticket.repo) {
            self.per_repo.entry(ticket.repo.clone()).or_default().push_back(ticket);
        } else {
            self.ready[class_index(ticket.class)].push_back(ticket);
        }
    }

    /// Everything that can start now, in priority order: `User` before
    /// `Background`, FIFO within a class, except that a job whose host is
    /// saturated does not block the jobs behind it in the queue.
    pub fn launchable(&mut self) -> Vec<Launch> {
        let mut launched = Vec::new();
        for class in 0..self.ready.len() {
            let mut deferred = VecDeque::new();
            while let Some(ticket) = self.ready[class].pop_front() {
                // May have gone busy since it was queued.
                if self.busy.contains(&ticket.repo) {
                    self.per_repo.entry(ticket.repo.clone()).or_default().push_back(ticket);
                    continue;
                }
                match self.acquire(&ticket) {
                    Some(permits) => {
                        self.busy.insert(ticket.repo.clone());
                        launched.push(Launch {
                            job: ticket.job,
                            repo: ticket.repo.clone(),
                            timeout: self.timeout_for(&ticket),
                            permits,
                        });
                    }
                    None => deferred.push_back(ticket),
                }
            }
            self.ready[class] = deferred;
        }
        launched
    }

    /// Global permit first, host permit second, always — the fixed order
    /// rules out deadlock between the two.
    fn acquire(&mut self, ticket: &Ticket) -> Option<Permits> {
        if !ticket.network {
            let global = Arc::clone(&self.local).try_acquire_owned().ok()?;
            return Some(Permits { _global: global, _host: None });
        }
        let global = Arc::clone(&self.network).try_acquire_owned().ok()?;
        let host_sem = Arc::clone(
            self.hosts
                .entry(ticket.host.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(self.limits.per_host))),
        );
        match host_sem.try_acquire_owned() {
            Ok(host) => Some(Permits { _global: global, _host: Some(host) }),
            // `global` drops here, releasing it.
            Err(_) => None,
        }
    }

    fn timeout_for(&self, ticket: &Ticket) -> Duration {
        if ticket.network {
            self.limits.network_timeout
        } else {
            self.limits.local_timeout
        }
    }

    /// A job finished. Releases its repository and promotes the next job
    /// queued against it, if any. The caller must drop the job's [`Permits`]
    /// first, or freed capacity won't be visible until the next pump.
    pub fn finished(&mut self, repo: &RepoId) {
        self.busy.remove(repo);
        if let Some(queue) = self.per_repo.get_mut(repo) {
            if let Some(next) = queue.pop_front() {
                self.ready[class_index(next.class)].push_back(next);
            }
            if queue.is_empty() {
                self.per_repo.remove(repo);
            }
        }
    }

    /// Remove every queued job matching `pred`, returning them, for batch
    /// cancellation. Running jobs are killed separately, through their
    /// process group, by `crates/exec`.
    pub fn drain_queued(&mut self, pred: impl Fn(&Ticket) -> bool) -> Vec<Ticket> {
        let mut removed = Vec::new();
        for queue in &mut self.ready {
            let mut kept = VecDeque::new();
            while let Some(t) = queue.pop_front() {
                if pred(&t) {
                    removed.push(t);
                } else {
                    kept.push_back(t);
                }
            }
            *queue = kept;
        }
        self.per_repo.retain(|_, queue| {
            let mut kept = VecDeque::new();
            while let Some(t) = queue.pop_front() {
                if pred(&t) {
                    removed.push(t);
                } else {
                    kept.push_back(t);
                }
            }
            *queue = kept;
            !queue.is_empty()
        });
        removed
    }

    pub fn is_busy(&self, repo: &RepoId) -> bool {
        self.busy.contains(repo)
    }

    pub fn queued(&self) -> usize {
        self.ready.iter().map(|q| q.len()).sum::<usize>()
            + self.per_repo.values().map(|q| q.len()).sum::<usize>()
    }

    pub fn in_flight(&self) -> usize {
        self.busy.len()
    }

    pub fn is_idle(&self) -> bool {
        self.queued() == 0 && self.in_flight() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(n: usize) -> RepoId {
        RepoId::from_canonical(format!("/r/{n}"))
    }

    fn net(job: u64, r: usize, host: Option<&str>) -> Ticket {
        Ticket {
            job: JobId(job),
            repo: repo(r),
            host: host.map(str::to_string),
            class: JobOrigin::User,
            network: true,
        }
    }

    fn local(job: u64, r: usize) -> Ticket {
        Ticket {
            job: JobId(job),
            repo: repo(r),
            host: None,
            class: JobOrigin::User,
            network: false,
        }
    }

    fn limits(network: usize, per_host: usize) -> Limits {
        Limits { network, per_host, local: 4, ..Default::default() }
    }

    #[test]
    fn the_global_network_cap_is_respected() {
        let mut s = Scheduler::new(limits(4, 99));
        for i in 0..50 {
            s.enqueue(net(i as u64, i, Some("github.com")));
        }
        let first = s.launchable();
        assert_eq!(first.len(), 4, "the global cap is 4");
        assert_eq!(s.queued(), 46);
        // Nothing more starts until something finishes and its permits drop.
        assert!(s.launchable().is_empty());
    }

    #[test]
    fn capacity_frees_only_when_the_permits_drop() {
        let mut s = Scheduler::new(limits(2, 99));
        s.enqueue(net(1, 1, Some("h")));
        s.enqueue(net(2, 2, Some("h")));
        s.enqueue(net(3, 3, Some("h")));
        let launched = s.launchable();
        assert_eq!(launched.len(), 2);

        // Marking the repository free is not enough; the permit is the capacity.
        s.finished(&repo(1));
        assert!(s.launchable().is_empty(), "the permit is still held");

        drop(launched);
        assert_eq!(s.launchable().len(), 1);
    }

    #[test]
    fn the_per_host_cap_holds_and_does_not_serialize_across_hosts() {
        // Both halves matter: a per-host cap that accidentally serializes
        // everything is not obviously broken.
        let mut s = Scheduler::new(limits(8, 2));
        for i in 0..30 {
            let host = ["a.example", "b.example", "c.example"][i % 3];
            s.enqueue(net(i as u64, i, Some(host)));
        }
        let launched = s.launchable();
        // 3 hosts x 2 = 6, under the global cap of 8: per-host is the binding constraint.
        assert_eq!(launched.len(), 6, "expected 2 per host across 3 hosts");
        assert!(launched.len() < 8, "and the global cap should not be the binding constraint");
    }

    #[test]
    fn a_saturated_host_does_not_block_jobs_behind_it() {
        // Strict FIFO would let one rate-limited host stall a whole batch.
        let mut s = Scheduler::new(limits(8, 1));
        s.enqueue(net(1, 1, Some("slow")));
        s.enqueue(net(2, 2, Some("slow")));
        s.enqueue(net(3, 3, Some("slow")));
        s.enqueue(net(4, 4, Some("fast")));

        let launched = s.launchable();
        let ids: Vec<u64> = launched.iter().map(|l| l.job.0).collect();
        assert_eq!(ids, vec![1, 4], "job 4 started despite 2 and 3 being blocked");
        assert_eq!(s.queued(), 2);
    }

    #[test]
    fn hosts_with_no_name_share_one_bucket() {
        // A path remote and an unparseable URL contend with each other and with
        // nothing else, which is right for both.
        let mut s = Scheduler::new(limits(8, 2));
        for i in 0..5 {
            s.enqueue(net(i as u64, i, None));
        }
        assert_eq!(s.launchable().len(), 2);
    }

    #[test]
    fn local_and_network_capacity_are_independent() {
        let mut s =
            Scheduler::new(Limits { network: 1, local: 2, per_host: 1, ..Default::default() });
        s.enqueue(net(1, 1, Some("h")));
        s.enqueue(net(2, 2, Some("h")));
        s.enqueue(local(3, 3));
        s.enqueue(local(4, 4));
        s.enqueue(local(5, 5));
        let launched = s.launchable();
        let ids: Vec<u64> = launched.iter().map(|l| l.job.0).collect();
        assert_eq!(ids, vec![1, 3, 4], "1 network + 2 local, and job 2 waits on the network cap");
    }

    #[test]
    fn two_jobs_for_one_repository_never_run_together() {
        // The rule that keeps index.lock contention from presenting as random
        // failure.
        let mut s = Scheduler::new(limits(8, 8));
        s.enqueue(net(1, 1, Some("h")));
        s.enqueue(net(2, 1, Some("h")));
        s.enqueue(net(3, 1, Some("h")));

        let first = s.launchable();
        assert_eq!(first.len(), 1);
        assert!(s.is_busy(&repo(1)));
        assert_eq!(s.queued(), 2, "the other two wait on the repository, not on capacity");

        drop(first);
        assert!(s.launchable().is_empty(), "still busy until finished() is called");

        s.finished(&repo(1));
        let second = s.launchable();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].job, JobId(2), "FIFO within the repository");
        drop(second);
        s.finished(&repo(1));
        assert_eq!(s.launchable()[0].job, JobId(3));
    }

    #[test]
    fn a_repository_that_goes_busy_after_queueing_is_re_queued_not_run() {
        let mut s = Scheduler::new(limits(8, 8));
        s.enqueue(net(1, 1, Some("h")));
        let held = s.launchable();
        // Job 2 arrives for the same repository while job 1 runs.
        s.enqueue(net(2, 1, Some("h")));
        assert!(s.launchable().is_empty());
        drop(held);
        s.finished(&repo(1));
        assert_eq!(s.launchable().len(), 1);
    }

    #[test]
    fn user_jobs_outrank_background_ones() {
        let mut s = Scheduler::new(limits(2, 8));
        let bg =
            |job: u64, r: usize| Ticket { class: JobOrigin::Background, ..net(job, r, Some("h")) };
        // Background arrives first, so only priority can reorder them.
        s.enqueue(bg(1, 1));
        s.enqueue(bg(2, 2));
        s.enqueue(net(3, 3, Some("h")));
        s.enqueue(net(4, 4, Some("h")));

        let ids: Vec<u64> = s.launchable().iter().map(|l| l.job.0).collect();
        assert_eq!(ids, vec![3, 4], "the user's jobs go first even though they queued last");
    }

    #[test]
    fn background_work_that_never_runs_is_correct_behaviour() {
        // No ageing, no starvation guarantee: if the user keeps the engine
        // busy, automatic fetching simply waits.
        let mut s = Scheduler::new(limits(1, 8));
        s.enqueue(Ticket { class: JobOrigin::Background, ..net(1, 1, Some("h")) });
        for round in 0..5 {
            s.enqueue(net(10 + round, 10 + round as usize, Some("h")));
            let launched = s.launchable();
            assert_eq!(launched.len(), 1);
            assert_ne!(launched[0].job, JobId(1), "the background job must keep waiting");
            let r = repo(10 + round as usize);
            drop(launched);
            s.finished(&r);
        }
        // ...and it does run once the user stops.
        assert_eq!(s.launchable()[0].job, JobId(1));
    }

    #[test]
    fn network_and_local_jobs_get_different_deadlines() {
        let mut s = Scheduler::new(Limits {
            network_timeout: Duration::from_secs(60),
            local_timeout: Duration::from_secs(5),
            ..Default::default()
        });
        s.enqueue(net(1, 1, Some("h")));
        s.enqueue(local(2, 2));
        let launched = s.launchable();
        let by_id = |id: u64| launched.iter().find(|l| l.job.0 == id).unwrap().timeout;
        assert_eq!(by_id(1), Duration::from_secs(60));
        assert_eq!(by_id(2), Duration::from_secs(5));
    }

    #[test]
    fn cancellation_drains_queued_jobs_from_both_kinds_of_queue() {
        let mut s = Scheduler::new(limits(1, 8));
        s.enqueue(net(1, 1, Some("h")));
        let running = s.launchable();
        assert_eq!(running.len(), 1);

        s.enqueue(net(2, 2, Some("h"))); // waiting on capacity
        s.enqueue(net(3, 1, Some("h"))); // waiting on repository 1
        assert_eq!(s.queued(), 2);

        let cancelled = s.drain_queued(|_| true);
        let mut ids: Vec<u64> = cancelled.iter().map(|t| t.job.0).collect();
        ids.sort();
        assert_eq!(ids, vec![2, 3], "both queues are drained; the running job is not");
        assert_eq!(s.queued(), 0);
        assert_eq!(s.in_flight(), 1);
        drop(running);
    }

    #[test]
    fn cancellation_can_be_selective() {
        let mut s = Scheduler::new(limits(0, 8));
        s.enqueue(net(1, 1, Some("h")));
        s.enqueue(Ticket { class: JobOrigin::Background, ..net(2, 2, Some("h")) });
        let removed = s.drain_queued(|t| t.class == JobOrigin::Background);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].job, JobId(2));
        assert_eq!(s.queued(), 1);
    }

    #[test]
    fn a_launch_names_its_repository_so_it_can_be_handed_back() {
        // Without this a caller that declines to run the job can never release it.
        let mut s = Scheduler::new(limits(4, 4));
        s.enqueue(net(1, 7, Some("h")));
        let launched = s.launchable();
        assert_eq!(launched[0].repo, repo(7));

        // Hand it back without running it.
        let handed_back = launched[0].repo.clone();
        drop(launched);
        s.finished(&handed_back);
        assert!(s.is_idle());
    }

    #[test]
    fn idle_means_nothing_queued_and_nothing_running() {
        let mut s = Scheduler::new(limits(4, 4));
        assert!(s.is_idle());
        s.enqueue(net(1, 1, Some("h")));
        assert!(!s.is_idle());
        let l = s.launchable();
        assert!(!s.is_idle());
        drop(l);
        s.finished(&repo(1));
        assert!(s.is_idle());
    }
}
