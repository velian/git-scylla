//! `git-scylla` — a supported surface, not a debugging aid.
//!
//! Every engine feature lands here first, because logic that can only be
//! exercised through a webview cannot be tested in CI.

mod batch;
mod common;
mod daemon;
mod progress;
mod render;
mod scan;
mod store;

use clap::{Parser, Subcommand, ValueEnum};
use git_scylla_core::{version::Bump, Action, PullMode};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "git-scylla",
    about = "Operate on many git repositories at once",
    version,
    after_long_help = render::LEGEND
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Walk one or more roots and report the state of every repository found.
    ///
    /// Strictly read-only, and it never touches the network: `behind` is
    /// computed from the locally cached remote-tracking refs, so the FETCH
    /// column is how stale that number is.
    Scan {
        /// Directories to search.
        #[arg(required = true)]
        roots: Vec<PathBuf>,

        /// Emit `Vec<RepoSnapshot>` as JSON instead of a table.
        #[arg(long)]
        json: bool,

        /// Descend into repositories to find nested ones.
        #[arg(long)]
        nested: bool,

        /// Per-repository probe deadline, in milliseconds.
        ///
        /// One repository on a slow or network volume must not stall the rest,
        /// so exceeding this is an ordinary outcome and not an error.
        #[arg(long, default_value_t = 2000)]
        timeout: u64,

        /// Concurrent probes. Defaults to the number of available cores.
        #[arg(long)]
        concurrency: Option<usize>,

        #[arg(long, value_enum, default_value_t = Sort::Badge)]
        sort: Sort,

        /// Selection expression, e.g. 'dirty & branch:main' or 'behind:>0'.
        ///
        /// `--filter` is kept as an alias: it was named that before the
        /// mutating verbs settled on `--select`, and one grammar with two names
        /// is confusing enough without breaking the older one.
        #[arg(long, visible_alias = "filter")]
        select: Option<String>,

        /// Maximum directory depth to descend.
        #[arg(long)]
        max_depth: Option<usize>,
    },

    /// Fetch, updating remote-tracking refs without touching any worktree.
    ///
    /// The one action that cannot alter a worktree or local history, which is
    /// why it is the only one run automatically.
    Fetch {
        #[command(flatten)]
        common: BatchArgs,
        /// Delete remote-tracking refs whose remote branch is gone.
        #[arg(long)]
        prune: bool,
        /// Fetch tags as well.
        #[arg(long)]
        tags: bool,

        /// Run the fetch scheduler in the foreground, logging every decision.
        ///
        /// This is where the fetch policy actually gets debugged: the decisions
        /// are one line each, they matter more than the outcomes, and a GUI is
        /// a poor place to watch a fifteen-minute cycle. Ctrl-C stops it and
        /// lets in-flight fetches finish.
        #[arg(long, conflicts_with_all = ["dry_run", "yes", "prune", "tags"])]
        daemon: bool,

        /// Per-repository fetch interval in seconds, for `--daemon`.
        #[arg(long, requires = "daemon")]
        interval: Option<u64>,
    },

    /// Pull, updating the checked-out branch from its upstream.
    ///
    /// Requires a clean worktree in every mode. There is no autostash: a
    /// silently stashed change is a change the user did not offer up.
    Pull {
        #[command(flatten)]
        common: BatchArgs,
        #[arg(long, value_enum, default_value_t = Mode::FfOnly)]
        mode: Mode,
    },

    /// Run an arbitrary git command in every selected repository.
    ///
    /// The deliberate escape hatch. An argv, never a shell string:
    /// everything after `--` is passed to `git` verbatim, with no shell and no
    /// interpolation. Preconditions and undo do not apply, and the confirmation
    /// says so.
    Run {
        #[command(flatten)]
        common: BatchArgs,

        /// Treat it as network work, so it takes the network semaphore.
        ///
        /// The default, because being wrong this way costs throughput and being
        /// wrong the other way invites rate limiting.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        network: bool,

        /// Record `head_before`, so a transcript says where the repository was.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        mutating: bool,

        /// The git arguments, after `--`.
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },

    /// Stash uncommitted work in every selected repository.
    Stash {
        #[command(flatten)]
        common: BatchArgs,

        /// Include untracked files.
        #[arg(short = 'u', long)]
        include_untracked: bool,
    },

    /// Pop the most recent stash in every selected repository.
    ///
    /// A pop that cannot apply leaves the stash entry alone — that is git's own
    /// behaviour and the tool does not second-guess it. Conflicts are reported,
    /// never resolved.
    StashPop {
        #[command(flatten)]
        common: BatchArgs,
    },

    /// Check out a branch, tag or commit in every selected repository.
    ///
    /// Requires a clean worktree: bulk checkout is genuinely useful — "put
    /// every repository back on main" — and genuinely dangerous on dirty trees.
    /// Repositories without the ref are skipped and named.
    Checkout {
        #[command(flatten)]
        common: BatchArgs,

        /// What to check out. May use `{repo}`, `{branch}` and `{date}`.
        #[arg(short = 'r', long)]
        rev: String,

        /// Create it rather than requiring it to exist.
        #[arg(short = 'b', long)]
        create: bool,
    },

    /// Create a branch in every selected repository, without switching to it.
    Branch {
        #[command(flatten)]
        common: BatchArgs,

        /// The branch name. May use `{repo}`, `{branch}` and `{date}`.
        #[arg(short = 'n', long)]
        name: String,

        /// Where to start it. Defaults to the current HEAD.
        #[arg(long)]
        from: Option<String>,
    },

    /// Commit, creating history in every selected repository.
    ///
    /// The message may use `{repo}`, `{branch}` and `{date}`, and is rendered
    /// per repository — `--dry-run` shows every rendered message before
    /// anything runs.
    Commit {
        #[command(flatten)]
        common: BatchArgs,

        /// The message, or a template.
        #[arg(short = 'm', long)]
        message: String,

        /// Stage everything first, **including untracked files**.
        ///
        /// `git add -A`, not `git commit -a`: the second stages tracked
        /// modifications only. The plan says how many untracked files this
        /// would sweep up.
        #[arg(short = 'a', long)]
        all: bool,

        /// Skip the repository's own hooks.
        ///
        /// Off by default: a `pre-commit` that reformats, or refuses a secret,
        /// is doing the job it was installed for.
        #[arg(long)]
        no_verify: bool,
    },

    /// Push, publishing local commits to their upstream.
    ///
    /// Worktree state is irrelevant to a push and is deliberately not checked.
    /// `--force` does not exist and never will: a bulk tool that can force-push
    /// across forty repositories is one that will eventually do so by accident.
    Push {
        #[command(flatten)]
        common: BatchArgs,

        /// Set the upstream to this remote, for branches that have none.
        #[arg(long, value_name = "REMOTE")]
        set_upstream: Option<String>,

        /// Refuse to overwrite anything that arrived since the last fetch.
        ///
        /// The safe half of a force push, and the only half offered. Requires a
        /// recent fetch — a lease against a stale remote-tracking ref is not a
        /// lease — and a confirmation that cannot be given without reading the
        /// plan.
        #[arg(long)]
        force_with_lease: bool,
    },

    /// Bring every selected repository's default branch up to date.
    ///
    /// Stash, switch to `main`/`master`, pull, switch back, pop — five git
    /// invocations that behave as one, so an interrupted batch cannot leave
    /// forty working sets stashed and parked on the wrong branch. The switch
    /// back and the pop run whether the pull succeeded or not.
    ///
    /// The branch is resolved per repository from `origin/HEAD`, falling back
    /// to `main` then `master`; `--dry-run` lists the exact commands, which is
    /// where a repository that calls its trunk something else shows up.
    SyncDefault {
        #[command(flatten)]
        common: BatchArgs,

        /// How to pull once there.
        ///
        /// The default refuses to merge or rebase, which for a branch the user
        /// is not standing on is what they almost always want: a default branch
        /// that cannot fast-forward has local commits on it, and quietly
        /// reconciling those in bulk is not a thing to do without being asked.
        #[arg(long, value_enum, default_value_t = Mode::FfOnly)]
        mode: Mode,
    },

    /// Cut the next tag in a pre-release series, in every selected repository.
    ///
    /// The name is derived per repository from *its* tags — the newest release
    /// decides where the next series starts, an existing series carries on —
    /// so a working set at different versions gets different names, and
    /// `--dry-run` lists every one of them before anything is created.
    ///
    /// The tag is published **before** it is created locally, which is the
    /// reverse of what you would type by hand and is deliberate: a name the
    /// remote already has is refused, and refusing must leave nothing behind.
    ///
    /// Nothing here fetches. The derivation reads local tags, and the automatic
    /// fetch does not include them, so run `git-scylla fetch --tags` first if
    /// somebody else may have cut a tag since.
    DevTag {
        #[command(flatten)]
        common: BatchArgs,

        /// The series name: `dev`, `rc`, `alpha`.
        #[arg(long, default_value = "dev")]
        channel: String,

        /// Where a *new* series starts, from the newest release.
        ///
        /// Ignored when a series is already under way at a higher version:
        /// resetting the counter would derive a name that already exists.
        #[arg(long, value_enum, default_value_t = BumpArg::Minor)]
        bump: BumpArg,

        /// Publish to this remote.
        #[arg(long, value_name = "REMOTE", default_value = "origin")]
        remote: String,

        /// Create the tag locally and do not publish it.
        #[arg(long, conflicts_with = "remote")]
        no_push: bool,
    },

    /// Report fetch health per repository.
    ///
    /// Answering "why does this say 3 behind" from a terminal must not require
    /// the application.
    Status {
        #[arg(required = true)]
        roots: Vec<PathBuf>,

        /// Selection expression, e.g. 'behind:>0'.
        #[arg(long)]
        select: Option<String>,

        /// Only repositories the scheduler is unhappy with — backing off or
        /// quarantined.
        #[arg(long)]
        stale_only: bool,

        #[arg(long)]
        json: bool,

        /// Descend into repositories to find nested ones.
        #[arg(long)]
        nested: bool,
    },

    /// Print the full transcript of a job from the last run.
    ///
    /// With no argument, list that run's jobs. History is one run deep: this
    /// answers "the batch just finished, what happened to number 37".
    Log {
        /// Job id, as printed by `fetch`/`pull`.
        job: Option<u64>,
    },
}

