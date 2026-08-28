//! The roots the user has chosen, and where they are kept.
//!
//! Plain paths — no security-scoped bookmarks. This is a non-sandboxed local
//! build, so a path is a path and stays valid across launches.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything persisted between launches.
///
/// Only roots today. The struct exists rather than a bare `Vec<PathBuf>` so
/// that adding a setting later is a field rather than a file-format change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(default)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    /// Application to open a repository in, by name as `open -a` understands it
    /// — "Visual Studio Code", "Zed". `None` falls back to `$EDITOR`, which for
    /// most people is a terminal program and will not work, so the row's menu
    /// says so rather than failing silently.
    pub editor: Option<String>,
    /// Application to open a repository's terminal in, by name as `open -a`
    /// understands it — "Ghostty", "iTerm". `None` is resolved by
    /// `handoff::terminal_app`, which always has an answer, so this differs
    /// from `editor`: leaving it unset is a working configuration rather than a
    /// missing one. The settings dialog shows what the resolution picked, so
    /// the guess is visible.
    pub terminal: Option<String>,
    /// Named custom commands.
    ///
    /// The deliberate escape hatch from the closed `Action` enum, which will not
    /// cover everything. The alternative is a shell loop with no plan, no
    /// transcript and no per-repository results.
    pub custom: Vec<CustomCommand>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// One saved custom command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomCommand {
    pub name: String,
    /// An argv vector, **never a shell string**. No shell, no interpolation, no
    /// injection surface — which is the whole reason this is safe to save and
    /// re-run across forty repositories.
    pub args: Vec<String>,
    /// Which semaphore it takes, and whether `head_before` is recorded. The
    /// engine cannot reason about an arbitrary command; whoever wrote the
    /// definition can.
    pub network: bool,
    pub mutating: bool,
    /// The user has been told, once, that preconditions and undo do not apply.
    ///
    /// Persisted **per definition** rather than per session: the point is that
    /// somebody read what this particular command does not get, not that they
    /// clicked through a dialog recently. A definition whose argv is edited has
    /// its acknowledgement cleared, because the thing they agreed to has
    /// changed.
    pub acknowledged: bool,
}

impl Default for CustomCommand {
    fn default() -> Self {
        // The conservative pair, matching `Action::Custom`'s own default when
        // nobody has said: the scarcer semaphore, and a recorded `head_before`.
        Self {
            name: String::new(),
            args: Vec::new(),
            network: true,
            mutating: true,
            acknowledged: false,
        }
    }
}

impl Config {
    /// Save or replace a custom command by name.
    ///
    /// Editing the argv clears the acknowledgement: what the user agreed to was
    /// *this command*, and a different one has not been agreed to.
    pub fn put_custom(&mut self, mut command: CustomCommand) {
        match self.custom.iter_mut().find(|c| c.name == command.name) {
            Some(existing) => {
                if existing.args != command.args {
                    command.acknowledged = false;
                }
                *existing = command;
            }
            None => self.custom.push(command),
        }
        self.custom.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn remove_custom(&mut self, name: &str) -> bool {
        let before = self.custom.len();
        self.custom.retain(|c| c.name != name);
        self.custom.len() != before
    }
}

impl Config {
    /// Add a root, ignoring one already covered.
    ///
    /// Returns whether anything changed. A root nested inside an existing one
    /// is rejected: the walk would find the same repositories twice and the
    /// sidebar would double-count them. A root that *contains* existing ones
    /// replaces them, because the broader choice is clearly the intent.
    pub fn add_root(&mut self, root: PathBuf) -> bool {
        if self.roots.iter().any(|r| root.starts_with(r)) {
            return false;
        }
        self.roots.retain(|r| !r.starts_with(&root));
        self.roots.push(root);
        self.roots.sort();
        true
    }

