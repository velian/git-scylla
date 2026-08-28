use crate::GRACE;
use tokio::process::Child;

/// Signal the whole process group: `SIGTERM`, up to [`GRACE`] for the direct
/// child to exit, then `SIGKILL` unconditionally.
pub async fn terminate_group(pgid: i32, child: &mut Child) {
    signal_group(pgid, libc::SIGTERM);
    let _ = tokio::time::timeout(GRACE, child.wait()).await;
    signal_group(pgid, libc::SIGKILL);
}

#[allow(unsafe_code, reason = "signalling a process group has no safe std equivalent")]
fn signal_group(pgid: i32, signal: i32) {
    // `kill(-0, …)` would signal every process in the caller's own group.
    if pgid <= 0 {
        tracing::error!(pgid, "refusing to signal a non-positive process group");
        return;
    }
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc != 0 {
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
        super::signal_group(0, libc::SIGKILL);
        super::signal_group(-1, libc::SIGKILL);
    }
}
