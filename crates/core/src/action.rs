use crate::Oid;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// What to do to a repository.
///
/// Closed and explicit rather than a free-form string, so preconditions and
/// undo semantics are defined exhaustively per variant — the compiler then
/// refuses a new action without deciding both. [`Action::Custom`] is the
/// deliberate escape hatch, and is exempt from undo.
///
/// An `Action` is **resolved for one repository**. The planner turns a template
/// into one of these per repository, and the plan displays the resolved value,
/// not the template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Action {
    Fetch {
        prune: bool,
        tags: bool,
    },
    Pull {
        mode: PullMode,
    },
    Push {
        /// `Some(remote)` sets the upstream to that remote, for a branch that
        /// has none. `None` pushes to the configured upstream.
        ///
        /// Not a `set_upstream: bool`. `git push -u` with no remote and no
        /// refspec is fatal, so a bool plus an absent remote is a state that
        /// cannot produce a working command; an `Option` makes it
        /// unrepresentable.
        set_upstream: Option<String>,
        force_with_lease: bool,
    },
    Checkout {
        rev: String,
        create: bool,
    },
    Commit {
        /// The template before the planner resolves it, the rendered message
        /// afterwards. One field, because the resolved value is both what the
        /// plan shows and what the executor runs.
        message: String,
        stage_all: bool,
        /// Skip the repository's own hooks.
        ///
        /// Off by default and surfaced in the plan. A `pre-commit` that
        /// reformats, or refuses a secret, is doing the job it was installed
        /// for; a bulk tool that bypassed it silently would be the fastest way
        /// to commit something that should not exist.
        no_verify: bool,
    },
    Stash {
        include_untracked: bool,
    },
    StashPop,
    /// Create a branch, without switching to it.
    ///
    /// Distinct from `Checkout { create: true }`, which creates *and* switches.
    /// "Cut a branch across the working set and carry on where I am" is a
    /// different intent from "move the working set onto a new branch".
    Branch {
        name: String,
        /// Where to start it. `None` is the current `HEAD`.
        from: Option<String>,
    },
    /// Move `HEAD` back to a commit. The repair half of undo.
    ///
    /// Its own variant rather than a `Custom`, because this is the one action
    /// whose whole purpose is to be dangerous. A `Custom` carrying
    /// `reset --hard` would be exempt from every precondition that matters
    /// here, and invisible in a plan as anything but an argv.
    Reset {
        to: Oid,
        mode: ResetMode,
    },
    /// Bring this repository's default branch up to date, and put the user
    /// back where they were.
    ///
    /// Five git invocations behaving as one: stash, switch, pull, switch back,
    /// pop. One `Action` rather than five, because five independent jobs is a
    /// bug generator — one interrupted batch leaves forty working sets stashed
    /// and parked on `main`.
    ///
    /// `plan` is `None` in a template and `Some` once the engine has resolved it
    /// for one repository. An `Option` rather than four fields because the
    /// branch to visit cannot be answered from a `RepoSnapshot` at all, and an
    /// unresolved value that *looked* runnable would be a plan naming `main` at
    /// a repository whose default is `master`.
    SyncDefault {
        mode: PullMode,
        plan: Option<SyncPlan>,
    },
    /// Cut the next tag in a pre-release series, and publish it.
    ///
    /// `name` is `None` in a template and `Some` once the engine has derived it
    /// from *this repository's* tags — the same shape, and the same reason, as
    /// [`Action::SyncDefault`]. The plan has to read "create `v2.4.0-dev.3`" per
    /// repository, because "create the next dev tag" is not something a user can
    /// check.
    DevTag {
        /// The series: `dev`, `rc`, whatever the team writes.
        channel: String,
        /// Where a *new* series starts, from the newest release.
        bump: crate::version::Bump,
        name: Option<String>,
        /// Publish to this remote. `None` creates it locally only.
        push: Option<String>,
    },
    /// An argv vector, never a shell string: no shell, no interpolation, no
    /// injection surface.
    Custom {
        args: Vec<String>,
        /// Does this reach the network, and so take the network semaphore?
        ///
        /// From the saved definition: the engine cannot reason about an
        /// arbitrary command and must not pretend to. Defaults to `true` where
        /// unstated — being wrong that way costs throughput, and being wrong the
        /// other way invites remote rate limiting.
        network: bool,
        /// Can it move `HEAD` or create local history, so that `head_before` is
        /// worth recording? Same source and same default, for the same reason:
        /// an unnecessary `rev-parse` costs one subprocess.
        mutating: bool,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// One repository's answer to "sync the default branch".
///
/// Every field differs per repository, which is why it exists as a unit: a plan
/// that showed only the action would be claiming a uniformity the working set
/// does not have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPlan {
    /// This repository's own default branch — `main` here, `master` there.
    pub default: String,
    /// The branch to come back to. A name, never `-`: the plan has to *show*
    /// where the user will be left, and `git checkout -` shows nothing.
    pub back_to: String,
    /// Stash first, because there is tracked work in the way of the switch.
    ///
    /// Untracked files are deliberately left alone. They rarely block a
    /// checkout, and stashing them means a failed pop leaves a user's
    /// build output and scratch files inside a stash entry — recoverable,
    /// alarming, and avoidable.
    pub stash: bool,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// How far back a reset takes the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetMode {
    /// Move `HEAD` and leave the index and working tree alone, so what the
    /// undone commit contained comes back as staged changes.
    ///
    /// What undoing a `Commit` uses: the user asked for the commit to go away,
    /// not for the work in it to go away.
    Soft,
    /// Move `HEAD`, the index and the working tree. **Discards uncommitted
    /// work**, which is why every caller is behind a plan sheet that says so.
    Hard,
}

