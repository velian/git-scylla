//! Subprocess discipline.
//!
//! Everything in this crate exists to make a spawned `git` incapable of
//! blocking, prompting, or escaping. It is the foundation the whole action
//! engine stands on, because the failure it prevents — one repository's `git`
//! sitting forever on an invisible credential prompt — does not present as an
//! error. It presents as a batch that never finishes.
//!
//! Three guarantees, each with a test:
//!
//! 1. **It cannot prompt.** A hardened environment plus `/dev/null` on stdin
//!    ([`env::HARDENED_ENV`]). A remote demanding credentials fails in under a
//!    second with `terminal prompts disabled`.
//! 2. **It cannot outlive its deadline.** Every child gets its own process
//!    group, and a timeout or cancellation kills the *group* — `git` spawns
//!    `ssh`, and signalling only the direct child orphans a live connection.
//! 3. **It cannot deadlock on its own output.** Both pipes are drained
//!    concurrently with the wait, never after it.

mod env;
mod kill;
mod lines;
mod transcript;

pub use env::HARDENED_ENV;
pub use transcript::{Transcript, DEFAULT_TRANSCRIPT_CAP};

use git_scylla_core::{LogLine, Stream};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

/// How long a child gets between `SIGTERM` and `SIGKILL`.
const GRACE: Duration = Duration::from_secs(2);

/// How long to wait for the pipes to close after the group is dead.
///
/// Defensive: with the group killed there is nothing left to hold the write
/// end, so this should never elapse. If it does, the transcript is truncated
/// and says so rather than the job hanging.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// `git` could not be started at all — not on `PATH`, or the repository
    /// directory is gone. Distinct from `git` running and failing, because the
    /// remedies are completely different.
    #[error("could not run git in {dir}: {source}")]
    Spawn { dir: PathBuf, source: std::io::Error },
    #[error("could not determine the child's process group")]
    NoPid,
}

/// Why the child stopped.
///
/// An enum rather than a `timed_out: bool`: a cancelled job and one that ran out
/// of time need different words in the UI and different states in
/// [`git_scylla_core`], and two booleans that cannot both be true is a worse way
/// to say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The child exited on its own, successfully or not.
    Exited,
    /// The deadline passed and the group was killed.
    TimedOut,
    /// The cancellation token fired and the group was killed.
    Cancelled,
}

/// The result of running one command.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// `None` when the child was killed by a signal, including by us.
    pub code: Option<i32>,
    /// Interleaved stdout and stderr, in the order it was read.
    pub log: Vec<LogLine>,
    pub duration: Duration,
    pub stop: Stop,
}

impl Outcome {
    pub fn success(&self) -> bool {
        self.stop == Stop::Exited && self.code == Some(0)
    }

    pub fn timed_out(&self) -> bool {
        self.stop == Stop::TimedOut
    }

    /// The last line the child wrote to stderr, which for `git` is almost always
    /// the `fatal:` that explains the failure.
    pub fn last_stderr(&self) -> Option<&str> {
        self.log.iter().rev().find(|l| l.stream == Stream::Stderr).map(|l| l.text.as_str())
    }
}

/// The result of running one command for its output rather than its transcript.
///
/// Separate from [`Outcome`] because the two callers want incompatible things.
/// A job wants an interleaved, timestamped, lossy-UTF-8 transcript. The probe
/// wants stdout as **raw bytes** — `git status -z` emits paths that need not be
/// UTF-8, and decoding them to build a transcript would corrupt the thing being
/// parsed. One spawn path, two output policies.
#[derive(Debug, Clone)]
pub struct Captured {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub duration: Duration,
    pub stop: Stop,
}

impl Captured {
    pub fn success(&self) -> bool {
        self.stop == Stop::Exited && self.code == Some(0)
    }

    pub fn timed_out(&self) -> bool {
        self.stop == Stop::TimedOut
    }
}

/// A `git` invocation that cannot prompt, hang, or leave orphans.
///
/// ```no_run
/// # use git_scylla_exec::GitCommand;
/// # use std::time::{Duration, Instant};
/// # async fn f() {
/// let outcome = GitCommand::new("/path/to/repo")
///     .args(["fetch", "--prune"])
///     .run(Instant::now() + Duration::from_secs(60))
///     .await
///     .expect("git is on PATH");
/// # }
/// ```
pub struct GitCommand {
    dir: PathBuf,
    args: Vec<OsString>,
    extra_env: Vec<(OsString, OsString)>,
    cancel: Option<CancellationToken>,
    cap: usize,
}

