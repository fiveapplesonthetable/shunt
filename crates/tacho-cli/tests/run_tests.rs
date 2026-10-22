//! Tests for `tacho run -- <cmd>`.
//!
//! Verifies exit code pass-through, signal exit codes (128 + n), and stdout teeing.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn find_tacho_bin() -> PathBuf {
    if let Ok(dir) = env::var("CARGO_TARGET_DIR") {
        let p = PathBuf::from(dir).join("debug/tacho");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("/tmp/tacho-target-ring/debug/tacho")
}

#[test]
fn test_run_exit_code_passthrough() {
    let tacho_bin = find_tacho_bin();

    // 1. Success exit code (0)
    let status = Command::new(&tacho_bin)
        .args(["run", "--", "sh", "-c", "exit 0"])
        .status()
        .expect("failed to run tacho run");
    assert_eq!(status.code(), Some(0));

    // 2. Specific non-zero exit code (42)
    let status42 = Command::new(&tacho_bin)
        .args(["run", "--", "sh", "-c", "exit 42"])
        .status()
        .expect("failed to run tacho run");
    assert_eq!(status42.code(), Some(42));
}

#[test]
fn test_run_signal_exit_code_128_plus_sig() {
    let tacho_bin = find_tacho_bin();

    // SIGKILL (9) -> 128 + 9 = 137
    let status_sigkill = Command::new(&tacho_bin)
        .args(["run", "--", "sh", "-c", "kill -9 $$"])
        .status()
        .expect("failed to run tacho run");
    assert_eq!(status_sigkill.code(), Some(137));

    // SIGTERM (15) -> 128 + 15 = 143
    let status_sigterm = Command::new(&tacho_bin)
        .args(["run", "--", "sh", "-c", "kill -15 $$"])
        .status()
        .expect("failed to run tacho run");
    assert_eq!(status_sigterm.code(), Some(143));
}

#[test]
fn test_run_stdout_teeing() {
    let tacho_bin = find_tacho_bin();

    let output = Command::new(&tacho_bin)
        .args(["run", "--", "echo", "hello tachograph flight recorder"])
        .output()
        .expect("failed to run tacho run");

    assert!(output.status.success());
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout_str.trim(), "hello tachograph flight recorder");
}

#[test]
fn test_run_binary_non_utf8_output() {
    let tacho_bin = find_tacho_bin();

    // 1. Pure binary non-UTF8 output
    let output1 = Command::new(&tacho_bin)
        .args([
            "run",
            "--",
            "sh",
            "-c",
            "printf 'hello\\xff\\xfe\\x00world'",
        ])
        .output()
        .expect("failed to run tacho run");
    assert!(output1.status.success());
    assert_eq!(output1.stdout, b"hello\xff\xfe\x00world");

    // 2. Mixed lines: line1, binary bytes, line2
    let output2 = Command::new(&tacho_bin)
        .args([
            "run",
            "--",
            "sh",
            "-c",
            "echo 'line1'; printf '\\xff\\xfe\\x00'; echo 'line2'",
        ])
        .output()
        .expect("failed to run tacho run");
    assert!(output2.status.success());
    assert_eq!(output2.stdout, b"line1\n\xff\xfe\x00line2\n");
}