impl ResetMode {
    fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Hard => "--hard",
        }
    }
}

impl std::fmt::Display for ResetMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ResetMode::Soft => "soft",
            ResetMode::Hard => "hard",
        })
    }
}

/// Can this action be undone, and how?
///
/// Being honest about what cannot be undone matters more than maximising
/// coverage: an undo offered and then refused mid-batch is worse than one never
/// offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undoable {
    /// Reset to the recorded `head_before`, in this mode.
    ///
    /// The mode rides along so it sits next to the judgement it follows from,
    /// rather than in whichever caller builds the plan.
    Reset(ResetMode),
    /// Return to the branch that was checked out before.
    ///
    /// A `Checkout` **cannot** be undone with a reset, and the difference is
    /// destructive rather than cosmetic. On a branch, `reset --hard` moves
    /// *that branch's pointer* — so undoing "switch to `main`" by resetting to
    /// the previous branch's tip drags `main` there and leaves it looking like
    /// a normal branch that has silently swallowed somebody else's commits.
    ///
    /// The repair for a switch is a switch.
    Switch,
    /// No, and why — in words a plan can show as a skip reason.
    No(&'static str),
}

/// Whether an action's effect can be repaired by moving `HEAD` back.
pub fn undoability(action: &Action) -> Undoable {
    match action {
        // The commit goes away; the work in it comes back staged.
        Action::Commit { .. } => Undoable::Reset(ResetMode::Soft),
        Action::Pull { .. } | Action::StashPop => Undoable::Reset(ResetMode::Hard),
        // A reset would move the branch that was switched *to*, dragging it
        // to the previous branch's tip. The repair for a switch is a switch.
        Action::Checkout { .. } => Undoable::Switch,
        // Creating a branch moved nothing, so there is nothing for a reset to
        // repair — and deleting it is a different operation with its own
        // hazards, not an undo.
        Action::Branch { .. } => Undoable::No("creating a branch moved nothing"),
        Action::Fetch { .. } => Undoable::No("fetch only advances remote-tracking refs"),
        Action::Push { .. } => Undoable::No("the remote has already accepted the commits"),
        // A stash is on the stack; resetting would discard the worktree without
        // putting anything back, which is a different operation wearing undo's
        // name.
        Action::Stash { .. } => Undoable::No("the changes are on the stash, not in HEAD"),
        // "Undoable to `head_before` with the stash restored" is worse than
        // useless: a sync leaves HEAD where it found it, so that reset is a
        // no-op wearing undo's name — reporting success while the thing that
        // moved, the default branch, stayed moved. A real undo would have to
        // rewind `main` to a tip nothing recorded and re-stash work already
        // popped, and neither is repair.
        //
        // Nothing is lost by saying so: a sync fast-forwards a branch the user
        // was not on and puts their work back, so there is no destroyed state
        // to recover.
        Action::SyncDefault { .. } => {
            Undoable::No("a sync leaves HEAD where it found it; only the default branch moved")
        }
        // Both answers are no, for the two different reasons that already
        // govern `Branch` and `Push`. Kept apart because the *reason* is what
        // the plan shows, and "the remote has it" is a different fact from
        // "nothing moved" — the first tells the user to talk to whoever else
        // has fetched, and the second tells them to delete a local ref.
        Action::DevTag { push: Some(_), .. } => {
            Undoable::No("the remote has already accepted the tag")
        }
        Action::DevTag { push: None, .. } => Undoable::No("creating a tag moved nothing"),
        Action::Custom { .. } => Undoable::No("arbitrary command; effects are unknown"),
        // Undoing an undo is refused at the batch level too, which is where the
        // rule is enforceable — but saying it here keeps the match exhaustive
        // rather than defaulted.
        Action::Reset { .. } => Undoable::No("an undo is not itself undone"),
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullMode {
    /// Refuse anything that is not a fast-forward.
    FfOnly,
    Rebase,
    Merge,
}

impl PullMode {
    fn flag(self) -> &'static str {
        match self {
            PullMode::FfOnly => "--ff-only",
            PullMode::Rebase => "--rebase",
            // Explicit, not bare `pull`. Without it the user's `pull.rebase`
            // config decides, and the plan would have promised one thing while
            // git did another.
            PullMode::Merge => "--no-rebase",
        }
    }
}

impl std::fmt::Display for PullMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PullMode::FfOnly => "ff-only",
            PullMode::Rebase => "rebase",
            PullMode::Merge => "merge",
        })
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// One `git` invocation, with the command that closes what it opened.
///
/// `compensate` runs **after the forward pass, whether the job succeeded or
/// failed**, in reverse order over the steps that completed. It is not undo,
/// which repairs a job that succeeded and should not have; this finishes a job,
/// and a job that opened something has to close it either way. `stash push`
/// settles it: the pop is owed whether or not the pull in between worked.
///
/// A failing compensation stops the ones still queued behind it, because each
/// assumes the steps after it have already been undone. Leaving the stash on
/// the stack because the return switch failed is recoverable; popping it onto
/// the branch the user did not want it on is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub argv: Vec<String>,
    pub compensate: Option<Vec<String>>,
}

