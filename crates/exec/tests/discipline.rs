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

/// Run a shell command *as git's child*, via an alias.
///
/// Deliberately not an API hole in `GitCommand`: the discipline under test is
/// the discipline applied to `git`, so the test drives `git` and lets git spawn
/// the awkward process. A `!`-prefixed alias is git's own documented mechanism
/// for that, and it puts the shell in the same process group — which is exactly
/// the shape of the real problem (`git fetch` spawning `ssh`).
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

    // Both streams land in one log, each tagged with its origin.
    assert_eq!(out.log.iter().find(|l| l.text == "two").unwrap().stream, Stream::Stderr);
    assert_eq!(out.log.iter().find(|l| l.text == "one").unwrap().stream, Stream::Stdout);
    assert_eq!(out.last_stderr(), Some("two"));

    // Timestamps are non-decreasing, so reading the transcript top to bottom is
    // reading it in time order.
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
    // A directory that does not exist cannot be a working directory, so the
    // spawn itself fails. The caller must be able to tell that from `git`
    // running and exiting non-zero: one means "install git" or "the repository
    // vanished", the other means "git said no".
    let err = GitCommand::new("/nonexistent/directory/anywhere")
        .args(["status"])
        .run(Instant::now() + Duration::from_secs(5))
        .await;
    assert!(matches!(err, Err(git_scylla_exec::ExecError::Spawn { .. })), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chatty_child_does_not_deadlock() {
    // ~250 KB down one pipe, far past the ~64 KB kernel pipe buffer. If the
    // implementation waited on the child before draining, the child would block
    // in `write`, we would block in `wait`, and neither would ever move.
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
    // Both ends survive, and the middle says what it dropped.
    assert_eq!(out.log[0].text, "line 0");
    assert_eq!(out.log.last().unwrap().text, "line 19999");
    let notices: Vec<_> = out.log.iter().filter(|l| l.stream == Stream::Notice).collect();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].text.contains("elided"), "{}", notices[0].text);
}

/// Write a shell script that **ignores `SIGTERM`**, stays busy for ~5 s, and
/// then announces that it survived by creating the file given as `$1`.
///
/// A script on disk rather than an inline `sh -c`: the grandchild case needs a
/// script that spawns a script, and expressing that through a git alias inline
/// means four levels of nested quoting, which is how a test ends up silently
/// running something other than what it says.
///
/// The delay is `sleep 1` five times rather than one `sleep 5` on purpose. A
/// group `SIGTERM` kills the `sleep`, and a trapping shell would then fall
/// straight through to the next command — so a single sleep would let the marker
/// appear within milliseconds of the signal and the test would assert a race
/// rather than a kill. Looping means killing one `sleep` only advances the shell
/// by a second.
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

/// A script that backgrounds another script and then waits for it.
///
/// This makes the *direct* child an ordinary `SIGTERM` casualty while a
/// grandchild ignores the signal. The `wait` keeps it — and therefore `git` —
/// alive until the deadline, so the job times out rather than exiting.
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
    // `git` runs the alias in a child process, so the tree is
    // git -> stubborn.sh -> sleep, all in one process group. The script ignores
    // SIGTERM; only SIGKILL ends it.
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
    // Bounded by the deadline plus at most the 2 s grace, never by the child.
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?} to give up");

    // Wait past when the script would have finished. The marker is how a
    // survivor announces itself.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(!survived.exists(), "the SIGTERM-ignoring child outlived the job: it was not reaped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grandchild_that_outlives_its_parent_is_still_reaped() {
    // The real shape of the problem: `git fetch` spawns `ssh`, and signalling
    // only the direct child orphans a live connection.
    //
    // The direct child dies on SIGTERM immediately while a grandchild ignores
    // it. Nothing about the direct child's fate says anything about the group's,
    // which is why the kill targets `-pgid` and why SIGKILL is unconditional
    // rather than skipped once the child is gone.
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
    // Output produced before the deadline is kept: a transcript that discards
    // everything on timeout answers no questions.
    assert!(out.log.iter().any(|l| l.text == "working"), "{:?}", out.log);
    // And the transcript explains itself rather than just ending.
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
///
/// Local and offline: the whole suite must run with no network.
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
    // The single most valuable test in the crate.
    //
    // Without the hardening, a `git fetch` against a remote that asks for
    // credentials waits on a terminal read. Not slowly — forever, and silently,
    // and one such repository is enough to make a batch of forty never finish.
    // Because it never errors, nothing else in the system can detect it.
    //
    // Asserting the *message* and not just the timing is the point: `terminal
    // prompts disabled` is proof that GIT_TERMINAL_PROMPT reached the child and
    // took effect, rather than the connection merely having failed for some
    // other reason.
    let port = spawn_401_server();
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo(tmp.path());
    let url = format!("http://127.0.0.1:{port}/needs-auth.git");

    let started = Instant::now();
    let out = GitCommand::new(&repo)
        .args(["fetch", &url])
        // Generous on purpose: if the hardening failed, this would sit here for
        // the full ten seconds rather than failing in under one.
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
    // The probe's path. Same environment and same deadline; stdout stays bytes,
    // because `git status -z` emits paths that need not be UTF-8 and decoding
    // them would corrupt the thing being parsed.
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
    // A plan sheet shows the exact argv, and a transcript wants a header.
    let cmd = GitCommand::new("/tmp").args(["fetch", "--prune", "origin"]);
    assert_eq!(cmd.argv(), vec!["git", "fetch", "--prune", "origin"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unroutable_remote_is_bounded_by_the_deadline_and_nothing_else() {
    // An unreachable remote should fail in under two seconds. Measured on
    // macOS against TEST-NET-2, a *silently dropped* route
    // takes 4 s, 7 s, 11 s — and once, 75 s — and `http.connectTimeout` is not
    // honoured by this git (its own error reports "after 4094 ms" with the
    // option set). Nothing in this tool can shorten the OS TCP handshake.
    //
    // So the guarantee is the deadline, not a bound on git: the job is killed,
    // its process group with it, and the batch moves on. That is what "no hang,
    // ever" actually rests on — and the 75 s sample is why it has to.
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
    // Either git gave up first or we killed it; either way it is bounded.
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_connection_fails_immediately() {
    // The common shape of "unreachable" — a host that answers, refusing — and
    // the one where the two-second criterion is met with room to spare. Port 1
    // on loopback needs no network at all.
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
