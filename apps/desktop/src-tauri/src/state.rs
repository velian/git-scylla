//! What the app owns for its lifetime.

use crate::config::Config;
use git_scylla_engine::{Engine, EngineHandle};
use git_scylla_watch::Watcher;
use std::sync::{Arc, Mutex, MutexGuard};

/// Where the watcher lives. Shared, so a background task can refill its index.
pub type WatcherSlot = Arc<Mutex<Option<Watcher>>>;

/// Managed state: a handle to the one engine, and the persisted configuration.
pub struct App {
    pub engine: EngineHandle,
    /// Guarded because commands run concurrently and two `add_root` calls must
    /// not lose one another. A `std::sync::Mutex` and not tokio's: nothing held
    /// across an await, and the critical sections are a `Vec` push and a file
    /// write.
    pub config: Mutex<Config>,
    /// The filesystem watcher, once roots are known.
    ///
    /// `None` until the first scan is asked for, because there is nothing to
    /// watch before that — and rebuilt rather than adjusted when the roots
    /// change, since `notify` has no cheap "watch this set instead".
    ///
    /// An `Arc` so the task that keeps its index current can hold the slot
    /// itself rather than reaching back through the `AppHandle` on every scan:
    /// a `State` borrowed inside a loop cannot outlive the iteration, and the
    /// lock must not be held across the await that asks the engine what it has.
    pub watcher: WatcherSlot,
    /// Held, not used. Its `Drop` is what stops the actor.
    _engine: Engine,
}

impl App {
    /// The persisted configuration.
    ///
    /// The lock is taken through here rather than at each of a dozen call
    /// sites, which each had to spell the poisoned-mutex message for
    /// themselves. Poisoning means a command panicked mid-edit; there is no
    /// recovery from a half-written `Config` that is better than stopping.
    pub fn config(&self) -> MutexGuard<'_, Config> {
        self.config.lock().expect("config poisoned")
    }

    pub fn new(engine: Engine, config: Config) -> Self {
        Self {
            engine: engine.handle(),
            config: Mutex::new(config),
            watcher: Arc::new(Mutex::new(None)),
            _engine: engine,
        }
    }
}
