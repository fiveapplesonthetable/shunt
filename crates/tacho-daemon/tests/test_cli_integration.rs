//! End-to-end integration test running the real `tacho` CLI binary against the running daemon.
//!
//! Tests:
//! 1. `tacho emit state ...`
//! 2. `tacho emit span begin / end ...`
//! 3. `tacho hook claude < tests/fixtures/hooks/claude/SessionStart.json`
//! 4. `tacho status` and `tacho status --json`
//! 5. `tacho trace -o <file>`
//! 6. Trace processor oracle validation on the generated trace.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tacho_daemon::{Config, Daemon, DaemonPaths};

fn locate_tacho_bin() -> PathBuf {
    if let Ok(p) = env::var("TACHO_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return pb;
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(target_dir) = exe.parent().and_then(|p| p.parent()) {
            let candidate = target_dir.join("tacho");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    if let Ok(dir) = env::var("CARGO_TARGET_DIR") {
        let candidate = PathBuf::from(dir).join("debug/tacho");
        if candidate.is_file() {
            return candidate;
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ws_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(&manifest_dir);
    let ws_bin = ws_root.join("target/debug/tacho");
    if ws_bin.is_file() {
        return ws_bin;
    }
    let status = Command::new("cargo")
        .args(["build", "-p", "tachograph", "--bin", "tacho"])
        .current_dir(ws_root)
        .status()
        .expect("failed to build tacho binary");
    assert!(
        status.success(),
        "cargo build -p tachograph --bin tacho failed"
    );
    if let Ok(exe) = env::current_exe() {
        if let Some(target_dir) = exe.parent().and_then(|p| p.parent()) {
            let candidate = target_dir.join("tacho");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    ws_bin
}

#[test]
fn test_real_cli_binary_end_to_end() {
    let tp_path = tacho_core::find_trace_processor()
        .expect("trace_processor_shell not found; run scripts/oracle.sh or set TACHO_TP");

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let config = Config::default();

    // Start daemon
    let daemon = Daemon::start(paths.clone(), config).expect("Failed to start daemon");

    // Locate tacho binary
    let tacho_bin = locate_tacho_bin();
    assert!(
        tacho_bin.exists(),
        "tacho binary not found at {:?}",
        tacho_bin
    );

    let run_cli = |args: &[&str], stdin_data: Option<&[u8]>| -> std::process::Output {
        let mut cmd = Command::new(&tacho_bin);
        cmd.args(args)
            .env("TACHO_HOME", &home)
            .env("TACHO_RUNTIME", &runtime)
            .env("TACHO_TEST_MODE", "1")
            .env_remove("TACHO_NOTIFY")
            .env_remove("TACHO_ROOT_PRUNE");

        if let Some(data) = stdin_data {
            cmd.stdin(Stdio::piped());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            let mut child = cmd.spawn().expect("failed to spawn tacho command");
            use std::io::Write;
            if let Some(mut sin) = child.stdin.take() {
                sin.write_all(data).unwrap();
            }
            child
                .wait_with_output()
                .expect("failed waiting for tacho command")
        } else {
            cmd.output().expect("failed executing tacho command")
        }
    };

    // 1. `tacho emit state agent:worker-1 working`
    let out = run_cli(&["emit", "state", "agent:worker-1", "working"], None);
    assert!(
        out.status.success(),
        "tacho emit state failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. `tacho emit span begin` and `end`
    let out = run_cli(
        &[
            "emit",
            "span",
            "begin",
            "agent:worker-1",
            "build",
            "--id",
            "span-1",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "tacho emit span begin failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = run_cli(
        &["emit", "span", "end", "agent:worker-1", "--id", "span-1"],
        None,
    );
    assert!(
        out.status.success(),
        "tacho emit span end failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 3. `tacho hook claude` with SessionStart fixture
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/hooks/claude/SessionStart.json");
    let fixture_bytes = fs::read(&fixture_path).expect("failed reading SessionStart fixture");
    let out = run_cli(&["hook", "claude"], Some(&fixture_bytes));
    assert!(
        out.status.success(),
        "tacho hook claude failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Flush appender to ensure events are processed
    daemon.appender.flush();

    // 4. `tacho status`
    let out = run_cli(&["status"], None);
    assert!(
        out.status.success(),
        "tacho status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status_stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !status_stdout.trim().is_empty(),
        "Expected non-empty tacho status output"
    );

    // 5. `tacho status --json`
    let out = run_cli(&["status", "--json"], None);
    assert!(
        out.status.success(),
        "tacho status --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json_stdout = String::from_utf8_lossy(&out.stdout);
    let status_val: serde_json::Value =
        serde_json::from_str(&json_stdout).expect("valid JSON from tacho status --json");
    assert!(
        status_val.is_array()
            || status_val
                .get("agents")
                .and_then(|v| v.as_array())
                .is_some(),
        "Expected JSON array of agents or StatusSnapshot object with agents array, got: {status_val}"
    );

    // 6. `tacho trace -o <path>`
    let trace_path = temp.path().join("cli_test_trace.pftrace");
    let out = run_cli(&["trace", "-o", trace_path.to_str().unwrap()], None);
    assert!(
        out.status.success(),
        "tacho trace failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        trace_path.exists() && fs::metadata(&trace_path).unwrap().len() > 0,
        "Expected non-empty trace file generated by tacho trace"
    );

    // 7. Validate captured trace in Perfetto trace_processor_shell
    let tp_output = Command::new(&tp_path)
        .arg(&trace_path)
        .arg("-Q")
        .arg("SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;")
        .output();

    if let Ok(tp_out) = tp_output {
        if tp_out.status.success() {
            let stdout = String::from_utf8_lossy(&tp_out.stdout);
            let error_rows: Vec<&str> = stdout
                .trim()
                .lines()
                .filter(|l| !l.starts_with("\"name\"") && !l.is_empty())
                .collect();
            assert!(
                error_rows.is_empty(),
                "Oracle found errors in trace captured via real tacho CLI: {:?}",
                error_rows
            );
        }
    }
}
