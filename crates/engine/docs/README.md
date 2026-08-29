# git-scylla-engine

The action engine for git-scylla: policy, planning, scheduling, and job execution
for operating on many git repositories at once.

The CLI and the desktop shell are both thin surfaces over this crate. Neither
Tauri nor a terminal is visible from here.

## Position in the workspace

```mermaid
flowchart TD
    cli["apps/cli"]
    desktop["apps/desktop (Tauri)"]
    engine["git-scylla-engine"]
    core["git-scylla-core\ndomain types: Action, RepoSnapshot, Job..."]
    discovery["git-scylla-discovery\nwalks directories for repos"]
    probe["git-scylla-probe\nreads git status/refs"]
    exec["git-scylla-exec\nruns git subprocesses"]
    store["git-scylla-store\nstartup cache"]
    watch["git-scylla-watch\nfilesystem invalidation events"]

    cli --> engine
    desktop --> engine
    engine --> core
    engine --> discovery
    engine --> probe
    engine --> exec
    engine --> store
    engine --> watch
```

`engine` depends on every other domain crate. Nothing depends on `engine`.

## The actor

One `tokio::task` owns every mutable map: repository snapshots, jobs,
batches, scans, the scheduler, probe bookkeeping. A walker, a probe pool, a
job scheduler, a watcher, and a fetch tick all write into that state.

`EngineHandle` is the public interface. It sends `Cmd` over an `mpsc`
channel and exposes a `broadcast::Receiver<Event>`. Both are cheap to clone;
the CLI and every GUI window hold their own handle.

`Engine::start` spawns the actor and returns a handle. `Engine::shutdown`
stops it and waits for in-flight jobs to finish.

### Task boundaries

The actor loop itself never blocks. Anything that touches disk or a
subprocess runs on a separate task and reports back through one internal
`mpsc` channel, which the actor loop drains alongside `Cmd`.

`Actor::on_cmd` is a synchronous function, and that is how the claim is kept
rather than merely stated: a command either answers from state the actor
already holds or spawns a task. Adding an `await` to it means making it
`async` again, which is the moment to spawn instead.

```mermaid
flowchart LR
    subgraph actor["actor task (single, owns all state)"]
        loop["run() loop:\ncmd_rx, internal_rx, timers"]
    end

    subgraph spawned["spawned per operation"]
        walk["walk (blocking thread)"]
        probeT["probe (async, via Arc<dyn Probe>)"]
        job["run_job (async, via git-scylla-exec)"]
    end

    handle["EngineHandle::send/ask"] -->|Cmd| loop
    loop -->|spawns| walk
    loop -->|spawns| probeT
    loop -->|spawns| job
    walk -->|Internal::Found / WalkFinished| loop
    probeT -->|Internal::Probed| loop
    job -->|Internal::JobFinished| loop
    loop -->|Event| bus["broadcast::Sender<Event>"]
```

Cancelling a scan sets an `AtomicBool` the walker checks. Cancelling a batch
cancels a `CancellationToken` that every job in it shares. Neither requires
the actor loop to wait.

### The I/O seams

Reading a repository's live state — a `git status`, a ref, a tag list —
always goes through `Arc<dyn Probe>`. The actor never opens a `.git`
directory itself. Tests substitute a fake `Probe` and drive planning and
scheduling with no filesystem at all.

Finding repositories on disk is a separate concern, `git-scylla-discovery`'s
`Walker`. Running a job's git commands is a third, `git-scylla-exec`'s
subprocess wrapper, used by `runner`.

## Modules

| Module | Role |
|---|---|
| [`engine`](../src/engine.rs) | The actor: `Cmd` in, `Event` out, owns all mutable state |
| [`plan`](../src/plan.rs) | Turns an `Action` template, a `Selection`, and a set of snapshots into a `PlanTemplate`; `resolve` finishes one into a `Plan`; also `undo` |
| [`policy`](../src/policy.rs) | Pure eligibility rules and fetch backoff/quarantine — no I/O, no clock |
| [`sched`](../src/sched.rs) | Admits queued jobs under global, per-host, and per-repo concurrency limits |
| [`probe_traffic`](../src/probe_traffic.rs) | Decides which repositories are owed a probe, and debounces watcher-triggered ones |
| [`runner`](../src/runner.rs) | Executes one job: an `Action`'s steps as git subprocesses, then reverse-order cleanup |
| [`selection`](../src/selection.rs) | `Selection` — which repositories an action targets |

`policy` and `sched` and `probe_traffic` hold state or logic with no I/O and
no clock of their own; every input, including time, is a function argument.
That is what makes them exhaustively testable by stating a situation rather
than producing one.

## Planning a batch

A `Plan` is a pure description of what a batch would do. It is computed
without running anything, rendered identically by the CLI and the GUI, and
confirmed before any job starts.

```mermaid
flowchart LR
    action["Action template\n+ Selection\n+ snapshots"]
    filter["filter by Selection"]
    eval["policy::evaluate\nper snapshot"]
    elig["Eligible"]
    skip["Skip(reason)"]
    resolve["resolve()\nper-repo templating"]
    plan["Plan\neligible / skipped / warning"]
    view["Plan::view()"]
    pv["PlanView\n(strings only)"]

    regate["policy::evaluate\nper resolved action"]

    action --> filter --> eval
    eval -->|eligible| elig --> resolve --> regate
    regate -->|eligible| plan
    regate -->|ineligible| skip
    eval -->|ineligible| skip --> plan
    plan --> view --> pv
```

