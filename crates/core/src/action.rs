use crate::Oid;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
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
        set_upstream: Option<String>,
        force_with_lease: bool,
    },
    Checkout {
        rev: String,
        create: bool,
    },
    Commit {
        message: String,
        stage_all: bool,
        no_verify: bool,
    },
    Stash {
        include_untracked: bool,
    },
    StashPop,
    Branch {
        name: String,
        from: Option<String>,
    },
    Reset {
        to: Oid,
        mode: ResetMode,
    },
    SyncDefault {
        mode: PullMode,
        plan: Option<SyncPlan>,
    },
    DevTag {
        channel: String,
        bump: crate::version::Bump,
        name: Option<String>,
        push: Option<String>,
    },
    Custom {
        args: Vec<String>,
        network: bool,
        mutating: bool,
    },
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPlan {
    pub default: String,
    pub back_to: String,
    pub stash: bool,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetMode {
    Soft,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undoable {
    Reset(ResetMode),
    Switch,
    No(&'static str),
}

pub fn undoability(action: &Action) -> Undoable {
    match action {
        Action::Commit { .. } => Undoable::Reset(ResetMode::Soft),
        Action::Pull { .. } | Action::StashPop => Undoable::Reset(ResetMode::Hard),
        Action::Checkout { .. } => Undoable::Switch,
        Action::Branch { .. } => Undoable::No("creating a branch moved nothing"),
        Action::Fetch { .. } => Undoable::No("fetch only advances remote-tracking refs"),
        Action::Push { .. } => Undoable::No("the remote has already accepted the commits"),
        Action::Stash { .. } => Undoable::No("the changes are on the stash, not in HEAD"),
        Action::SyncDefault { .. } => {
            Undoable::No("a sync leaves HEAD where it found it; only the default branch moved")
        }
        Action::DevTag { push: Some(_), .. } => {
            Undoable::No("the remote has already accepted the tag")
        }
        Action::DevTag { push: None, .. } => Undoable::No("creating a tag moved nothing"),
        Action::Custom { .. } => Undoable::No("arbitrary command; effects are unknown"),
        Action::Reset { .. } => Undoable::No("an undo is not itself undone"),
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullMode {
    FfOnly,
    Rebase,
    Merge,
}

impl PullMode {
    fn flag(self) -> &'static str {
        match self {
            PullMode::FfOnly => "--ff-only",
            PullMode::Rebase => "--rebase",
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pass {
    Forward,
    Cleanup,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum StepState {
    Pending,
    Running,
    Ok,
    Failed { code: i32 },
    Cancelled,
    NotRun,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRun {
    pub step: Step,
    pub pass: Pass,
    pub state: StepState,
    pub log: std::ops::Range<usize>,
}

impl StepRun {
    pub fn pending(step: Step, pass: Pass) -> Self {
        Self { step, pass, state: StepState::Pending, log: 0..0 }
    }
}

fn pull_argv(mode: PullMode) -> Vec<String> {
    let mut argv = vec!["pull".to_string(), mode.flag().to_string()];
    argv.push("--no-autostash".into());
    if matches!(mode, PullMode::Merge) {
        argv.push("--no-edit".into());
    }
    argv
}

impl Action {
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
                if *force_with_lease {
                    argv.push("--force-with-lease".into());
                }
                if let Some(remote) = set_upstream {
                    argv.push("--set-upstream".into());
                    argv.push(remote.clone());
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
                vec![Step::simple(vec!["add".into(), "-A".into()]), commit]
            }

            Action::Stash { include_untracked } => {
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

            Action::SyncDefault { mode, plan: Some(p) } => {
                let mut steps = Vec::with_capacity(3);
                if p.stash {
                    steps.push(Step::with_compensation(
                        vec!["stash".into(), "push".into()],
                        vec!["stash".into(), "pop".into()],
                    ));
                }
                if p.default != p.back_to {
                    steps.push(Step::with_compensation(
                        vec!["checkout".into(), p.default.clone()],
                        vec!["checkout".into(), p.back_to.clone()],
                    ));
                }
                steps.push(Step::simple(pull_argv(*mode)));
                steps
            }
            Action::SyncDefault { plan: None, .. } => Vec::new(),

            Action::DevTag { name: None, .. } => Vec::new(),

            Action::DevTag { name: Some(name), push, .. } => {
                let create = Step::simple(vec!["tag".into(), name.clone()]);
                let Some(remote) = push else { return vec![create] };
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

    pub fn is_resolved(&self) -> bool {
        match self {
            Action::SyncDefault { plan, .. } => plan.is_some(),
            Action::DevTag { name, .. } => name.is_some(),
            Action::Fetch { .. }
            | Action::Pull { .. }
            | Action::Push { .. }
            | Action::Checkout { .. }
            | Action::Commit { .. }
            | Action::Stash { .. }
            | Action::StashPop
            | Action::Branch { .. }
            | Action::Reset { .. }
            | Action::Custom { .. } => true,
        }
    }

    pub fn is_network(&self) -> bool {
        match self {
            Action::Fetch { .. }
            | Action::Pull { .. }
            | Action::Push { .. }
            | Action::SyncDefault { .. } => true,
            Action::Checkout { .. }
            | Action::Commit { .. }
            | Action::Stash { .. }
            | Action::StashPop
            | Action::Branch { .. }
            | Action::Reset { .. } => false,
            Action::DevTag { push, .. } => push.is_some(),
            Action::Custom { network, .. } => *network,
        }
    }

    pub fn is_mutating(&self) -> bool {
        match self {
            // Fetch only advances `refs/remotes/**`.
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
            | Action::DevTag { .. } => true,
            Action::Custom { mutating, .. } => *mutating,
        }
    }

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
            Action::SyncDefault { .. } => "Sync default branch".into(),
            Action::DevTag { channel, .. } => format!("Cut {channel} tag"),
            Action::Custom { args, .. } => format!("git {}", args.join(" ")),
        }
    }
}

impl std::fmt::Display for Action {
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
            Action::SyncDefault { mode: PullMode::FfOnly, plan: Some(sync_plan(true)) },
            Action::SyncDefault { mode: PullMode::Rebase, plan: Some(sync_plan(false)) },
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
        let seen: std::collections::HashSet<_> =
            all_actions().iter().map(std::mem::discriminant).collect();
        assert_eq!(seen.len(), 10, "Action has a variant with no test coverage");
    }

    #[test]
    fn every_action_produces_at_least_one_runnable_step() {
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
        let plan = SyncPlan { default: "main".into(), back_to: "main".into(), stash: false };
        let steps = Action::SyncDefault { mode: PullMode::Rebase, plan: Some(plan) }.steps();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].argv, ["pull", "--rebase", "--no-autostash"]);
    }

    #[test]
    fn an_unresolved_sync_runs_nothing() {
        let steps = Action::SyncDefault { mode: PullMode::FfOnly, plan: None }.steps();
        assert!(steps.is_empty());
    }

    #[test]
    fn a_sync_pulls_exactly_as_a_pull_does() {
        for mode in [PullMode::FfOnly, PullMode::Rebase, PullMode::Merge] {
            let alone = Action::Pull { mode }.steps();
            let inside = Action::SyncDefault { mode, plan: Some(sync_plan(false)) }.steps();
            assert_eq!(alone[0].argv, inside.last().unwrap().argv, "{mode}");
        }
    }

    #[test]
    fn a_sync_cannot_be_undone_and_says_why() {
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
        let steps = dev_tag("v2.4.0-dev.3", Some("origin")).steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].argv, ["push", "origin", "HEAD:refs/tags/v2.4.0-dev.3"]);
        assert_eq!(steps[1].argv, ["tag", "v2.4.0-dev.3"]);
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

        let non_mutating: Vec<_> = all_actions().into_iter().filter(|a| !a.is_mutating()).collect();
        assert!(
            non_mutating.iter().all(|a| matches!(a, Action::Fetch { .. })),
            "something other than Fetch claims to be non-mutating: {non_mutating:?}"
        );

        let custom = Action::Custom { args: vec!["gc".into()], network: true, mutating: true };
        assert!(custom.is_network());
        assert!(custom.is_mutating());
    }

    #[test]
    fn resolved_and_runnable_mean_the_same_thing() {
        for action in all_actions() {
            assert!(action.is_resolved(), "all_actions carries only runnable shapes: {action:?}");
            assert!(!action.steps().is_empty(), "a resolved action expands to steps: {action:?}");
        }

        let templates = [
            Action::SyncDefault { mode: PullMode::FfOnly, plan: None },
            Action::DevTag {
                channel: "dev".into(),
                bump: crate::version::Bump::Minor,
                name: None,
                push: Some("origin".into()),
            },
        ];
        for action in templates {
            assert!(!action.is_resolved(), "{action:?}");
            assert!(action.steps().is_empty(), "a template must expand to nothing: {action:?}");
        }
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
