use crate::GRACE;
use tokio::process::Child;

/// Signal the whole process group, then make sure nothing survived.
///
/// The negative pid is the entire point. `git fetch` spawns `ssh`; `git push`
/// spawns credential helpers; a hook spawns whatever it likes. Signalling only
/// the direct child leaves those running, holding connections and pipes.
///
/// Order: `SIGTERM` the group, give the *direct child* up to [`GRACE`] to exit
/// cleanly, then `SIGKILL` the group unconditionally. That last word matters:
/// a child that dies promptly on `SIGTERM` proves nothing about its children,
/// and the case this crate exists for is exactly the grandchild that ignores
/// `SIGTERM`. `SIGKILL` to an already-empty group fails with `ESRCH` and is
/// harmless.
pub async fn terminate_group(pgid: i32, child: &mut Child) {
    signal_group(pgid, libc::SIGTERM);
    // Draining continues throughout: the reader tasks own the pipes, so a
    // chatty child can keep writing while it winds down.
    let _ = tokio::time::timeout(GRACE, child.wait()).await;
    signal_group(pgid, libc::SIGKILL);
}

#[allow(unsafe_code, reason = "signalling a process group has no safe std equivalent")]
fn signal_group(pgid: i32, signal: i32) {
    // A pgid of 0 would mean "my own group" — i.e. kill the whole application,
    // including the GUI. It cannot happen (`setpgid(0, 0)` makes pgid == pid,
    // and no child has pid 0) but the consequence is severe enough to check.
    if pgid <= 0 {
        tracing::error!(pgid, "refusing to signal a non-positive process group");
        return;
    }
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc != 0 {
        // ESRCH just means everything already exited, which is the common case.
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(pgid, signal, %err, "could not signal process group");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn refuses_to_signal_its_own_group() {
        // No assertion beyond "this returns instead of killing the test runner".
        // `kill(-0, SIGKILL)` would signal every process in our own group.
        super::signal_group(0, libc::SIGKILL);
        super::signal_group(-1, libc::SIGKILL);
    }
}