/// Roots and flags shared by every mutating verb.
///
/// `roots` lives here rather than on each variant: it was declared identically
/// nine times, and a positional that drifts in one of them is a positional
/// nobody notices has drifted.
#[derive(clap::Args)]
pub struct BatchArgs {
    /// Directories to search.
    #[arg(required = true)]
    pub roots: Vec<PathBuf>,

    /// Selection expression, e.g. 'behind:>0 & !dirty'. Defaults to everything.
    #[arg(long)]
    pub select: Option<String>,

    /// Print the plan and exit without running anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Emit the batch result as JSON on stdout.
    #[arg(long)]
    pub json: bool,

    /// Descend into repositories to find nested ones.
    #[arg(long)]
    pub nested: bool,

    /// Concurrent network jobs. Defaults to 8.
    ///
    /// Present on the mutating verbs and not only on `scan`: a limit no
    /// surface can set is not configurable.
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Concurrent jobs against any one remote host. Defaults to 3.
    #[arg(long)]
    pub per_host: Option<usize>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Refuse anything that is not a fast-forward. The safe default.
    FfOnly,
    Rebase,
    Merge,
}

/// [`git_scylla_core::version::Bump`], as a CLI flag.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum BumpArg {
    Major,
    Minor,
    Patch,
}

impl From<BumpArg> for Bump {
    fn from(b: BumpArg) -> Self {
        match b {
            BumpArg::Major => Bump::Major,
            BumpArg::Minor => Bump::Minor,
            BumpArg::Patch => Bump::Patch,
        }
    }
}