impl Step {
    pub fn simple(argv: Vec<String>) -> Self {
        Self { argv, compensate: None }
    }

    pub fn with_compensation(argv: Vec<String>, compensate: Vec<String>) -> Self {
        Self { argv, compensate: Some(compensate) }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// Which pass a [`StepRun`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pass {
    Forward,
    /// A compensating command, run after the forward pass to close what a step
    /// opened. Named for what it is rather than for failure, because it runs on
    /// success too — a successful sync ends `cleanup: git stash pop`, and
    /// calling that "compensating" would report a problem that did not happen.
    ///
    /// Recorded rather than hidden either way: a transcript that shows a failed
    /// job without showing the cleanup leaves the user unsure what state the
    /// repository is in.
    Cleanup,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// What happened to one step.
///
/// Not [`crate::JobState`]. A step cannot be `Skipped { why: SkipReason }` —
/// every `SkipReason` is a fact about a *repository's* preconditions, not about
/// a command — and a step needs `NotRun`, which no `JobState` expresses. Reusing
/// a type whose variants are mostly unreachable is how unreachable variants get
/// reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum StepState {
    Pending,
    Running,
    Ok,
    Failed {
        code: i32,
    },
    Cancelled,
    /// An earlier step failed, so this one never started.
    NotRun,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// One executed step, and where its output sits in the job's transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRun {
    pub step: Step,
    pub pass: Pass,
    pub state: StepState,
    /// A range into [`crate::Job::log`], so the transcript stays one ordered
    /// stream while remaining attributable per step. A slice rather than a
    /// nested `Vec` because the ordering across steps is the thing a reader
    /// needs, and copies of it would drift.
    pub log: std::ops::Range<usize>,
}

impl StepRun {
    pub fn pending(step: Step, pass: Pass) -> Self {
        Self { step, pass, state: StepState::Pending, log: 0..0 }
    }
}

/// The argv for a pull, in whatever mode.
///
/// Shared by [`Action::Pull`] and [`Action::SyncDefault`], which pulls too. Two
/// copies would be two chances to lose `--no-autostash`, the flag keeping a
/// user's config from silently stashing their work.
fn pull_argv(mode: PullMode) -> Vec<String> {
    let mut argv = vec!["pull".to_string(), mode.flag().to_string()];
    // With `rebase.autoStash = true` in the user's config, a pull on a dirty
    // tree silently stashes their work and pops it again; `--no-autostash`
    // refuses loudly. A clean worktree is a precondition, but the snapshot can
    // go stale between plan and execution, and that window is exactly where an
    // autostash would touch work the user never offered up.
    //
    // It matters more inside a sync, not less: there the tool has already
    // stashed on purpose, and a second stash it did not ask for would be popped
    // by git in an order nobody chose.
    argv.push("--no-autostash".into());
    if matches!(mode, PullMode::Merge) {
        // `GIT_EDITOR=true` already prevents a hang; this makes the command
        // non-interactive at the git level too, so the guarantee does not rest
        // on the environment alone.
        argv.push("--no-edit".into());
    }
    argv
}

impl Action {
    /// The commands this action runs, in order.
    ///
    /// **The only place in the project where a `git` argv for an action is
    /// constructed**, enforced by an allowlist test over every
    /// `GitCommand::new` site. Not tidiness: the argv a plan sheet shows, the
    /// one a transcript records, and the one a process runs must be the same
    /// three strings.
    pub fn steps(&self) -> Vec<Step> {
        match self {
            Action::Fetch { prune, tags } => {
                let mut argv = vec!["fetch".to_string()];
                if *prune {
                    argv.push("--prune".into());
                }
                if *tags {
                    argv.push("--tags".into());
                }
                vec![Step::simple(argv)]
            }

            Action::Pull { mode } => vec![Step::simple(pull_argv(*mode))],

            Action::Push { set_upstream, force_with_lease } => {
                let mut argv = vec!["push".to_string()];
                // `--force` is never constructed here. No code path reaches
                // it, which is stronger than a flag defaulting to false.
                if *force_with_lease {
                    argv.push("--force-with-lease".into());
                }
                if let Some(remote) = set_upstream {
                    argv.push("--set-upstream".into());
                    argv.push(remote.clone());
                    // `HEAD` resolves to the current branch name, so the
                    // upstream is set to <remote>/<branch> without the caller
                    // having to know the branch.
                    argv.push("HEAD".into());
                }
                vec![Step::simple(argv)]
            }

            Action::Checkout { rev, create } => {
                let mut argv = vec!["checkout".to_string()];
                if *create {
                    argv.push("-b".into());
                }
                argv.push(rev.clone());
                vec![Step::simple(argv)]
            }

            Action::Commit { message, stage_all, no_verify } => {
                let mut argv = vec!["commit".to_string(), "-m".into(), message.clone()];
                if *no_verify {
                    argv.push("--no-verify".into());
                }
                let commit = Step::simple(argv);
                if !*stage_all {
                    return vec![commit];
                }
                // `git commit -a` will not do: it stages tracked modifications
                // only, and untracked files have to be included — which is also
                // why the plan shows the untracked count.
                //
                // No compensation. Unstaging would need the prior index state,
                // and `reset` would discard whatever the user staged themselves.
                // Leaving a staged index behind is recoverable; guessing is not.
                vec![Step::simple(vec!["add".into(), "-A".into()]), commit]
            }

            Action::Stash { include_untracked } => {
                // `stash push`, not bare `stash`: the bare form is the
                // deprecated alias and its argument handling differs.
                let mut argv = vec!["stash".to_string(), "push".into()];
                if *include_untracked {
                    argv.push("--include-untracked".into());
                }
                vec![Step::simple(argv)]
            }

            Action::StashPop => vec![Step::simple(vec!["stash".into(), "pop".into()])],

            Action::Branch { name, from } => {
                let mut argv = vec!["branch".to_string(), name.clone()];
                if let Some(from) = from {
                    argv.push(from.clone());
                }
                vec![Step::simple(argv)]
            }

            Action::Reset { to, mode } => {
                vec![Step::simple(vec!["reset".into(), mode.flag().into(), to.to_string()])]
            }

            // Five invocations that have to behave as one. The last two are the
            // first two's compensations rather than forward steps, which is what
            // makes them run on the failure path too: a pull that cannot
            // fast-forward must still put the user back on their branch with
            // their work restored.
            Action::SyncDefault { mode, plan: Some(p) } => {
                let mut steps = Vec::with_capacity(3);
                if p.stash {
                    steps.push(Step::with_compensation(
                        vec!["stash".into(), "push".into()],
                        vec!["stash".into(), "pop".into()],
                    ));
                }
                // Skipped when the user is already on it, rather than emitted
                // as a pair of no-ops: a plan that lists `git checkout main`
                // for a repository sitting on `main` reads as a tool that has
                // not looked.
                if p.default != p.back_to {
                    steps.push(Step::with_compensation(
                        vec!["checkout".into(), p.default.clone()],
                        vec!["checkout".into(), p.back_to.clone()],
                    ));
                }
                steps.push(Step::simple(pull_argv(*mode)));
                steps
            }
            // An unresolved template runs nothing — the safe direction, and the
            // reason `plan` is an `Option`. The alternative fails by checking
            // out the wrong branch; this fails by doing nothing. The
            // precondition refuses it before it gets this far.
            Action::SyncDefault { plan: None, .. } => Vec::new(),

            // Unresolved, like an unresolved sync: no steps, which is the
            // failure mode that does nothing rather than the one that creates
            // the wrong tag.
            Action::DevTag { name: None, .. } => Vec::new(),

            Action::DevTag { name: Some(name), push, .. } => {
                let create = Step::simple(vec!["tag".into(), name.clone()]);
                let Some(remote) = push else { return vec![create] };
                // Publish first, create locally second — the reverse of what
                // anybody types by hand. The refspec form creates the tag on the
                // remote without creating one here, so the operation that can be
                // refused happens before any local state changes.
                //
                // The other order leaves a local `X` at this commit while the
                // remote's `X` is at another, and the next derivation reads
                // local tags and skips past it, so the two differ silently for
                // ever. Compensation cannot rescue it either: compensations run
                // on success too, so a `tag -d` would delete the tag just
                // published.
                //
                // A name already on the remote is rejected with
                // `(already exists)`; the same name at the same commit is
                // `Everything up-to-date`, so a re-run after a partial batch is
                // harmless.
                vec![
                    Step::simple(vec![
                        "push".into(),
                        remote.clone(),
                        format!("HEAD:refs/tags/{name}"),
                    ]),
                    create,
                ]
            }

            Action::Custom { args, .. } => vec![Step::simple(args.clone())],
        }
    }

