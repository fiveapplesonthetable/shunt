//! The one set of process-identity checks the plan executor (and every CLI command that
//! signals a process) uses.
//!
//! Rules:
//! - A pid is only ever signalled when a strong identity was recorded for it: `starttime > 0`
//!   (field 22 of `/proc/<pid>/stat`) and/or a non-empty cgroup. Never cmdline/environ.
//! - Single processes are pinned with a pidfd before their identity is checked, and the signal
//!   is sent through that pidfd, so a pid recycled between check and signal cannot be hit.
//! - `ESRCH` always means "already gone", which is not an error.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Recorded identity of a process the fleet started or observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity<'a> {
    pub pid: u32,
    pub starttime: Option<u64>,
    pub cgroup: Option<&'a str>,
}

impl Identity<'_> {
    /// Whether a strong identity was recorded at all.
    pub fn is_recorded(&self) -> bool {
        self.starttime.is_some_and(|st| st > 0) || self.cgroup.is_some_and(|cg| !cg.is_empty())
    }
}

/// Outcome of [`signal_verified`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signalled {
    /// The signal was delivered to the verified process.
    Delivered,
    /// The process had already exited (`ESRCH`), or the pid now names a different process.
    Gone,
    /// No strong identity was recorded, or a live process failed verification: not signalled.
    Refused,
}

/// Returns `true` if `pid` exists, `/proc/<pid>/stat` is readable, and the process is not a
/// zombie. Fails closed.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill(2) with signal 0 only checks existence/permission.
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    proc_pid_state(pid).is_some_and(|s| s != 'Z')
}

/// `true` if `pid` is alive and its starttime equals `expected_starttime` (which must be > 0).
pub fn pid_alive_matching(pid: u32, expected_starttime: Option<u64>) -> bool {
    let Some(expected) = expected_starttime.filter(|&st| st > 0) else {
        return false;
    };
    pid_alive(pid) && proc_pid_starttime(pid) == Some(expected)
}

/// Field 22 (`starttime`, clock ticks since boot) of `/proc/<pid>/stat`.
pub fn proc_pid_starttime(pid: u32) -> Option<u64> {
    let content = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = content.rfind(')')?;
    content
        .get(rparen + 2..)?
        .split_whitespace()
        .nth(19)
        .and_then(|s| s.parse::<u64>().ok())
}

/// `/proc/<pid>/cgroup`, trimmed; `None` when unreadable or empty.
pub fn proc_pid_cgroup(pid: u32) -> Option<String> {
    let content = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let trimmed = content.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Field 3 (state letter) of `/proc/<pid>/stat`.
pub fn proc_pid_state(pid: u32) -> Option<char> {
    let content = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = content.rfind(')')?;
    content[rparen + 1..].trim_start().chars().next()
}

/// Verifies that the live process at `id.pid` is the one recorded for `label`.
///
/// Requires a strong identity (see module docs); logs why a check refused.
pub fn verify_pid(id: &Identity<'_>, label: &str) -> bool {
    let pid = id.pid;
    if !id.is_recorded() {
        eprintln!(
            "[tacho-safety] PID {pid} for '{label}' has neither recorded starttime nor cgroup; refusing signal without identity"
        );
        return false;
    }
    if !pid_alive(pid) {
        return false;
    }
    let Some(actual_st) = proc_pid_starttime(pid) else {
        return false;
    };
    if let Some(expected) = id.starttime {
        if expected == 0 || actual_st != expected {
            eprintln!(
                "[tacho-safety] PID {pid} starttime ({actual_st}) does not match recorded ({expected}); refusing signal to reused PID"
            );
            return false;
        }
    }
    if let Some(cg) = id.cgroup.filter(|s| !s.is_empty()) {
        let Ok(actual_cg) = fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
            return false;
        };
        if !actual_cg.contains(cg) {
            eprintln!(
                "[tacho-safety] PID {pid} cgroup does not contain expected '{cg}' for '{label}'; refusing signal"
            );
            return false;
        }
    }
    true
}

fn pidfd_open(pid: u32) -> io::Result<Option<OwnedFd>> {
    // SAFETY: plain syscall; the result is checked before being wrapped.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if raw < 0 {
        let err = io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ESRCH) => Ok(None),
            _ => Err(io::Error::new(
                err.kind(),
                format!("pidfd_open({pid}) failed: {err}"),
            )),
        };
    }
    // SAFETY: pidfd_open returned a fresh fd that we exclusively own.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(raw as i32) }))
}

