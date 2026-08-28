//! The persisted configuration: roots, editor, terminal, custom commands.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything persisted between launches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(default)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    /// Application to open a repository in, by name as `open -a` understands
    /// it — "Visual Studio Code", "Zed". `None` falls back to `$EDITOR`.
    pub editor: Option<String>,
    /// Application to open a repository's terminal in, by name as `open -a`
    /// understands it — "Ghostty", "iTerm". `None` is resolved by
    /// `handoff::terminal_app`, which always has an answer.
    pub terminal: Option<String>,
    pub custom: Vec<CustomCommand>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// One saved custom command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomCommand {
    pub name: String,
    /// An argv vector, **never a shell string**.
    pub args: Vec<String>,
    /// Which semaphore it takes, and whether `head_before` is recorded.
    pub network: bool,
    pub mutating: bool,
    /// Persisted per definition: an edit to `args` clears it.
    pub acknowledged: bool,
}

impl Default for CustomCommand {
    fn default() -> Self {
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
    /// Save or replace a custom command by name. An edit to `args` clears
    /// `acknowledged`.
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
    /// Add a root. Rejects one nested inside an existing root; a root that
    /// contains existing ones replaces them. Returns whether anything changed.
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

const FILE: &str = "config.json";

pub fn path() -> Option<PathBuf> {
    git_scylla_store::path(FILE)
}

/// Read the stored configuration, or the default if missing or unreadable.
pub fn load() -> Config {
    git_scylla_store::load_json(FILE).unwrap_or_default()
}

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
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("GIT_SCYLLA_STATE_DIR", tmp.path());
        std::fs::write(tmp.path().join("config.json"), b"{ not json").unwrap();
        assert_eq!(load(), Config::default());
        std::env::remove_var("GIT_SCYLLA_STATE_DIR");
    }
}
