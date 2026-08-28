use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// A repository's identity: its canonicalized absolute path.
///
/// Canonicalized once, at construction, and never again. Every later
/// comparison, map key and selection is then a plain path comparison — no
/// filesystem access, and no chance of two ids for one repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoId(PathBuf);

impl RepoId {
    /// Canonicalize `path` into an id.
    ///
    /// Fails only if the path cannot be resolved, which for a repository just
    /// discovered means it was deleted underneath us. A caller that ignores
    /// that will key a map on a lie.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self(path.as_ref().canonicalize()?))
    }

    /// Build an id from a path already known to be canonical.
    ///
    /// For tests and for deserializing a cache written by a previous run.
    pub fn from_canonical(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The last path component — what a human calls the repository.
    pub fn name(&self) -> &str {
        self.0.file_name().and_then(|s| s.to_str()).unwrap_or("<unnamed>")
    }
}

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OidError {
    #[error("object id is {0} characters, expected 4-64")]
    Length(usize),
    #[error("object id contains a non-hex character")]
    NotHex,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
/// A git object id.
///
/// Hex string rather than 20 bytes: every producer (`git` stdout) and consumer
/// (`git reset --hard <oid>`, the transcript, the UI) speaks hex, so bytes would
/// mean decoding and re-encoding at every boundary to buy nothing. The width is
/// loose so SHA-256 repositories parse.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Oid(String);

impl Oid {
    pub fn parse(s: &str) -> Result<Self, OidError> {
        let s = s.trim();
        if !(4..=64).contains(&s.len()) {
            return Err(OidError::Length(s.len()));
        }
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(OidError::NotHex);
        }
        Ok(Self(s.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The customary 7-character prefix, for display only.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(7)]
    }
}

impl std::fmt::Display for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_validates() {
        assert_eq!(
            Oid::parse("A94A8FE5CCB19BA61C4C0873D391E987982FBBD3").unwrap().as_str(),
            "a94a8fe5ccb19ba61c4c0873d391e987982fbbd3"
        );
        assert_eq!(Oid::parse("abc").unwrap_err(), OidError::Length(3));
        assert_eq!(Oid::parse("zzzzzzz").unwrap_err(), OidError::NotHex);
        // porcelain v2 emits this for an unborn HEAD; it must not parse as an oid.
        assert_eq!(Oid::parse("(initial)").unwrap_err(), OidError::NotHex);
    }

    #[test]
    fn oid_short_is_safe_on_short_input() {
        assert_eq!(Oid::parse("abcd").unwrap().short(), "abcd");
    }

    #[test]
    fn repo_id_is_transparent_in_json() {
        let id = RepoId::from_canonical("/tmp/x");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"/tmp/x\"");
    }
}
