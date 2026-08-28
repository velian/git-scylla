//! The guarantees, end to end, against real `git` and real signals.
//!
//! These are the tests the crate exists for. The unit tests check the tables and
//! the parsers; these check that a `git` which does not want to stop, stops.

use git_scylla_core::Stream;
use git_scylla_exec::{GitCommand, Stop};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn repo(dir: &Path) -> PathBuf {
    let repo = dir.join("r");
    std::fs::create_dir_all(&repo).unwrap();
    let out = std::process::Command::new("git")
        .args(["init", "-b", "main", "."])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    repo
}

/// Run a shell command as `git`'s child, via a `!`-prefixed alias — git's own
/// mechanism for it, and it puts the shell in the same process group.
fn via_alias(repo: &Path, script: &str) -> GitCommand {
    GitCommand::new(repo).args(["-c", &format!("alias.x=!{script}"), "x"])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_successful_command_reports_its_output_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());

    let out = via_alias(&repo, "sh -c 'echo one; echo two >&2; echo three'")
        .run(Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();

    assert_eq!(out.stop, Stop::Exited);
    assert_eq!(out.code, Some(0));
    assert!(out.success());

    let texts: Vec<&str> = out.log.iter().map(|l| l.text.as_str()).collect();
    assert!(texts.contains(&"one"), "{texts:?}");
    assert!(texts.contains(&"two"), "{texts:?}");
    assert!(texts.contains(&"three"), "{texts:?}");

    assert_eq!(out.log.iter().find(|l| l.text == "two").unwrap().stream, Stream::Stderr);
    assert_eq!(out.log.iter().find(|l| l.text == "one").unwrap().stream, Stream::Stdout);
    assert_eq!(out.last_stderr(), Some("two"));

    let stamps: Vec<_> = out.log.iter().map(|l| l.at).collect();
    assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "transcript is out of order");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_command_reports_its_code_and_its_error() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());

    let out = GitCommand::new(&repo)
        .args(["rev-parse", "--verify", "refs/heads/nope"])
        .run(Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();

    assert_eq!(out.stop, Stop::Exited);
    assert!(!out.success());
    assert!(out.code.is_some_and(|c| c != 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn git_not_being_runnable_is_distinct_from_git_failing() {
    let err = GitCommand::new("/nonexistent/directory/anywhere")
        .args(["status"])
        .run(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(matches!(err, Err(git_scylla_exec::ExecError::Spawn { .. })), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chatty_child_does_not_deadlock() {
    // ~250 KB down one pipe, past the ~64 KB kernel pipe buffer.
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());

    let out =
        via_alias(&repo, "sh -c 'i=0; while [ $i -lt 20000 ]; do echo line $i; i=$((i+1)); done'")
            .run(Instant::now() + Duration::from_secs(30))
            .await
            .unwrap();

    assert_eq!(out.stop, Stop::Exited, "deadlocked into the deadline");
    assert_eq!(out.code, Some(0));
    assert_eq!(out.log.len(), 20000);
    assert_eq!(out.log[0].text, "line 0");
    assert_eq!(out.log[19999].text, "line 19999");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chatty_child_is_elided_rather_than_retained_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());

    let out =
        via_alias(&repo, "sh -c 'i=0; while [ $i -lt 20000 ]; do echo line $i; i=$((i+1)); done'")
            .transcript_cap(4096)
            .run(Instant::now() + Duration::from_secs(30))
            .await
            .unwrap();

    assert_eq!(out.code, Some(0));
    assert!(out.log.len() < 20000);
    assert_eq!(out.log[0].text, "line 0");
    assert_eq!(out.log.last().unwrap().text, "line 19999");
    let notices: Vec<_> = out.log.iter().filter(|l| l.stream == Stream::Notice).collect();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].text.contains("elided"), "{}", notices[0].text);
}

/// Write a shell script that ignores `SIGTERM`, stays busy for ~5 s, and then
/// announces it survived by creating the file given as `$1`.
///
/// `sleep 1` five times rather than one `sleep 5`: a group `SIGTERM` kills the
/// `sleep`, and a trapping shell falls through to the next command
/// immediately — one sleep would let the marker appear within milliseconds of
/// the signal, asserting a race rather than a kill.
fn write_stubborn(dir: &Path) -> PathBuf {
    let path = dir.join("stubborn.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\ntrap '' TERM\nfor i in 1 2 3 4 5; do sleep 1; done\ntouch \"$1\"\n",
    )
    .unwrap();
    make_executable(&path);
    path
}

/// A script that backgrounds another script and then waits for it, so the
/// direct child dies to `SIGTERM` while a grandchild ignores it.
fn write_parent(dir: &Path) -> PathBuf {
    let path = dir.join("parent.sh");
    std::fs::write(&path, "#!/bin/sh\n\"$1\" \"$2\" &\nwait\n").unwrap();
    make_executable(&path);
    path
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_ignores_sigterm_is_still_reaped() {
    // git -> stubborn.sh -> sleep, all in one process group. Only SIGKILL ends it.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let repo = repo(&dir);
    let survived = dir.join("survived");
    let script = write_stubborn(&dir);

    let started = Instant::now();
    let out = via_alias(&repo, &format!("{} {}", script.display(), survived.display()))
        .run(Instant::now() + Duration::from_millis(600))
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(out.stop, Stop::TimedOut);
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?} to give up");

    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(!survived.exists(), "the SIGTERM-ignoring child outlived the job: it was not reaped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grandchild_that_outlives_its_parent_is_still_reaped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let repo = repo(&dir);
    let survived = dir.join("survived");
    let stubborn = write_stubborn(&dir);
    let parent = write_parent(&dir);

    let out = via_alias(
        &repo,
        &format!("{} {} {}", parent.display(), stubborn.display(), survived.display()),
    )
    .run(Instant::now() + Duration::from_millis(600))
    .await
    .unwrap();

    assert_eq!(out.stop, Stop::TimedOut, "the parent should still have been running");
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        !survived.exists(),
        "a grandchild outlived the job: the kill did not reach the whole process group"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timeout_says_so_in_the_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());

    let out = via_alias(&repo, "sh -c 'echo working; sleep 10'")
        .run(Instant::now() + Duration::from_millis(400))
        .await
        .unwrap();

    assert_eq!(out.stop, Stop::TimedOut);
    assert!(out.timed_out());
    assert!(out.log.iter().any(|l| l.text == "working"), "{:?}", out.log);
    let notice = out.log.iter().find(|l| l.stream == Stream::Notice).expect("a notice");
    assert!(notice.text.contains("timed out"), "{}", notice.text);
    assert!(notice.text.contains("killed the process group"), "{}", notice.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_kills_the_group_and_is_distinct_from_a_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let repo = repo(&dir);
    let survived = dir.join("survived");

    let token = tokio_util::sync::CancellationToken::new();
    let fired = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        fired.cancel();
    });

    let script = write_stubborn(&dir);
    let script = format!("{} {}", script.display(), survived.display());
    let started = Instant::now();
    // A deadline far in the future, so only cancellation can end this.
    let out = via_alias(&repo, &script)
        .cancel_with(token)
        .run(Instant::now() + Duration::from_secs(60))
        .await
        .unwrap();

    assert_eq!(out.stop, Stop::Cancelled);
    assert!(!out.timed_out(), "cancelled is not timed out");
    assert!(started.elapsed() < Duration::from_secs(3));
    let notice = out.log.iter().find(|l| l.stream == Stream::Notice).expect("a notice");
    assert!(notice.text.contains("cancelled"), "{}", notice.text);

    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(!survived.exists(), "cancellation left the child running");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_already_cancelled_token_stops_the_command_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();

    let started = Instant::now();
    let out = via_alias(&repo, "sh -c 'sleep 10'")
        .cancel_with(token)
        .run(Instant::now() + Duration::from_secs(60))
        .await
        .unwrap();

    assert_eq!(out.stop, Stop::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
}

/// A listener that answers every request with `401 Unauthorized` and a
/// `WWW-Authenticate` header, which is what makes `git` reach for credentials.
fn spawn_401_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                b"HTTP/1.1 401 Unauthorized\r\n\
                  WWW-Authenticate: Basic realm=\"git\"\r\n\
                  Content-Length: 0\r\n\r\n",
            );
            let _ = s.flush();
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_demanding_credentials_fails_fast_instead_of_hanging() {
    // Without the hardening, this waits on a terminal read forever. Asserting
    // the message, not just the timing: `terminal prompts disabled` is proof
    // that `GIT_TERMINAL_PROMPT` reached the child, not just that the
    // connection failed for some other reason.
    let port = spawn_401_server();
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());
    let url = format!("http://127.0.0.1:{port}/needs-auth.git");

    let started = Instant::now();
    let out = GitCommand::new(&repo)
        .args(["fetch", &url])
        .run(Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(out.stop, Stop::Exited, "it hung and had to be killed");
    assert!(!out.success());
    assert!(
        elapsed < Duration::from_secs(2),
        "took {elapsed:?}; it should fail in well under a second"
    );

    let transcript = out.log.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
    assert!(
        transcript.contains("terminal prompts disabled"),
        "the failure did not come from the hardening:\n{transcript}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn capture_shares_the_discipline_but_returns_raw_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();

    let out = GitCommand::new(&repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["--no-optional-locks", "status", "--porcelain=v2", "-z"])
        .capture(Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();

    assert!(out.success());
    assert_eq!(out.stdout, b"? a.txt\0", "raw NUL-separated bytes, undecoded");
    assert!(out.stderr.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn capture_honours_the_deadline_too() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let repo = repo(&dir);
    let survived = dir.join("survived");

    let script = write_stubborn(&dir);
    let out = via_alias(&repo, &format!("{} {}", script.display(), survived.display()))
        .capture(Instant::now() + Duration::from_millis(500))
        .await
        .unwrap();

    assert!(out.timed_out());
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(!survived.exists(), "capture() left a child running");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_argv_is_reportable_before_it_runs() {
    let cmd = GitCommand::new("/tmp").args(["fetch", "--prune", "origin"]);
    assert_eq!(cmd.argv(), vec!["git", "fetch", "--prune", "origin"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unroutable_remote_is_bounded_by_the_deadline_and_nothing_else() {
    // A silently dropped route (measured against TEST-NET-2) takes anywhere
    // from 4 s to 75 s, and `http.connectTimeout` does not bound it. The
    // guarantee here is the deadline, not a bound on git.
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());
    let started = Instant::now();
    let out = GitCommand::new(&repo)
        .args(["ls-remote", "https://198.51.100.1/nope.git"])
        .run(Instant::now() + Duration::from_millis(800))
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(!out.success());
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_connection_fails_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());
    let started = Instant::now();
    let out = GitCommand::new(&repo)
        .args(["ls-remote", "https://127.0.0.1:1/nope.git"])
        .run(Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(out.stop, Stop::Exited, "it should fail on its own, not be killed");
    assert!(!out.success());
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    assert!(out.last_stderr().is_some_and(|e| e.contains("unable to access")), "{:?}", out.log);
}
