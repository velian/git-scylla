//! Runs `git` as a subprocess. A spawned child cannot prompt, cannot outlive
//! its deadline, and cannot deadlock the caller on its own output.
//!
//! See `docs/README.md` for the design and diagrams.

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
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// `git` could not be started — not on `PATH`, or the directory is gone.
    #[error("could not run git in {dir}: {source}")]
    Spawn { dir: PathBuf, source: std::io::Error },
    #[error("could not determine the child's process group")]
    NoPid,
}

/// Why the child stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Exited on its own, successfully or not.
    Exited,
    TimedOut,
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

/// The result of running one command for its raw output rather than a transcript.
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

    /// Set an extra environment variable. Cannot override [`HARDENED_ENV`],
    /// which is applied last.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.extra_env.push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// Set several environment variables at once. Same guarantee as [`Self::env`].
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

        // Pipes are taken and drained before `wait_or_kill` is called, so a
        // full pipe buffer can never block the child inside `wait()`.
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // setpgid(0, 0): pgid becomes the child's own pid.
            .process_group(0)
            .kill_on_drop(true);
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
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
            None => std::future::pending().await,
        }
    };

    let stop = tokio::select! {
        status = child.wait() => {
            return (Stop::Exited, status.ok().and_then(|s| s.code()));
        }
        _ = tokio::time::sleep_until(deadline.into()) => Stop::TimedOut,
        _ = cancelled => Stop::Cancelled,
    };

    kill::terminate_group(pgid, child).await;
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
