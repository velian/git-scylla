//! Finding repositories under a set of roots.

mod skip;
mod walk;

pub use skip::{is_git_dir, is_hard_skipped, looks_dataless, HARD_SKIP_NAMES};
pub use walk::{DiscoveryError, RepoFound, WalkOptions, Walker};
