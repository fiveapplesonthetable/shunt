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
            .env("TACHO_BIN", &self.tacho_bin);
        c
    }
}

impl Drop for TestSandbox {
    fn drop(&mut self) {
        let pid_file = self.runtime.join("daemon.pid");
        if let Ok(s) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = s.trim().parse::<i32>() {
                if pid > 0 {
                    unsafe {
                        let _ = libc::kill(pid, libc::SIGTERM);
                        let cap = std::time::Instant::now() + std::time::Duration::from_secs(2);
                        while libc::kill(pid, 0) == 0 && std::time::Instant::now() < cap {
                            std::thread::yield_now();
                        }
                        let _ = libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
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
