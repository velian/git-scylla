//! Live progress.
//!
//! On a TTY: finished repositories scroll up as permanent lines while the
//! currently-running ones sit in a block below, redrawn in place. Off a TTY:
//! append-only, so `| tee` and CI logs stay readable — which is the whole
//! reason the two modes exist rather than one clever one.

use git_scylla_core::{JobState, RepoId};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};

/// Most running repositories to name at once. Beyond this the block would be
/// taller than a terminal and redrawing it would flicker.
const MAX_BLOCK: usize = 8;

pub struct Progress {
    tty: bool,
    /// Lines currently occupied by the live block, so they can be erased.
    drawn: usize,
    running: BTreeMap<RepoId, ()>,
    done: usize,
    total: usize,
}

impl Progress {
    pub fn new(total: usize) -> Self {
        Self {
            tty: std::io::stderr().is_terminal(),
            drawn: 0,
            running: BTreeMap::new(),
            done: 0,
            total,
        }
    }

    pub fn started(&mut self, repo: RepoId) {
        self.running.insert(repo, ());
        self.redraw();
    }

    /// Report a terminal state. Skips are not printed individually — the plan
    /// already grouped and counted them, and repeating each one buries the
    /// results that need reading.
    pub fn finished(&mut self, repo: &RepoId, state: &JobState) {
        self.running.remove(repo);
        if state.ran() {
            self.done += 1;
            self.erase();
            let mark = match state {
                JobState::Ok => "\u{2713}",
                JobState::Cancelled => "\u{25cb}",
                _ => "\u{2717}",
            };
            eprintln!("  {mark} {:<40} {state}", repo.name());
        }
        self.redraw();
    }

    /// Erase the live block, leaving the permanent lines above it.
    pub fn erase(&mut self) {
        if !self.tty || self.drawn == 0 {
            return;
        }
        let mut err = std::io::stderr();
        for _ in 0..self.drawn {
            let _ = write!(err, "\x1b[1A\x1b[2K");
        }
        let _ = write!(err, "\r");
        let _ = err.flush();
        self.drawn = 0;
    }

    fn redraw(&mut self) {
        if !self.tty {
            return;
        }
        self.erase();
        if self.running.is_empty() {
            return;
        }
        let mut err = std::io::stderr();
        let mut lines = 0;
        for (repo, _) in self.running.iter().take(MAX_BLOCK) {
            let _ = writeln!(err, "  \u{2026} {}", repo.name());
            lines += 1;
        }
        if self.running.len() > MAX_BLOCK {
            let _ = writeln!(err, "  \u{2026} and {} more", self.running.len() - MAX_BLOCK);
            lines += 1;
        }
        let _ = writeln!(err, "  [{}/{}]", self.done, self.total);
        lines += 1;
        let _ = err.flush();
        self.drawn = lines;
    }
}
