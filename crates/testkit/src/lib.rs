//! Fixture repositories, and the expectations that make them assertions.
//!
//! Built before the probe, deliberately: this module is the specification of
//! correct behaviour, and the probe is judged against it.
//!
//! Layout of a built fixture set:
//!
//! ```text
//! <dir>/
//!   home/       scratch HOME, so no fixture reads the developer's ~/.gitconfig
//!   origins/    bare repositories acting as remotes — outside the scan root on
//!               purpose, so they are not themselves discovered
//!   repos/      the scan root: every fixture the walker should find
//! ```

mod expect;
mod git;
mod set;

pub use expect::{normalize, Expect, FetchExpect, UpstreamExpect};
pub use git::{Git, GitError};
pub use set::{Fixture, FixtureSet};