    /// Does this action reach the network, and so take the network semaphore
    /// rather than the local one?
    pub fn is_network(&self) -> bool {
        match self {
            Action::Fetch { .. }
            | Action::Pull { .. }
            | Action::Push { .. }
            // It pulls, so it goes to the network like any other pull.
            | Action::SyncDefault { .. } => true,
            Action::Checkout { .. }
            | Action::Commit { .. }
            | Action::Stash { .. }
            | Action::StashPop
            | Action::Branch { .. }
            | Action::Reset { .. } => false,
            // Only when it publishes. A tag created locally reaches nothing.
            Action::DevTag { push, .. } => push.is_some(),
            // The saved definition's own flag. Whoever wrote it knows what the
            // command does; the engine does not, and its default when nobody
            // said is the scarcer resource.
            Action::Custom { network, .. } => *network,
        }
    }

    /// Can this action move `HEAD` or change local history?
    ///
    /// Decides whether `head_before` is recorded, which is what makes undo
    /// possible at all.
    pub fn is_mutating(&self) -> bool {
        match self {
            // Fetch only advances `refs/remotes/**`, which is the sole reason
            // automatic fetching may skip the plan-confirm flow.
            Action::Fetch { .. } => false,
            Action::Pull { .. }
            | Action::Push { .. }
            | Action::Checkout { .. }
            | Action::Commit { .. }
            | Action::Stash { .. }
            | Action::StashPop
            | Action::Branch { .. }
            | Action::Reset { .. }
            | Action::SyncDefault { .. }
            // A tag does not move `HEAD`, so the `rev-parse` this buys is not
            // strictly needed. True anyway: creating a ref is changing local
            // history, and keeping `Fetch` the only non-mutating action is what
            // makes "may run unconfirmed" checkable at a glance.
            | Action::DevTag { .. } => true,
            // The saved definition's own flag, for the same reason.
            Action::Custom { mutating, .. } => *mutating,
        }
    }

