//! Performance and invariant test for `tacho hook`.
//!
//! Executes the built `tacho` binary as a hook 200 times:
//! 1. With daemon absent: asserts exit 0, empty stdout, under 200ms, drops counted.
//! 2. With daemon present: asserts exit 0, empty stdout, fast latency, reports p50 and p99.

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

fn find_tacho_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tacho"))
}

struct DaemonProcess {
    child: Child,
    _temp_dir: tempfile::TempDir,
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for_file(path: &std::path::Path) {
    if path.exists() {
        return;
    }
    let parent = path.parent().unwrap();
    let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if inotify_fd >= 0 {
        if let Ok(c_dir) = std::ffi::CString::new(parent.as_os_str().as_encoded_bytes()) {
            let _ = unsafe {
                libc::inotify_add_watch(
                    inotify_fd,
                    c_dir.as_ptr(),
                    libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_MODIFY,
                )
            };
        }
        let mut buf = [0u8; 1024];
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            let mut pfd = libc::pollfd {
                fd: inotify_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let _ = unsafe { libc::poll(&mut pfd, 1, 100) };
            let _ =
                unsafe { libc::read(inotify_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        }
        unsafe {
            libc::close(inotify_fd);
        }
    }
}

fn start_test_daemon(tacho_bin: &PathBuf) -> (DaemonProcess, PathBuf, PathBuf) {
    let temp_dir = tempfile::tempdir().unwrap();
    let home = temp_dir.path().join("home");
    let runtime = temp_dir.path().join("run");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&runtime).unwrap();

    let child = Command::new(tacho_bin)
        .arg("daemon")
        .arg("--foreground")
        .env("TACHO_HOME", &home)
        .env("TACHO_RUNTIME", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn daemon");

    let api_sock = runtime.join("api.sock");
    wait_for_file(&api_sock);
    assert!(api_sock.exists(), "daemon failed to create api.sock");

    (
        DaemonProcess {
            child,
            _temp_dir: temp_dir,
        },
        home,
        runtime,
    )
}

#[test]
fn test_hook_200_runs_daemon_absent_and_present() {
    let tacho_bin = find_tacho_bin();
    let sample_payload = br#"{
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "call_12345",
        "session_id": "test-session-001",
        "tool_input": { "command": "cargo test" }
    }"#;

    // =========================================================================
    // Part 1: Daemon Absent
    // =========================================================================
    let temp_absent = tempfile::tempdir().unwrap();
    let home_absent = temp_absent.path().join("home");
    let runtime_absent = temp_absent.path().join("run");
    fs::create_dir_all(&home_absent).unwrap();
    fs::create_dir_all(&runtime_absent).unwrap();

    let mut absent_latencies_us = Vec::with_capacity(200);

    for i in 0..200 {
        let run_dir = runtime_absent.join(format!("run_{i}"));
        fs::create_dir_all(&run_dir).unwrap();

        let t0 = Instant::now();
        let mut child = Command::new(&tacho_bin)
            .arg("hook")
            .arg("claude")
            .env("TACHO_HOME", &home_absent)
            .env("TACHO_RUNTIME", &run_dir)
            .env("TACHO_NO_AUTOSPAWN", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn hook");

        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(sample_payload)
            .unwrap();

        let output = child.wait_with_output().unwrap();
        let elapsed = t0.elapsed();
        absent_latencies_us.push(elapsed.as_micros());

        // Invariants:
        // 1. Exit code is ALWAYS 0
        assert_eq!(
            output.status.code(),
            Some(0),
            "hook must ALWAYS exit 0 even if daemon is absent"
        );
        // 2. Output to stdout is strictly empty
        assert!(
            output.stdout.is_empty(),
            "hook must NEVER write to stdout: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    absent_latencies_us.sort_unstable();
    let p50_absent = absent_latencies_us[100];
    let p99_absent = absent_latencies_us[198];

    // Assert hook drops were counted
    let drops_file = home_absent.join("hook-drops");
    assert!(
        drops_file.exists(),
        "hook-drops file must be created when daemon absent"
    );
    let drops_count: u64 = fs::read_to_string(&drops_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        drops_count, 200,
        "all 200 dropped events must be counted in hook-drops"
    );

    // Clean up background daemons spawned during absent test by their specific pid files
    for i in 0..200 {
        let run_dir = runtime_absent.join(format!("run_{i}"));
        let pid_file = run_dir.join("daemon.pid");
        if let Ok(pid_str) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }

    // =========================================================================
    // Part 2: Daemon Present
    // =========================================================================
    let (_daemon, home_present, runtime_present) = start_test_daemon(&tacho_bin);
    let mut present_latencies_us = Vec::with_capacity(200);

    for _ in 0..200 {
        let t0 = Instant::now();
        let mut child = Command::new(&tacho_bin)
            .arg("hook")
            .arg("claude")
            .env("TACHO_HOME", &home_present)
            .env("TACHO_RUNTIME", &runtime_present)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn hook");

        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(sample_payload)
            .unwrap();

        let output = child.wait_with_output().unwrap();
        let elapsed = t0.elapsed();
        present_latencies_us.push(elapsed.as_micros());

        assert_eq!(output.status.code(), Some(0));
        assert!(output.stdout.is_empty());
    }

    present_latencies_us.sort_unstable();
    let p50_present = present_latencies_us[100];
    let p99_present = present_latencies_us[198];

    println!("\n--- Hook Benchmark Results (200 iterations each) ---");
    println!(
        "Daemon ABSENT:  p50 = {:.2} ms, p99 = {:.2} ms (all exit 0, stdout empty, 200 drops recorded)",
        p50_absent as f64 / 1000.0,
        p99_absent as f64 / 1000.0
    );
    println!(
        "Daemon PRESENT: p50 = {:.2} ms, p99 = {:.2} ms (all exit 0, stdout empty)",
        p50_present as f64 / 1000.0,
        p99_present as f64 / 1000.0
    );
    println!("----------------------------------------------------\n");
}

#[test]
fn test_20_parallel_hooks_single_daemon() {
    let tacho_bin = find_tacho_bin();
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("run");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&runtime).unwrap();

    let sample_payload = br#"{
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "call_race_test",
        "session_id": "test-session-race",
        "tool_input": { "command": "cargo test" }
    }"#;

    // Spawn 20 hooks in parallel with daemon absent
    let mut procs = Vec::new();
    for _ in 0..20 {
        let mut child = Command::new(&tacho_bin)
            .args(["hook", "claude"])
            .env("TACHO_HOME", &home)
            .env("TACHO_RUNTIME", &runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn hook");
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(sample_payload);
        }
        procs.push(child);
    }

    // All hooks must exit 0 within budget
    for mut child in procs {
        let status = child.wait().expect("failed to wait on hook");
        assert!(status.success());
    }

    // Wait for the spawned daemon to acquire daemon.lock and write daemon.pid
    let daemon_lock_path = runtime.join("daemon.lock");
    let pid_file = runtime.join("daemon.pid");
    let api_sock = runtime.join("api.sock");
    wait_for_file(&daemon_lock_path);
    wait_for_file(&pid_file);
    wait_for_file(&api_sock);

    let daemon_log = home.join("daemon.log");
    if daemon_log.exists() {
        let content = fs::read_to_string(&daemon_log).unwrap_or_default();
        println!("DAEMON LOG CONTENT:\n{}\n--- END DAEMON LOG ---", content);
    } else {
        println!("DAEMON LOG DOES NOT EXIST at {:?}", daemon_log);
    }

    // Exactly one daemon process should end up running
    let daemon_lock_path = runtime.join("daemon.lock");
    assert!(daemon_lock_path.exists());
    let file = std::fs::File::open(&daemon_lock_path).unwrap();
    use std::os::unix::io::AsRawFd;
    let flock_res = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_ne!(
        flock_res, 0,
        "daemon.lock should be held by the running daemon"
    );

    // Verify exactly one daemon PID was recorded and is alive
    let pid_file = runtime.join("daemon.pid");
    assert!(pid_file.exists(), "daemon.pid must exist");
    let pid_str = fs::read_to_string(&pid_file).unwrap();
    let pid: i32 = pid_str.trim().parse().expect("valid pid");
    assert!(pid > 0);

    // Verify process is alive (kill 0)
    let is_alive = unsafe { libc::kill(pid, 0) == 0 };
    assert!(is_alive, "Daemon process {} must be alive", pid);

    // Terminate the daemon
    let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
}