/// Signals the process recorded as `id` after pinning it with a pidfd and verifying identity.
///
/// `Err` only for real failures (e.g. `EPERM`); "gone" and "refused" are outcomes.
pub fn signal_verified(id: &Identity<'_>, label: &str, sig: libc::c_int) -> io::Result<Signalled> {
    if !id.is_recorded() {
        eprintln!(
            "[tacho-safety] PID {} for '{label}' has no recorded identity; refusing signal",
            id.pid
        );
        return Ok(Signalled::Refused);
    }
    if id.pid <= 1 {
        return Ok(Signalled::Refused);
    }
    let Some(pidfd) = pidfd_open(id.pid)? else {
        return Ok(Signalled::Gone);
    };
    if !pid_alive(id.pid) {
        return Ok(Signalled::Gone);
    }
    if let Some(expected) = id.starttime.filter(|&st| st > 0) {
        if proc_pid_starttime(id.pid).is_some_and(|actual| actual != expected) {
            // The pid now names a different process, so the recorded one has exited.
            return Ok(Signalled::Gone);
        }
    }
    if !verify_pid(id, label) {
        return Ok(Signalled::Refused);
    }
    // SAFETY: valid pidfd; a null siginfo means "as if sent by kill(2)".
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc == 0 {
        return Ok(Signalled::Delivered);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => Ok(Signalled::Gone),
        _ => Err(io::Error::new(
            err.kind(),
            format!("pidfd_send_signal({}, {sig}) failed: {err}", id.pid),
        )),
    }
}

/// Sends `sig` with kill(2) (negative `pid` = process group). `Ok(false)` on `ESRCH`.
///
/// Only for targets whose identity the caller already verified (process groups cannot be
/// pinned with a pidfd).
pub fn send_signal(pid: i32, sig: libc::c_int) -> io::Result<bool> {
    // SAFETY: kill(2) has no memory-safety preconditions.
    if unsafe { libc::kill(pid, sig) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(io::Error::new(
            err.kind(),
            format!("kill({pid}, {sig}) failed: {err}"),
        ))
    }
}

/// What is known about the process recorded for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentProcess {
    /// No process recorded, or the recorded one has exited (or the pid was reused).
    Gone,
    /// Verified and job-control stopped.
    Stopped(u32),
    /// Verified and running.
    Running,
    /// Alive at the recorded pid but no identity proves it is ours.
    Unverified,
}

/// Classifies the process recorded for `label` without signalling it.
pub fn classify_agent_process(
    pid: Option<u32>,
    label: &str,
    cgroup: Option<&str>,
    starttime: Option<u64>,
) -> AgentProcess {
    let Some(pid) = pid else {
        return AgentProcess::Gone;
    };
    if !pid_alive(pid) {
        return AgentProcess::Gone;
    }
    if let Some(expected) = starttime.filter(|&st| st > 0) {
        if proc_pid_starttime(pid).is_some_and(|actual| actual != expected) {
            return AgentProcess::Gone;
        }
    }
    let id = Identity {
        pid,
        starttime,
        cgroup,
    };
    if !verify_pid(&id, label) {
        return AgentProcess::Unverified;
    }
    if proc_pid_state(pid).is_some_and(|s| s == 'T' || s == 't') {
        AgentProcess::Stopped(pid)
    } else {
        AgentProcess::Running
    }
}

/// Blocks until `pid` exits, waiting on its pidfd (no timer). Returns at once if it is gone.
pub fn wait_for_pid_exit_event(pid: u32) -> io::Result<()> {
    let Some(pidfd) = pidfd_open(pid)? else {
        return Ok(());
    };
    let mut pfd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `pfd` is a valid pollfd for the duration of the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(io::Error::new(
                err.kind(),
                format!("poll on pidfd for {pid} failed: {err}"),
            ));
        }
    }
}

/// SIGKILLs the process group of a verified, stopped agent and blocks until its leader exits.
pub fn kill_stopped_and_wait(pid: u32) -> io::Result<()> {
    send_signal(-(pid as i32), libc::SIGKILL)?;
    wait_for_pid_exit_event(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_verified_requires_identity_and_matching_starttime() {
        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let st = proc_pid_starttime(pid).unwrap();
        let id = |starttime| Identity {
            pid,
            starttime,
            cgroup: None,
        };
        assert_eq!(
            signal_verified(&id(None), "t", libc::SIGKILL).unwrap(),
            Signalled::Refused
        );
        assert_eq!(
            signal_verified(&id(Some(0)), "t", libc::SIGKILL).unwrap(),
            Signalled::Refused
        );
        assert_eq!(
            signal_verified(&id(Some(st + 1)), "t", libc::SIGKILL).unwrap(),
            Signalled::Gone,
            "a different starttime means the recorded process is gone"
        );
        assert!(child.try_wait().unwrap().is_none(), "nothing was signalled");
        assert_eq!(
            signal_verified(&id(Some(st)), "t", libc::SIGKILL).unwrap(),
            Signalled::Delivered
        );
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert_eq!(
            signal_verified(&id(Some(st)), "t", libc::SIGKILL).unwrap(),
            Signalled::Gone,
            "ESRCH is gone, not an error"
        );
    }
}