impl GitCommand {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            args: Vec::new(),
            extra_env: Vec::new(),
            cancel: None,
            cap: DEFAULT_TRANSCRIPT_CAP,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args.extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    /// Set an extra environment variable.
    ///
    /// **Cannot override the hardening.** [`HARDENED_ENV`] is applied last, so a
    /// caller that passes `GIT_TERMINAL_PROMPT=1` — whether by mistake or by
    /// clever reasoning about one special repository — gets `0` anyway. The
    /// guarantee is only worth having if it is unconditional.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.extra_env.push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// Set several environment variables at once.
    ///
    /// Same guarantee as [`Self::env`]: [`HARDENED_ENV`] is applied last and
    /// wins. This exists because every caller that threads a configured
    /// environment through — the job runner's three spawn sites and the
    /// probe's status command — had otherwise written the same loop, and four
    /// copies of "apply the environment" is four places to forget one.
    pub fn envs<'a, K, V>(mut self, vars: impl IntoIterator<Item = &'a (K, V)>) -> Self
    where
        K: AsRef<OsStr> + 'a,
        V: AsRef<OsStr> + 'a,
    {
        self.extra_env.extend(
            vars.into_iter().map(|(k, v)| (k.as_ref().to_os_string(), v.as_ref().to_os_string())),
        );
        self
    }

    /// Kill the process group when this token fires.
    pub fn cancel_with(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Override the transcript byte cap. See [`DEFAULT_TRANSCRIPT_CAP`].
    pub fn transcript_cap(mut self, bytes: usize) -> Self {
        self.cap = bytes;
        self
    }

    /// The argv this would run, for a plan sheet or a transcript header.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once("git".to_string())
            .chain(self.args.iter().map(|a| a.to_string_lossy().into_owned()))
            .collect()
    }

    /// Run, collecting an interleaved transcript.
    pub async fn run(self, deadline: Instant) -> Result<Outcome, ExecError> {
        let started = Instant::now();
        let cap = self.cap;
        let cancel = self.cancel.clone();
        let (mut child, pgid) = self.spawn()?;

        // Take the pipes before waiting on anything. A chatty child fills the
        // pipe buffer and blocks in `write`; if we were inside `wait()` at that
        // point, neither side would ever move again.
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<LogLine>(1024);
        tokio::spawn(lines::pump(stdout, Stream::Stdout, tx.clone()));
        tokio::spawn(lines::pump(stderr, Stream::Stderr, tx));

        let drain = tokio::spawn(async move {
            let mut t = Transcript::new(cap);
            while let Some(line) = rx.recv().await {
                t.push(line);
            }
            t
        });

        let (stop, code) = wait_or_kill(&mut child, pgid, deadline, cancel.as_ref()).await;

        // The group is dead, so the write ends are closed and this returns. The
        // timeout is belt and braces, and it degrades the transcript rather than
        // the job.
        let mut transcript = match tokio::time::timeout(DRAIN_TIMEOUT, drain).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                tracing::error!(%e, "transcript collector panicked");
                Transcript::new(cap)
            }
            Err(_) => {
                tracing::warn!("gave up draining child output");
                Transcript::new(cap)
            }
        };
        if let Some(note) = stop_notice(stop, deadline.saturating_duration_since(started)) {
            transcript.push(LogLine::notice(note));
        }

        Ok(Outcome { code, log: transcript.finish(), duration: started.elapsed(), stop })
    }

    /// Run, capturing raw stdout bytes and stderr as text.
    ///
    /// Same spawn, same environment, same process group, same deadline as
    /// [`Self::run`] — only the output policy differs. For the probe, whose
    /// stdout is NUL-separated data and not something to read.
    pub async fn capture(self, deadline: Instant) -> Result<Captured, ExecError> {
        let started = Instant::now();
        let cancel = self.cancel.clone();
        let (mut child, pgid) = self.spawn()?;

        let mut stdout_pipe = child.stdout.take().expect("piped");
        let mut stderr_pipe = child.stderr.take().expect("piped");
        let out = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut stdout_pipe, &mut buf).await;
            buf
        });
        let err = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr_pipe, &mut buf).await;
            buf
        });

        let (stop, code) = wait_or_kill(&mut child, pgid, deadline, cancel.as_ref()).await;

        let stdout = tokio::time::timeout(DRAIN_TIMEOUT, out)
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default();
        let stderr = tokio::time::timeout(DRAIN_TIMEOUT, err)
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_default();

        Ok(Captured {
            code,
            stdout,
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
            duration: started.elapsed(),
            stop,
        })
    }

    fn spawn(self) -> Result<(Child, i32), ExecError> {
        let mut cmd = Command::new("git");
        cmd.args(&self.args)
            .current_dir(&self.dir)
            // Nothing may read from a terminal. This is the guarantee the
            // environment hardening only *implies*: with no stdin, `ssh` cannot
            // prompt for a passphrase or a host-key confirmation even if some
            // future git forgets to honour GIT_TERMINAL_PROMPT.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so a deadline can kill the whole tree.
            // `setpgid(0, 0)` makes the pgid equal the pid.
            .process_group(0)
            // If a caller drops the future, do not leave the child running.
            .kill_on_drop(true);
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        // Last, and therefore unconditional.
        env::harden(&mut cmd);

        let child =
            cmd.spawn().map_err(|source| ExecError::Spawn { dir: self.dir.clone(), source })?;
        let pgid = child.id().ok_or(ExecError::NoPid)? as i32;
        Ok((child, pgid))
    }
}

/// Wait for the child, killing its group if the deadline or the token fires.
async fn wait_or_kill(
    child: &mut Child,
    pgid: i32,
    deadline: Instant,
    cancel: Option<&CancellationToken>,
) -> (Stop, Option<i32>) {
    let cancelled = async {
        match cancel {
            Some(t) => t.cancelled().await,
            // Never resolves, so `select!` reduces to the other two arms.
            None => std::future::pending().await,
        }
    };

    let stop = tokio::select! {
        // `Child::wait` is cancel-safe, so losing this race and calling it again
        // below is sound.
        status = child.wait() => {
            return (Stop::Exited, status.ok().and_then(|s| s.code()));
        }
        _ = tokio::time::sleep_until(deadline.into()) => Stop::TimedOut,
        _ = cancelled => Stop::Cancelled,
    };

    kill::terminate_group(pgid, child).await;
    // Reap, so the process does not linger as a zombie.
    let code = child.wait().await.ok().and_then(|s| s.code());
    (stop, code)
}

fn stop_notice(stop: Stop, budget: Duration) -> Option<String> {
    match stop {
        Stop::Exited => None,
        Stop::TimedOut => {
            Some(format!("timed out after {:.1}s; killed the process group", budget.as_secs_f64()))
        }
        Stop::Cancelled => Some("cancelled; killed the process group".to_string()),
    }
}
