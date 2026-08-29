//! Builds real git repositories on disk, paired with the snapshot each must
//! produce. See `docs/README.md` for layout and design.

mod expect;
mod git;
mod set;

pub use expect::{normalize, Expect, FetchExpect, RefExpect, UpstreamExpect};
pub use git::{Git, GitError};
pub use set::{Fixture, FixtureSet};
