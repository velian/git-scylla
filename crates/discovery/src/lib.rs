//! Finding repositories under a set of roots.
//!
//! A raw filesystem walk, not gitignore semantics: the thing we are looking for
//! is very often inside a directory some `.gitignore` would exclude.

mod skip;
mod walk;

pub use skip::{is_git_dir, is_hard_skipped, looks_dataless, HARD_SKIP_NAMES};
pub use walk::{DiscoveryError, RepoFound, WalkOptions, Walker};
