//! Where git-scylla keeps what has to outlive a process.
//!
//! The CLI's last-run transcripts, the shell's configuration and the startup
//! cache all need the same directory rule. Two of them had already spelled it
//! out separately, byte for byte, while disagreeing about how to write a file.
//! A rule each component spells for itself is one that ends up spelled three
//! ways.
//!
//! Deliberately not in `core`: `core` is the domain and touches neither the
//! filesystem nor the environment.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The state directory: `$GIT_SCYLLA_STATE_DIR`, else the macOS
/// application-support directory.
///
/// One variable for every consumer, on purpose. It is what keeps a test suite
/// and a CI run out of the developer's real state, and someone who redirects it
/// expects to have redirected all of it — a second variable, or a component
/// that honoured none, would make that promise false in exactly the case where
/// it matters.
///
/// `None` means `$HOME` is unset and there is nowhere to put anything, which
/// callers treat as "this feature is unavailable" rather than as an error.
pub fn dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("GIT_SCYLLA_STATE_DIR") {
        return Some(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/dev.jvs.git-scylla"))
}

/// The path of one file in the state directory.
pub fn path(name: &str) -> Option<PathBuf> {
    Some(dir()?.join(name))
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no state directory: $HOME is not set")]
    NoDirectory,
    #[error("could not write {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("could not serialize: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Write `bytes` to `path`, creating the directory, without ever leaving a
/// half-written file behind.
///
/// Into a temporary in the same directory and renamed over the target, so an
/// interrupted write cannot produce a file the next launch reads as corrupt.
/// Same directory because a rename across filesystems is not atomic.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let parent = path.parent().ok_or(StoreError::NoDirectory)?;
    std::fs::create_dir_all(parent)
        .map_err(|source| StoreError::Io { path: parent.to_path_buf(), source })?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|source| StoreError::Io { path: tmp.clone(), source })?;
    std::fs::rename(&tmp, path)
        .map_err(|source| StoreError::Io { path: path.to_path_buf(), source })
}

/// Read and deserialize one file from the state directory.
///
/// `None` for every failure — missing, unreadable, malformed. A first launch
/// has no state, and a corrupt file should leave the application usable rather
/// than refusing to start; the distinction between the two is not one any
/// caller here acts on differently, so it is not offered.
pub fn load_json<T: DeserializeOwned>(name: &str) -> Option<T> {
    let path = path(name)?;
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(%e, path = %path.display(), "unreadable state; ignoring it");
            None
        }
    }
}

