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
    /// A `std::sync::Mutex`: nothing is held across an `.await`.
    pub config: Mutex<Config>,
    /// `None` until the first scan; replaced wholesale on every `start_scan`,
    /// since `notify` has no cheap "watch this set instead".
    pub watcher: WatcherSlot,
    /// Held, not used. Its `Drop` is what stops the actor.
    _engine: Engine,
}

impl App {
    /// The persisted configuration.
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
