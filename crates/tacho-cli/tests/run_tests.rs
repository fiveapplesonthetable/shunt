//! Tests for `tacho run -- <cmd>`.
//!
//! Verifies exit code pass-through, signal exit codes (128 + n), and stdout teeing.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct TestSandbox {
    _temp: tempfile::TempDir,
    home: PathBuf,
    runtime: PathBuf,
    tacho_bin: PathBuf,
}

impl TestSandbox {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let runtime = temp.path().join("run");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        Self {
            _temp: temp,
            home,
            runtime,
            tacho_bin: PathBuf::from(env!("CARGO_BIN_EXE_tacho")),
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.tacho_bin);
        c.env("TACHO_HOME", &self.home)
            .env("TACHO_RUNTIME", &self.runtime)
            .env("TACHO_BIN", &self.tacho_bin)
            .env("TACHO_TEST_MODE", "1")
            .env_remove("TACHO_NOTIFY")
            .env_remove("TACHO_ROOT_PRUNE");
        c
    }
}

impl Drop for TestSandbox {
    fn drop(&mut self) {
        let pid_file = self.runtime.join("daemon.pid");
        let Ok(s) = fs::read_to_string(&pid_file) else {
            return;
        };
        let pid = match s.trim().parse::<i32>() {
            Ok(pid) if pid > 0 => pid,
            _ => return,
        };
        if let Err(e) = stop_and_wait(pid) {
            let msg = format!("failed to stop test daemon pid {pid}: {e}");
            if std::thread::panicking() {
                eprintln!("{msg}");
            } else {
                panic!("{msg}");
            }
        }
    }
}

/// SIGTERM `pid` and block until it has exited, via a pidfd (no polling, no inotify).
///
/// Fails loudly rather than skipping the wait: a daemon left running leaks into later tests.
fn stop_and_wait(pid: i32) -> std::io::Result<()> {
    // SAFETY: plain syscalls on an fd this function owns and closes.
    unsafe {
        let pidfd = libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int;
        if pidfd < 0 {
            let err = std::io::Error::last_os_error();
            // Already gone: nothing to wait for.
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(std::io::Error::other(format!("pidfd_open: {err}")));
        }
        let result = (|| {
            if libc::kill(pid, libc::SIGTERM) != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    return Err(std::io::Error::other(format!("kill: {err}")));
                }
            }
            loop {
                let mut pfd = libc::pollfd {
                    fd: pidfd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if libc::poll(&mut pfd, 1, -1) >= 0 {
                    return Ok(());
                }
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(std::io::Error::other(format!("poll(pidfd): {err}")));
                }
            }
        })();
        libc::close(pidfd);
        result
    }
}

#[test]
fn test_run_exit_code_passthrough() {
    let sb = TestSandbox::new();

    // 1. Success exit code (0)
    let status = sb
        .cmd()
        .args(["run", "--", "sh", "-c", "exit 0"])
        .status()
        .expect("failed to run tacho run");
    assert_eq!(status.code(), Some(0));

    // 2. Specific non-zero exit code (42)
    let status42 = sb
        .cmd()
        .args(["run", "--", "sh", "-c", "exit 42"])
        .status()
        .expect("failed to run tacho run");
    assert_eq!(status42.code(), Some(42));
}

#[test]
fn test_run_signal_exit_code_128_plus_sig() {
    let sb = TestSandbox::new();

    // SIGKILL (9) -> 128 + 9 = 137
    let status_sigkill = sb
        .cmd()
        .args(["run", "--", "sh", "-c", "kill -9 $$"])
        .status()
        .expect("failed to run tacho run");
    assert_eq!(status_sigkill.code(), Some(137));

    // SIGTERM (15) -> 128 + 15 = 143
    let status_sigterm = sb
        .cmd()
        .args(["run", "--", "sh", "-c", "kill -15 $$"])
        .status()
        .expect("failed to run tacho run");
    assert_eq!(status_sigterm.code(), Some(143));
}

#[test]
fn test_run_stdout_teeing() {
    let sb = TestSandbox::new();

    let output = sb
        .cmd()
        .args(["run", "--", "echo", "hello tachograph flight recorder"])
        .output()
        .expect("failed to run tacho run");

    assert!(output.status.success());
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout_str.trim(), "hello tachograph flight recorder");
}

#[test]
fn test_run_binary_non_utf8_output() {
    let sb = TestSandbox::new();

    // 1. Pure binary non-UTF8 output (POSIX octal escapes work under both dash and bash)
    let output1 = sb
        .cmd()
        .args([
            "run",
            "--",
            "sh",
            "-c",
            "printf 'hello\\377\\376\\000world'",
        ])
        .output()
        .expect("failed to run tacho run");
    assert!(output1.status.success());
    assert_eq!(output1.stdout, b"hello\xff\xfe\x00world");

    // 2. Mixed lines: line1, binary bytes, line2
    let output2 = sb
        .cmd()
        .args([
            "run",
            "--",
            "sh",
            "-c",
            "echo 'line1'; printf '\\377\\376\\000'; echo 'line2'",
        ])
        .output()
        .expect("failed to run tacho run");
    assert!(output2.status.success());
    assert_eq!(output2.stdout, b"line1\n\xff\xfe\x00line2\n");
}