    pub fn remove_root(&mut self, root: &Path) -> bool {
        let before = self.roots.len();
        self.roots.retain(|r| r != root);
        self.roots.len() != before
    }
}

/// The file, under the state directory `crates/store` resolves.
const FILE: &str = "config.json";

pub fn path() -> Option<PathBuf> {
    git_scylla_store::path(FILE)
}

/// Read the stored configuration, or the default.
///
/// A missing or unreadable file is not an error: the first launch has no
/// configuration, and a corrupt one should leave the application usable rather
/// than refusing to start. It is logged by the store and replaced on the next
/// write.
pub fn load() -> Config {
    git_scylla_store::load_json(FILE).unwrap_or_default()
}

/// Write the configuration, atomically — see `git_scylla_store::write_atomic`
/// for why that matters for a file read at launch.
pub fn save(config: &Config) -> Result<(), git_scylla_store::StoreError> {
    git_scylla_store::save_json(FILE, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_custom_command_is_saved_and_replaced_by_name() {
        let mut c = Config::default();
        c.put_custom(CustomCommand {
            name: "prune".into(),
            args: vec!["remote".into(), "prune".into(), "origin".into()],
            ..Default::default()
        });
        c.put_custom(CustomCommand {
            name: "gc".into(),
            args: vec!["gc".into()],
            ..Default::default()
        });
        assert_eq!(c.custom.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["gc", "prune"]);
        assert!(c.remove_custom("gc"));
        assert!(!c.remove_custom("gc"));
        assert_eq!(c.custom.len(), 1);
    }

    #[test]
    fn editing_the_argv_clears_the_acknowledgement() {
        // What the user agreed to was *this command*. A different one has not
        // been agreed to, and the acknowledgement is the only thing standing
        // between a saved definition and forty repositories.
        let mut c = Config::default();
        c.put_custom(CustomCommand {
            name: "thing".into(),
            args: vec!["gc".into()],
            acknowledged: true,
            ..Default::default()
        });
        c.put_custom(CustomCommand {
            name: "thing".into(),
            args: vec!["push".into(), "--mirror".into()],
            acknowledged: true,
            ..Default::default()
        });
        assert!(!c.custom[0].acknowledged, "an edited command kept its acknowledgement");
    }

    #[test]
    fn renaming_leaves_the_acknowledgement_alone() {
        // Only the argv is what was agreed to. A rename is bookkeeping.
        let mut c = Config::default();
        c.put_custom(CustomCommand {
            name: "thing".into(),
            args: vec!["gc".into()],
            acknowledged: true,
            ..Default::default()
        });
        c.put_custom(CustomCommand {
            name: "thing".into(),
            args: vec!["gc".into()],
            acknowledged: true,
            ..Default::default()
        });
        assert!(c.custom[0].acknowledged);
    }

    #[test]
    fn a_root_inside_an_existing_one_is_not_added_twice() {
        // The walk would find the same repositories under both, and the sidebar
        // would show them under each.
        let mut c = Config::default();
        assert!(c.add_root("/work".into()));
        assert!(!c.add_root("/work/api".into()));
        assert_eq!(c.roots, vec![PathBuf::from("/work")]);
    }

    #[test]
    fn a_root_containing_existing_ones_replaces_them() {
        let mut c = Config::default();
        c.add_root("/work/api".into());
        c.add_root("/work/web".into());
        assert!(c.add_root("/work".into()));
        assert_eq!(c.roots, vec![PathBuf::from("/work")]);
    }

    #[test]
    fn unrelated_roots_coexist_in_a_stable_order() {
        let mut c = Config::default();
        c.add_root("/b".into());
        c.add_root("/a".into());
        assert_eq!(c.roots, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_treated_as_nested() {
        // `/work-old` starts with the *string* `/work` but is not inside it.
        let mut c = Config::default();
        c.add_root("/work".into());
        assert!(c.add_root("/work-old".into()));
        assert_eq!(c.roots.len(), 2);
    }

    #[test]
    fn removing_reports_whether_anything_went() {
        let mut c = Config::default();
        c.add_root("/a".into());
        assert!(c.remove_root(Path::new("/a")));
        assert!(!c.remove_root(Path::new("/a")));
        assert!(c.roots.is_empty());
    }

    #[test]
    fn a_corrupt_file_leaves_the_application_usable() {
        // Refusing to start because a settings file is malformed would be a
        // worse outcome than losing the setting.
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("GIT_SCYLLA_STATE_DIR", tmp.path());
        std::fs::write(tmp.path().join("config.json"), b"{ not json").unwrap();
        assert_eq!(load(), Config::default());
        std::env::remove_var("GIT_SCYLLA_STATE_DIR");
    }
}