/// Serialize and write one file to the state directory, atomically.
pub fn save_json<T: Serialize>(name: &str, value: &T) -> Result<(), StoreError> {
    let path = path(name).ok_or(StoreError::NoDirectory)?;
    write_atomic(&path, &serde_json::to_vec_pretty(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests set `GIT_SCYLLA_STATE_DIR`, which is process-wide, so they run
    /// under one lock rather than racing each other's directory.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_state_dir<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("GIT_SCYLLA_STATE_DIR", tmp.path());
        let out = f(tmp.path());
        std::env::remove_var("GIT_SCYLLA_STATE_DIR");
        out
    }

    #[test]
    fn the_override_beats_the_application_support_directory() {
        with_state_dir(|d| {
            assert_eq!(dir().as_deref(), Some(d));
            assert_eq!(path("x.json").as_deref(), Some(d.join("x.json").as_path()));
        });
    }

    #[test]
    fn a_value_round_trips() {
        with_state_dir(|_| {
            save_json("thing.json", &vec![1u32, 2, 3]).unwrap();
            assert_eq!(load_json::<Vec<u32>>("thing.json"), Some(vec![1, 2, 3]));
        });
    }

    #[test]
    fn a_missing_or_corrupt_file_is_absent_rather_than_fatal() {
        // Refusing to start because a state file is malformed would be a worse
        // outcome than losing what it held.
        with_state_dir(|d| {
            assert_eq!(load_json::<Vec<u32>>("nothing.json"), None);
            std::fs::write(d.join("bad.json"), b"{ not json").unwrap();
            assert_eq!(load_json::<Vec<u32>>("bad.json"), None);
        });
    }

    #[test]
    fn a_cache_round_trips_for_the_roots_it_was_written_for() {
        with_state_dir(|_| {
            let roots = vec![PathBuf::from("/work")];
            Cache::new(roots.clone(), vec![], std::time::SystemTime::UNIX_EPOCH).save().unwrap();
            assert!(Cache::load_for(&roots).is_some());
            // A different working set is not this window's.
            assert!(Cache::load_for(&[PathBuf::from("/elsewhere")]).is_none());
        });
    }

    #[test]
    fn a_cache_from_an_older_layout_is_discarded_rather_than_migrated() {
        // It is re-derivable in under a second. Migration code for it would
        // cost more than it could save, and a migration bug would present as
        // wrong rows rather than as no rows.
        with_state_dir(|d| {
            let roots = vec![PathBuf::from("/work")];
            let mut cache = Cache::new(roots.clone(), vec![], std::time::SystemTime::UNIX_EPOCH);
            cache.version = CACHE_VERSION + 1;
            std::fs::write(d.join("cache.json"), serde_json::to_vec(&cache).unwrap()).unwrap();
            assert!(Cache::load_for(&roots).is_none());
        });
    }

    #[test]
    fn writing_creates_the_directory_and_leaves_no_temporary() {
        with_state_dir(|d| {
            let nested = d.join("deep/state.json");
            write_atomic(&nested, b"{}").unwrap();
            assert_eq!(std::fs::read(&nested).unwrap(), b"{}");
            let strays: Vec<_> = std::fs::read_dir(d.join("deep"))
                .unwrap()
                .filter_map(Result::ok)
                .map(|e| e.file_name())
                .filter(|n| n.to_string_lossy().ends_with(".tmp"))
                .collect();
            assert!(strays.is_empty(), "left a temporary behind: {strays:?}");
        });
    }
}

// ---- the startup cache --------------------------------------------------

/// What a previous run knew, so a launch has rows before it has a scan.
///
/// One JSON file. No SQLite: at fewer than a hundred repositories that is
/// unearned complexity, and the whole file is rewritten anyway because what it
/// mirrors serializes in a millisecond.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cache {
    pub version: u32,
    #[serde(with = "git_scylla_core::serde_time")]
    pub written_at: std::time::SystemTime,
    /// The roots this was written for. A cache written for a different working
    /// set is not wrong, but it is not this window's, and presenting it would
    /// show rows the user has since removed a root for.
    pub roots: Vec<PathBuf>,
    /// Snapshots verbatim, `FetchHealth` and all: a quarantine that forgets
    /// itself on restart is a retry loop with extra steps.
    pub repos: Vec<git_scylla_core::RepoSnapshot>,
}

/// Bumped whenever the shape of a snapshot changes.
///
/// A mismatch **discards**; there is deliberately no migration. The cache is a
/// convenience whose entire content is re-derivable in under a second, so
/// carrying migration code for it would cost more than it could ever save —
/// and a migration bug would present as wrong rows rather than as no rows.
pub const CACHE_VERSION: u32 = 1;

const CACHE_FILE: &str = "cache.json";

impl Cache {
    pub fn new(
        roots: Vec<PathBuf>,
        repos: Vec<git_scylla_core::RepoSnapshot>,
        at: std::time::SystemTime,
    ) -> Self {
        Self { version: CACHE_VERSION, written_at: at, roots, repos }
    }

    /// The cache, if there is a loadable one for these roots.
    ///
    /// `None` for missing, unreadable, malformed, a version mismatch, or a
    /// different root set. Every one of those means "start from a scan", and no
    /// caller does anything different about which.
    pub fn load_for(roots: &[PathBuf]) -> Option<Self> {
        let cache: Self = load_json(CACHE_FILE)?;
        if cache.version != CACHE_VERSION {
            tracing::info!(
                found = cache.version,
                want = CACHE_VERSION,
                "discarding a cache from an older layout"
            );
            return None;
        }
        if cache.roots != roots {
            tracing::debug!("the cache was written for different roots; ignoring it");
            return None;
        }
        Some(cache)
    }

    pub fn save(&self) -> Result<(), StoreError> {
        save_json(CACHE_FILE, self)
    }
}