impl From<Mode> for PullMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::FfOnly => PullMode::FfOnly,
            Mode::Rebase => PullMode::Rebase,
            Mode::Merge => PullMode::Merge,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum Sort {
    /// Worst first, so problems surface at the top.
    Badge,
    Path,
    Branch,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // stderr, never stdout: `--json | jq` must not have log lines in it.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Scan { roots, json, nested, timeout, concurrency, sort, select, max_depth } => {
            scan::run(scan::Args {
                roots,
                json,
                nested,
                timeout_ms: timeout,
                concurrency,
                sort,
                filter: select,
                max_depth,
            })
            .await
        }

        // `--daemon` conflicts with `--prune`/`--tags`: the scheduler picks
        // its own action (`prune: true, tags: false`), and letting the flag
        // through would suggest it does not.
        Command::Fetch { common, daemon: true, interval, .. } => {
            daemon::run(daemon::DaemonArgs {
                roots: common.roots,
                nested: common.nested,
                interval,
                concurrency: common.concurrency,
                per_host: common.per_host,
            })
            .await
        }

        Command::Fetch { common, prune, tags, .. } => {
            batch::run(Action::Fetch { prune, tags }, common).await
        }

        Command::Run { common, network, mutating, args } => {
            batch::run(Action::Custom { args, network, mutating }, common).await
        }

        Command::Stash { common, include_untracked } => {
            batch::run(Action::Stash { include_untracked }, common).await
        }

        Command::StashPop { common } => batch::run(Action::StashPop, common).await,

        Command::Checkout { common, rev, create } => {
            batch::run(Action::Checkout { rev, create }, common).await
        }

        Command::Branch { common, name, from } => {
            batch::run(Action::Branch { name, from }, common).await
        }

        Command::Commit { common, message, all, no_verify } => {
            batch::run(Action::Commit { message, stage_all: all, no_verify }, common).await
        }

        Command::Push { common, set_upstream, force_with_lease } => {
            batch::run(Action::Push { set_upstream, force_with_lease }, common).await
        }

        Command::Status { roots, select, stale_only, json, nested } => {
            daemon::status(daemon::StatusArgs { roots, select, stale_only, json, nested }).await
        }

        Command::DevTag { common, channel, bump, remote, no_push } => {
            // `name: None` — the template. The engine derives one per
            // repository from that repository's own tags.
            let action = Action::DevTag {
                channel,
                bump: bump.into(),
                name: None,
                push: (!no_push).then_some(remote),
            };
            batch::run(action, common).await
        }

        Command::SyncDefault { common, mode } => {
            // `plan: None` — the template. The engine resolves one of these per
            // repository, because which branch is the default is a fact about
            // `refs/` and not about a snapshot.
            batch::run(Action::SyncDefault { mode: mode.into(), plan: None }, common).await
        }

        Command::Pull { common, mode } => {
            batch::run(Action::Pull { mode: mode.into() }, common).await
        }

        Command::Log { job: Some(id) } => batch::print_log(id),
        Command::Log { job: None } => batch::list_jobs(),
    }
}