`policy::evaluate` is the one gate every action passes through. Most actions
are blocked by a stale snapshot, a bare repository, an operation already in
progress, a detached HEAD, or a dirty worktree; `Fetch` is exempt from all
but the first two, since it never touches the worktree.

It runs twice: on the template, so that a repository heading for a skip is
never asked a ref question, and again on the resolved action, because
`SyncDefault`'s last rule — already on the trunk, and dirty — needs a trunk
name only resolving can supply. `PlanTemplate` carries the `now` and `policy`
of the first pass into the second, so the two judge alike and the second can
only differ where a resolved fact was missing the first time. Every other
variant's arm reads the snapshot and fields resolving does not touch, which is
what makes the repeat free of surprises rather than merely cheap.

Planning is two steps, and only the gap between them touches disk.

`plan` decides everything a snapshot can decide, filling in a push remote, a
commit message template, a branch name. What it cannot answer it leaves alone
and returns a `PlanTemplate`, which is a separate type precisely so a plan
still full of templates cannot be mistaken for one that runs.

`resolve` finishes the job, given the answers as an argument. Three questions
need more than a snapshot — whether a ref exists, a repository's default
branch, and the next dev tag name — and the actor's only remaining part in
planning is to ask them: `queries_for` says what to ask and of whom, one
`Probe::refs` call per distinct question, read once per plan rather than once
per row.

The asking happens on a spawned task. `ref_requests` builds the requests from
actor state synchronously — the only half that touches the actor's maps — and
everything after that is self-contained, so the reads, the resolution and the
reply all happen off the actor task. A plan that takes a second to resolve
costs the engine no responsiveness at all. A row whose question went unanswered is skipped as `SnapshotStale`;
a row that needed no cold fact never consults the answers at all.

The grouping follows each row's *own* action, not the template's. `checkout
main` is one question asked of every repository; `checkout release/{repo}`
renders differently per repository, and `undo` returns each repository to its
own branch, so both ask once per distinct ref.

`PlanView` carries only display data: no `Action`, no `SkipReason`. The CLI's
text renderer and the GUI's plan sheet both consume `PlanView`, so they
cannot present two different plans for the same input.

`undo` mirrors `plan`: it derives a repair action per finished job (a
checkout back to `branch_before`, or a reset to `head_before`), skips a job
whose repair is undefined or whose HEAD has moved since, then runs the same
`evaluate` gate.

## Scheduling and running jobs

A batch produces one `Job` per eligible repository. Each job becomes a
`Ticket` in the scheduler, runs through `runner::run_job`, and settles back
into the actor, which releases its resources and asks for a fresh probe.

```mermaid
flowchart TD
    ticket["Ticket\n(job, repo, host, class, network)"]
    busy{"repo busy?"}
    perrepo["per_repo queue\n(waits for that repo alone)"]
    ready["ready[class] queue\n(User before Background)"]
    acquire["acquire:\nglobal permit, then host permit"]
    launch["Launch\n(permits held)"]
    run["runner::run_job\nforward steps, then reverse cleanup"]
    settle["job_settled\nrelease permits, request re-probe"]

    ticket --> busy
    busy -->|yes| perrepo
    busy -->|no| ready --> acquire --> launch --> run --> settle
    settle -->|repo free| perrepo -.->|promoted| ready
```

Three limits apply at once: one repository runs one job at a time, a global
semaphore caps network and local concurrency separately, and a per-host
semaphore caps concurrent network jobs to the same remote. Permits are
acquired global-first, host-second, and the global permit is released
immediately if the host is saturated — the fixed order rules out deadlock
between the two.

`runner::run_job` turns an `Action` into an ordered list of steps and runs
them one at a time. On the first failure, remaining steps are marked
`NotRun` rather than attempted. A cleanup pass then runs in reverse over
whatever completed — a `stash pop` for a `stash push`, a checkout back to the
original branch — regardless of whether the forward pass succeeded, and is
not cancellable.

`probe_traffic` tracks, per repository, whether a probe is owed and why:
`Definite` (the actor knows something changed — a job finished, a command
asked) always proceeds; `Observed` (a filesystem watcher event) is debounced
to at most one probe per repository per interval. A repository already
running a job is never re-probed underneath it.

## Fetch scheduling

Background fetches are paced by `FetchHealth`, carried on each
`RepoSnapshot` and advanced by pure functions in `policy`.

```mermaid
stateDiagram-v2
    [*] --> Due
    Due --> Due: attempt ok
    Due --> BackingOff: attempt failed
    BackingOff --> BackingOff: attempt failed, below threshold
    BackingOff --> Due: attempt ok
    BackingOff --> Quarantined: failures >= quarantine_after
    Quarantined --> Due: user fetch (manual_attempt)
```

`due` selects repositories whose schedule has come round and that still pass
`policy::evaluate` for `Fetch`. `after_attempt` advances the schedule after
a background attempt. `manual_attempt` resets backoff and quarantine before
applying the same transition, so a fetch the user asked for always starts a
clean cycle regardless of the repository's prior failures.

The fetch scheduler is gated on the first scan having settled: nothing
fetches until the initial walk of the working set is done.

## Testing

`policy`, `plan`, and `sched` take every input as an argument, including
time, so their tests state a scenario directly rather than building one. The
engine as a whole is tested against a fake `Probe` (the `testkit` feature on
`git-scylla-probe` and `git-scylla-core`), so batch execution, scheduling,
and plan resolution are exercised without real git repositories on disk.
