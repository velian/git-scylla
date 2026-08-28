//! `git-scylla` — CLI for the engine.

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
        /// A repository that exceeds this is reported, not treated as an error.
        #[arg(long, default_value_t = 2000)]
        timeout: u64,

        /// Concurrent probes. Defaults to the number of available cores.
        #[arg(long)]
        concurrency: Option<usize>,

        #[arg(long, value_enum, default_value_t = Sort::Badge)]
        sort: Sort,

        /// Selection expression, e.g. 'dirty & branch:main' or 'behind:>0'.
        #[arg(long, visible_alias = "filter")]
        select: Option<String>,

        /// Maximum directory depth to descend.
        #[arg(long)]
        max_depth: Option<usize>,
    },

    /// Fetch, updating remote-tracking refs without touching any worktree.
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
        #[arg(long, conflicts_with_all = ["dry_run", "yes", "prune", "tags"])]
        daemon: bool,

        /// Per-repository fetch interval in seconds, for `--daemon`.
        #[arg(long, requires = "daemon")]
        interval: Option<u64>,
    },

    /// Pull, updating the checked-out branch from its upstream.
    /// Requires a clean worktree in every mode.
    Pull {
        #[command(flatten)]
        common: BatchArgs,
        #[arg(long, value_enum, default_value_t = Mode::FfOnly)]
        mode: Mode,
    },

    /// Run an arbitrary git command in every selected repository.
    ///
    /// Everything after `--` is passed to `git` verbatim as argv, with no
    /// shell interpolation. Preconditions and undo do not apply.
    Run {
        #[command(flatten)]
        common: BatchArgs,

        /// Treat it as network work, so it takes the network semaphore.
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
    /// A failed pop leaves the stash entry in place. Conflicts are reported,
    /// never resolved.
    StashPop {
        #[command(flatten)]
        common: BatchArgs,
    },

    /// Check out a branch, tag or commit in every selected repository.
    ///
    /// Requires a clean worktree. Repositories without the ref are skipped
    /// and named.
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
        /// `git add -A`, not `git commit -a`: the latter stages tracked
        /// modifications only.
        #[arg(short = 'a', long)]
        all: bool,

        /// Skip the repository's own hooks.
        #[arg(long)]
        no_verify: bool,
    },

    /// Push, publishing local commits to their upstream.
    ///
    /// Worktree state is not checked. `--force` is not offered; use
    /// `--force-with-lease`.
    Push {
        #[command(flatten)]
        common: BatchArgs,

        /// Set the upstream to this remote, for branches that have none.
        #[arg(long, value_name = "REMOTE")]
        set_upstream: Option<String>,

        /// Refuse to overwrite anything that arrived since the last fetch.
        ///
        /// Requires a recent fetch and confirmation by typing the affected
        /// count.
        #[arg(long)]
        force_with_lease: bool,
    },

    /// Bring every selected repository's default branch up to date.
    ///
    /// Runs stash, checkout to the default branch, pull, checkout back, and
    /// stash pop as one unit. The checkout-back and pop run regardless of
    /// whether the pull succeeded.
    ///
    /// The default branch is resolved per repository from `origin/HEAD`,
    /// falling back to `main` then `master`.
    SyncDefault {
        #[command(flatten)]
        common: BatchArgs,

        /// How to pull once there. Defaults to fast-forward only.
        #[arg(long, value_enum, default_value_t = Mode::FfOnly)]
        mode: Mode,
    },

    /// Cut the next tag in a pre-release series, in every selected repository.
    ///
    /// The name is derived per repository from its own tags, so a working set
    /// at different versions gets different names. `--dry-run` lists every one
    /// before anything is created.
    ///
    /// The tag is published before it is created locally: a name the remote
    /// already has is refused before anything local exists.
    ///
    /// This does not fetch. Run `git-scylla fetch --tags` first if another tag
    /// may have been cut since the last fetch.
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
    /// With no argument, list that run's jobs. History is one run deep.
    Log {
        /// Job id, as printed by `fetch`/`pull`.
        job: Option<u64>,
    },
}

/// Roots and flags shared by every mutating verb.
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
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Concurrent jobs against any one remote host. Defaults to 3.
    #[arg(long)]
    pub per_host: Option<usize>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum Mode {
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

        // The scheduler picks its own prune/tags; ignore the flags here.
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
            // None: the engine derives the name per repository.
            let action = Action::DevTag {
                channel,
                bump: bump.into(),
                name: None,
                push: (!no_push).then_some(remote),
            };
            batch::run(action, common).await
        }

        Command::SyncDefault { common, mode } => {
            // None: the engine resolves the default branch per repository.
            batch::run(Action::SyncDefault { mode: mode.into(), plan: None }, common).await
        }

        Command::Pull { common, mode } => {
            batch::run(Action::Pull { mode: mode.into() }, common).await
        }

        Command::Log { job: Some(id) } => batch::print_log(id),
        Command::Log { job: None } => batch::list_jobs(),
    }
}