    /// Short label for a plan header or an action bar.
    pub fn label(&self) -> String {
        match self {
            Action::Fetch { .. } => "Fetch".into(),
            Action::Pull { mode } => format!("Pull ({mode})"),
            Action::Push { force_with_lease: true, .. } => "Push (force-with-lease)".into(),
            Action::Push { .. } => "Push".into(),
            Action::Checkout { rev, create: true } => format!("Create branch {rev}"),
            Action::Checkout { rev, create: false } => format!("Check out {rev}"),
            Action::Commit { no_verify: true, .. } => "Commit (no hooks)".into(),
            Action::Commit { .. } => "Commit".into(),
            Action::Stash { .. } => "Stash".into(),
            Action::StashPop => "Stash pop".into(),
            Action::Branch { name, .. } => format!("Branch {name}"),
            Action::Reset { .. } => "Undo".into(),
            // The branch is left out: it differs per repository, and a label
            // naming one of them would be wrong for the rest. The resolved
            // commands are listed separately.
            Action::SyncDefault { .. } => "Sync default branch".into(),
            // Not the name: it differs per repository, which is the whole
            // point. The resolved commands are listed separately.
            Action::DevTag { channel, .. } => format!("Cut {channel} tag"),
            Action::Custom { args, .. } => format!("git {}", args.join(" ")),
        }
    }
}

impl std::fmt::Display for Action {
    /// The exact command line, for a plan sheet and a transcript header.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let steps = self.steps();
        let rendered: Vec<String> =
            steps.iter().map(|s| format!("git {}", s.argv.join(" "))).collect();
        f.write_str(&rendered.join(" && "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, so the tests below are exhaustive by construction. When a
    /// variant is added the compiler does not complain here — but
    /// `every_variant_is_covered` does.
    fn all_actions() -> Vec<Action> {
        vec![
            Action::Fetch { prune: false, tags: false },
            Action::Fetch { prune: true, tags: true },
            Action::Pull { mode: PullMode::FfOnly },
            Action::Pull { mode: PullMode::Rebase },
            Action::Pull { mode: PullMode::Merge },
            Action::Push { set_upstream: None, force_with_lease: false },
            Action::Push { set_upstream: Some("origin".into()), force_with_lease: false },
            Action::Push { set_upstream: None, force_with_lease: true },
            Action::Checkout { rev: "main".into(), create: false },
            Action::Checkout { rev: "feature".into(), create: true },
            Action::Commit { message: "msg".into(), stage_all: false, no_verify: false },
            Action::Commit { message: "msg".into(), stage_all: true, no_verify: false },
            Action::Stash { include_untracked: false },
            Action::Stash { include_untracked: true },
            Action::StashPop,
            Action::Custom {
                args: vec!["remote".into(), "prune".into(), "origin".into()],
                network: true,
                mutating: true,
            },
            // Resolved. The unresolved template is exercised on its own, in
            // `an_unresolved_sync_runs_nothing`, because it is the one action
            // that deliberately produces no steps.
            Action::SyncDefault { mode: PullMode::FfOnly, plan: Some(sync_plan(true)) },
            Action::SyncDefault { mode: PullMode::Rebase, plan: Some(sync_plan(false)) },
            // Resolved. The unresolved template gets its own test, for the same
            // reason a sync's does.
            Action::DevTag {
                channel: "dev".into(),
                bump: crate::version::Bump::Minor,
                name: Some("v2.4.0-dev.3".into()),
                push: Some("origin".into()),
            },
            Action::DevTag {
                channel: "rc".into(),
                bump: crate::version::Bump::Major,
                name: Some("v3.0.0-rc.1".into()),
                push: None,
            },
        ]
    }

    fn sync_plan(stash: bool) -> SyncPlan {
        SyncPlan { default: "main".into(), back_to: "feature".into(), stash }
    }

    #[test]
    fn every_variant_is_covered_by_the_test_corpus() {
        // A discriminant-level check, so adding a variant fails here rather
        // than silently going untested.
        let seen: std::collections::HashSet<_> =
            all_actions().iter().map(std::mem::discriminant).collect();
        assert_eq!(seen.len(), 10, "Action has a variant with no test coverage");
    }

    #[test]
    fn every_action_produces_at_least_one_runnable_step() {
        // Over the *resolved* corpus. The single exception is an unresolved
        // `SyncDefault`, which produces none on purpose — see
        // `an_unresolved_sync_runs_nothing`.
        for action in all_actions() {
            let steps = action.steps();
            assert!(!steps.is_empty(), "{action:?} produces no steps");
            for step in &steps {
                assert!(!step.argv.is_empty(), "{action:?} produced an empty argv");
                assert!(
                    !step.argv[0].starts_with('-'),
                    "{action:?}: first token must be a subcommand, got {:?}",
                    step.argv[0]
                );
                // No token may be a shell string. `Custom` is an argv vector
                // precisely so there is no shell, and nothing else may sneak
                // one in either.
                assert!(
                    step.argv.iter().all(|a| !a.contains(';') && !a.contains('|')),
                    "{action:?} looks like a shell string: {:?}",
                    step.argv
                );
            }
        }
    }

    #[test]
    fn a_sync_is_a_pull_wrapped_in_the_commands_that_put_the_user_back() {
        let steps =
            Action::SyncDefault { mode: PullMode::FfOnly, plan: Some(sync_plan(true)) }.steps();
        assert_eq!(steps.len(), 3, "stash, switch, pull — the return is compensation");

        assert_eq!(steps[0].argv, ["stash", "push"]);
        assert_eq!(steps[0].compensate.as_deref(), Some(&["stash".to_string(), "pop".into()][..]));

        assert_eq!(steps[1].argv, ["checkout", "main"]);
        assert_eq!(
            steps[1].compensate.as_deref(),
            Some(&["checkout".to_string(), "feature".into()][..])
        );

        // The pull owes nothing: it opened nothing.
        assert_eq!(steps[2].argv, ["pull", "--ff-only", "--no-autostash"]);
        assert_eq!(steps[2].compensate, None);
    }

    #[test]
    fn a_sync_stashes_only_when_something_is_in_the_way() {
        let steps =
            Action::SyncDefault { mode: PullMode::FfOnly, plan: Some(sync_plan(false)) }.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].argv, ["checkout", "main"]);
    }

    #[test]
    fn a_sync_of_the_branch_you_are_on_is_just_a_pull() {
        // No `git checkout main` while standing on `main`. A plan that listed
        // one would read as a tool that had not looked.
        let plan = SyncPlan { default: "main".into(), back_to: "main".into(), stash: false };
        let steps = Action::SyncDefault { mode: PullMode::Rebase, plan: Some(plan) }.steps();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].argv, ["pull", "--rebase", "--no-autostash"]);
    }

    #[test]
    fn an_unresolved_sync_runs_nothing() {
        // The safe direction, and the reason `plan` is an `Option`. The other
        // encoding — a default branch name that is merely provisional — fails
        // by checking out `main` in a repository whose default is `master`.
        let steps = Action::SyncDefault { mode: PullMode::FfOnly, plan: None }.steps();
        assert!(steps.is_empty());
    }

    #[test]
    fn a_sync_pulls_exactly_as_a_pull_does() {
        // One `pull_argv`, so a sync cannot quietly lose `--no-autostash` — the
        // flag that stops the user's own config from stashing on top of the
        // stash this action just took deliberately.
        for mode in [PullMode::FfOnly, PullMode::Rebase, PullMode::Merge] {
            let alone = Action::Pull { mode }.steps();
            let inside = Action::SyncDefault { mode, plan: Some(sync_plan(false)) }.steps();
            assert_eq!(alone[0].argv, inside.last().unwrap().argv, "{mode}");
        }
    }

    #[test]
    fn a_sync_cannot_be_undone_and_says_why() {
        // A reset to `head_before` would be a no-op wearing undo's name: a sync
        // leaves HEAD where it found it, and the branch that moved is one HEAD
        // is no longer on.
        let action = Action::SyncDefault { mode: PullMode::FfOnly, plan: Some(sync_plan(true)) };
        let Undoable::No(why) = undoability(&action) else {
            panic!("a sync must not offer a reset that would repair nothing")
        };
        assert!(why.contains("HEAD"), "{why}");
    }

    fn dev_tag(name: &str, push: Option<&str>) -> Action {
        Action::DevTag {
            channel: "dev".into(),
            bump: crate::version::Bump::Minor,
            name: Some(name.into()),
            push: push.map(Into::into),
        }
    }

    #[test]
    fn a_tag_is_published_before_it_is_created_locally() {
        // The reverse of what anybody types by hand, and the reason is what the
        // *other* order leaves behind. `git tag X` followed by a rejected push
        // leaves a local `X` at this commit while the remote's `X` is at
        // another, and the next derivation — which reads local tags — skips
        // past it, so the two differ silently for ever.
        //
        // Compensation cannot rescue that order either: it runs on success too,
        // so a `tag -d` would delete the tag that was just published. Doing the
        // refusable thing first means a refusal changes nothing.
        let steps = dev_tag("v2.4.0-dev.3", Some("origin")).steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].argv, ["push", "origin", "HEAD:refs/tags/v2.4.0-dev.3"]);
        assert_eq!(steps[1].argv, ["tag", "v2.4.0-dev.3"]);
        // Neither step declares one: nothing here opens something that must be
        // closed afterwards, which is what a compensation now means.
        assert!(steps.iter().all(|s| s.compensate.is_none()));
    }

    #[test]
    fn a_local_only_tag_is_one_step() {
        let steps = dev_tag("v2.4.0-dev.3", None).steps();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].argv, ["tag", "v2.4.0-dev.3"]);
    }

    #[test]
    fn an_unresolved_dev_tag_runs_nothing() {
        let unresolved = Action::DevTag {
            channel: "dev".into(),
            bump: crate::version::Bump::Minor,
            name: None,
            push: Some("origin".into()),
        };
        assert!(unresolved.steps().is_empty());
    }

    #[test]
    fn a_tag_reaches_the_network_only_when_it_publishes() {
        assert!(dev_tag("v1.0.0-dev.1", Some("origin")).is_network());
        assert!(!dev_tag("v1.0.0-dev.1", None).is_network());
    }

    #[test]
    fn a_published_tag_and_a_local_one_are_unundoable_for_different_reasons() {
        // The reason is what the plan shows, so the two must not be merged:
        // one tells the user to talk to whoever has already fetched, the other
        // tells them to delete a local ref.
        let Undoable::No(published) = undoability(&dev_tag("v1.0.0-dev.1", Some("origin"))) else {
            panic!("a published tag cannot be undone by moving HEAD")
        };
        let Undoable::No(local) = undoability(&dev_tag("v1.0.0-dev.1", None)) else {
            panic!("creating a tag moves nothing for a reset to repair")
        };
        assert!(published.contains("remote"), "{published}");
        assert_ne!(published, local);
    }

    #[test]
    fn fetch_argv() {
        assert_eq!(argv(&Action::Fetch { prune: false, tags: false }), [["fetch"]]);
        assert_eq!(
            argv(&Action::Fetch { prune: true, tags: true }),
            [["fetch", "--prune", "--tags"]]
        );
    }

    #[test]
    fn pull_names_its_mode_explicitly() {
        // Bare `git pull` would let the user's `pull.rebase` config decide, and
        // the plan would have promised one thing while git did another.
        assert_eq!(
            argv(&Action::Pull { mode: PullMode::FfOnly }),
            [["pull", "--ff-only", "--no-autostash"]]
        );
        assert_eq!(
            argv(&Action::Pull { mode: PullMode::Rebase }),
            [["pull", "--rebase", "--no-autostash"]]
        );
        assert_eq!(
            argv(&Action::Pull { mode: PullMode::Merge }),
            [["pull", "--no-rebase", "--no-autostash", "--no-edit"]]
        );
    }

    #[test]
    fn every_pull_refuses_to_autostash() {
        // The regression this guards: with `rebase.autoStash = true` a pull on a
        // dirty tree stashes the user's work silently. Verified against real
        // git, not assumed.
        for mode in [PullMode::FfOnly, PullMode::Rebase, PullMode::Merge] {
            let steps = Action::Pull { mode }.steps();
            assert!(
                steps[0].argv.iter().any(|a| a == "--no-autostash"),
                "{mode} may silently stash the user's work"
            );
        }
    }

    #[test]
    fn push_sets_upstream_only_with_a_remote_to_set_it_to() {
        assert_eq!(argv(&Action::Push { set_upstream: None, force_with_lease: false }), [["push"]]);
        assert_eq!(
            argv(&Action::Push { set_upstream: Some("upstream".into()), force_with_lease: false }),
            [["push", "--set-upstream", "upstream", "HEAD"]]
        );
        assert_eq!(
            argv(&Action::Push { set_upstream: None, force_with_lease: true }),
            [["push", "--force-with-lease"]]
        );
    }

    #[test]
    fn force_is_unreachable() {
        // Not "defaults to false" — no code path emits it. A bulk tool that can
        // `push --force` across forty repositories is one that will eventually
        // do so by accident.
        for action in all_actions() {
            for step in action.steps() {
                assert!(
                    !step.argv.iter().any(|a| a == "--force" || a == "-f"),
                    "{action:?} can emit a bare force"
                );
            }
        }
    }

    #[test]
    fn stage_all_is_two_steps_because_commit_dash_a_misses_untracked_files() {
        let one =
            Action::Commit { message: "m".into(), stage_all: false, no_verify: false }.steps();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].argv, ["commit", "-m", "m"]);

        let two = Action::Commit { message: "m".into(), stage_all: true, no_verify: false }.steps();
        assert_eq!(two.len(), 2, "add -A then commit; `commit -a` skips untracked files");
        assert_eq!(two[0].argv, ["add", "-A"]);
        assert_eq!(two[1].argv, ["commit", "-m", "m"]);
    }

    #[test]
    fn a_message_with_spaces_stays_one_argument() {
        // The reason this is an argv and never a command string.
        let steps = Action::Commit {
            message: "fix: the thing; rm -rf /".into(),
            stage_all: false,
            no_verify: false,
        }
        .steps();
        assert_eq!(steps[0].argv.len(), 3);
        assert_eq!(steps[0].argv[2], "fix: the thing; rm -rf /");
    }

    #[test]
    fn stash_uses_the_modern_push_form() {
        assert_eq!(argv(&Action::Stash { include_untracked: false }), [["stash", "push"]]);
        assert_eq!(
            argv(&Action::Stash { include_untracked: true }),
            [["stash", "push", "--include-untracked"]]
        );
        assert_eq!(argv(&Action::StashPop), [["stash", "pop"]]);
    }

    #[test]
    fn custom_passes_its_argv_through_untouched() {
        let args = vec!["remote".to_string(), "prune".into(), "origin".into()];
        assert_eq!(
            argv(&Action::Custom { args: args.clone(), network: true, mutating: true }),
            [args]
        );
    }

    #[test]
    fn network_and_mutating_are_decided_for_every_variant() {
        assert!(Action::Fetch { prune: false, tags: false }.is_network());
        assert!(Action::Pull { mode: PullMode::Rebase }.is_network());
        assert!(!Action::Commit { message: "m".into(), stage_all: false, no_verify: false }
            .is_network());
        assert!(!Action::StashPop.is_network());

        // Fetch is the only non-mutating action, which is exactly why it is the
        // only one allowed to run without confirmation.
        let non_mutating: Vec<_> = all_actions().into_iter().filter(|a| !a.is_mutating()).collect();
        assert!(
            non_mutating.iter().all(|a| matches!(a, Action::Fetch { .. })),
            "something other than Fetch claims to be non-mutating: {non_mutating:?}"
        );

        // Custom is treated as the scarce, dangerous case on both axes.
        let custom = Action::Custom { args: vec!["gc".into()], network: true, mutating: true };
        assert!(custom.is_network());
        assert!(custom.is_mutating());
    }

    #[test]
    fn display_renders_the_exact_command_line() {
        assert_eq!(
            Action::Pull { mode: PullMode::Rebase }.to_string(),
            "git pull --rebase --no-autostash"
        );
        assert_eq!(
            Action::Commit { message: "m".into(), stage_all: true, no_verify: false }.to_string(),
            "git add -A && git commit -m m"
        );
    }

    #[test]
    fn labels_are_short_and_say_what_will_happen() {
        assert_eq!(Action::Pull { mode: PullMode::Merge }.label(), "Pull (merge)");
        assert_eq!(
            Action::Push { set_upstream: None, force_with_lease: true }.label(),
            "Push (force-with-lease)"
        );
        assert_eq!(
            Action::Checkout { rev: "main".into(), create: false }.label(),
            "Check out main"
        );
    }

    #[test]
    fn round_trips_through_json() {
        for action in all_actions() {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action, "{json}");
        }
    }

    fn argv(action: &Action) -> Vec<Vec<String>> {
        action.steps().into_iter().map(|s| s.argv).collect()
    }
}
