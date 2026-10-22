//! Test suite for tachograph PerfettoSQL queries.
//!
//! Scripts a fleet directly through `tacho_core::Recorder` + `Encoder`,
//! runs all 9 queries in standard SQL and Pipe syntax, asserts exact expected rows,
//! and verifies that both variants return identical rows.
//!
//! When `TACHO_TEST_TRACED=1` is set, also runs real system tracing concatenation
//! to verify process tree and CPU/RSS attribution.

use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use tacho_core::{
    encode_snapshot_header, Encoder, FileEditKind, Input, Recorder, SessionKey, ToolInput, WaitOn,
};
use tachograph::query::get_embedded_query;

fn get_paired_clocks() -> (u64, u64) {
    let mut real_ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let mut boot_ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut real_ts);
        libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut boot_ts);
    }
    let real_ns = (real_ts.tv_sec as u64) * 1_000_000_000 + (real_ts.tv_nsec as u64);
    let boot_ns = (boot_ts.tv_sec as u64) * 1_000_000_000 + (boot_ts.tv_nsec as u64);
    (real_ns, boot_ns)
}

fn locate_trace_processor() -> PathBuf {
    tacho_core::find_trace_processor()
        .expect("trace_processor_shell binary not found; run scripts/oracle.sh or set TACHO_TP")
}

fn supports_pipe_syntax(tp: &Path, trace: &Path) -> bool {
    let mut child = match Command::new(tp)
        .arg(trace)
        .arg("-q")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"PERFETTO PRAGMA pipelines;\nSELECT 1;\n");
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

fn run_query(tp: &Path, trace: &Path, sql: &str) -> String {
    let mut child = Command::new(tp)
        .arg(trace)
        .arg("-q")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn trace_processor_shell");

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(sql.as_bytes())
            .expect("failed to write query to stdin");
    }
    let output = child
        .wait_with_output()
        .expect("failed to wait for trace_processor_shell");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "query failed:\nSQL:\n{}\nSTDOUT:\n{}\nSTDERR:\n{}",
        sql,
        stdout,
        stderr
    );
    stdout.to_string()
}

/// Helper to parse CSV output from trace_processor_shell into header and data rows.
fn parse_tp_csv(output: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let mut lines = output.lines();
    let mut header_line = None;
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed.starts_with("Trace loaded:") || trimmed.is_empty() {
            continue;
        }
        header_line = Some(trimmed.to_string());
        break;
    }

    let header_str = header_line.unwrap_or_default();
    let headers: Vec<String> = header_str
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .collect();

    let mut rows = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("Query executed in") || trimmed.is_empty() {
            continue;
        }
        let cols: Vec<String> = trimmed
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .collect();
        rows.push(cols);
    }
    (headers, rows)
}

#[test]
fn test_scripted_fleet_queries() {
    let tp = locate_trace_processor();
    let temp_dir = tempfile::tempdir().unwrap();
    let trace_path = temp_dir.path().join("fleet.pftrace");

    let base_ts: u64 = 1_700_000_000_000_000_000;
    let mut rec = Recorder::new();
    let mut events = Vec::new();

    let s1 = SessionKey::new("claude", "sess-1");
    let s2 = SessionKey::new("gemini", "sess-2");
    let s3 = SessionKey::new("jetski", "sess-3");

    // -------------------------------------------------------------
    // Agent 1: worker-1
    // -------------------------------------------------------------
    rec.apply(
        Input::RunStart {
            source_ts_ns: Some(base_ts),
            provider: "claude".to_string(),
            session_id: "sess-1".to_string(),
            agent: Some("worker-1".to_string()),
            task: Some("Build and test task".to_string()),
            pid: Some(1001),
            cwd: Some("/home/dev/project".to_string()),
            model: Some("claude-3-5".to_string()),
            parent_session: None,
            git_tree: None,
            role: None,
        },
        base_ts,
        &mut events,
    );

    rec.apply(
        Input::TurnStart {
            source_ts_ns: Some(base_ts + 1_000_000_000),
            session: s1.clone(),
            model: Some("claude-3-5".to_string()),
        },
        base_ts + 1_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::ToolBegin {
            source_ts_ns: Some(base_ts + 2_000_000_000),
            session: s1.clone(),
            tool_use_id: "call-1".to_string(),
            tool_name: "Bash".to_string(),
            input: ToolInput::Shell {
                argv: "cargo build --release".to_string(),
                cwd: None,
                git_tree: None,
            },
        },
        base_ts + 2_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::ToolEnd {
            source_ts_ns: Some(base_ts + 12_000_000_000),
            session: s1.clone(),
            tool_use_id: "call-1".to_string(),
            error: false,
            result_bytes: Some(1024),
            exit_code: Some(0),
        },
        base_ts + 12_000_000_000,
        &mut events,
    );

    // Failing test loop: 3 test failures
    for i in 0..3 {
        let t_begin = base_ts + (13 + i * 6) * 1_000_000_000;
        let t_end = base_ts + (18 + i * 6) * 1_000_000_000;
        let cid = format!("call-test-{}", i + 1);

        rec.apply(
            Input::ToolBegin {
                source_ts_ns: Some(t_begin),
                session: s1.clone(),
                tool_use_id: cid.clone(),
                tool_name: "Bash".to_string(),
                input: ToolInput::Shell {
                    argv: "cargo test -p core".to_string(),
                    cwd: None,
                    git_tree: None,
                },
            },
            t_begin,
            &mut events,
        );

        rec.apply(
            Input::ToolEnd {
                source_ts_ns: Some(t_end),
                session: s1.clone(),
                tool_use_id: cid,
                error: true,
                result_bytes: Some(256),
                exit_code: Some(101),
            },
            t_end,
            &mut events,
        );
    }

    // File edit: src/main.rs (+10, -2)
    rec.apply(
        Input::ToolBegin {
            source_ts_ns: Some(base_ts + 31_000_000_000),
            session: s1.clone(),
            tool_use_id: "call-edit-1".to_string(),
            tool_name: "Edit".to_string(),
            input: ToolInput::FileEdit {
                path: "src/main.rs".to_string(),
                kind: Some(FileEditKind::Edit),
                lines_added: Some(10),
                lines_removed: Some(2),
            },
        },
        base_ts + 31_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::ToolEnd {
            source_ts_ns: Some(base_ts + 32_000_000_000),
            session: s1.clone(),
            tool_use_id: "call-edit-1".to_string(),
            error: false,
            result_bytes: Some(100),
            exit_code: None,
        },
        base_ts + 32_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::TurnEnd {
            source_ts_ns: Some(base_ts + 33_000_000_000),
            session: s1.clone(),
            input_tokens: Some(5000),
            output_tokens: Some(800),
            cached_tokens: Some(1000),
            text_bytes: Some(2048),
            cost_micros: Some(1500),
        },
        base_ts + 33_000_000_000,
        &mut events,
    );

    // -------------------------------------------------------------
    // Agent 2: worker-2
    // -------------------------------------------------------------
    rec.apply(
        Input::RunStart {
            source_ts_ns: Some(base_ts),
            provider: "gemini".to_string(),
            session_id: "sess-2".to_string(),
            agent: Some("worker-2".to_string()),
            task: Some("Refactoring task".to_string()),
            pid: Some(1002),
            cwd: Some("/home/dev/project".to_string()),
            model: Some("gemini-2.5".to_string()),
            parent_session: None,
            git_tree: None,
            role: None,
        },
        base_ts,
        &mut events,
    );

    rec.apply(
        Input::TurnStart {
            source_ts_ns: Some(base_ts + 5_000_000_000),
            session: s2.clone(),
            model: Some("gemini-2.5".to_string()),
        },
        base_ts + 5_000_000_000,
        &mut events,
    );

    // Edit src/main.rs (+15, -5)
    rec.apply(
        Input::ToolBegin {
            source_ts_ns: Some(base_ts + 6_000_000_000),
            session: s2.clone(),
            tool_use_id: "w2-call-1".to_string(),
            tool_name: "Edit".to_string(),
            input: ToolInput::FileEdit {
                path: "src/main.rs".to_string(),
                kind: Some(FileEditKind::Edit),
                lines_added: Some(15),
                lines_removed: Some(5),
            },
        },
        base_ts + 6_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::ToolEnd {
            source_ts_ns: Some(base_ts + 7_000_000_000),
            session: s2.clone(),
            tool_use_id: "w2-call-1".to_string(),
            error: false,
            result_bytes: Some(100),
            exit_code: None,
        },
        base_ts + 7_000_000_000,
        &mut events,
    );

    // Edit src/lib.rs (+40, -0)
    rec.apply(
        Input::ToolBegin {
            source_ts_ns: Some(base_ts + 8_000_000_000),
            session: s2.clone(),
            tool_use_id: "w2-call-2".to_string(),
            tool_name: "Edit".to_string(),
            input: ToolInput::FileEdit {
                path: "src/lib.rs".to_string(),
                kind: Some(FileEditKind::Create),
                lines_added: Some(40),
                lines_removed: Some(0),
            },
        },
        base_ts + 8_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::ToolEnd {
            source_ts_ns: Some(base_ts + 9_000_000_000),
            session: s2.clone(),
            tool_use_id: "w2-call-2".to_string(),
            error: false,
            result_bytes: Some(200),
            exit_code: None,
        },
        base_ts + 9_000_000_000,
        &mut events,
    );

    // 4 FileReads of src/lib.rs triggering LOOP detector
    for i in 0..4 {
        let t_begin = base_ts + 9_000_000_000 + i * 1_000_000_000;
        let t_end = base_ts + 9_500_000_000 + i * 1_000_000_000;
        let cid = format!("w2-read-{}", i + 1);

        rec.apply(
            Input::ToolBegin {
                source_ts_ns: Some(t_begin),
                session: s2.clone(),
                tool_use_id: cid.clone(),
                tool_name: "Read".to_string(),
                input: ToolInput::FileRead {
                    path: "src/lib.rs".to_string(),
                },
            },
            t_begin,
            &mut events,
        );

        rec.apply(
            Input::ToolEnd {
                source_ts_ns: Some(t_end),
                session: s2.clone(),
                tool_use_id: cid,
                error: false,
                result_bytes: Some(500),
                exit_code: None,
            },
            t_end,
            &mut events,
        );
    }

    // Build command overlapping worker-1 and worker-3 (3-way contention!)
    rec.apply(
        Input::ToolBegin {
            source_ts_ns: Some(base_ts + 14_000_000_000),
            session: s2.clone(),
            tool_use_id: "w2-call-check".to_string(),
            tool_name: "Bash".to_string(),
            input: ToolInput::Shell {
                argv: "cargo check".to_string(),
                cwd: None,
                git_tree: None,
            },
        },
        base_ts + 14_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::ToolEnd {
            source_ts_ns: Some(base_ts + 16_000_000_000),
            session: s2.clone(),
            tool_use_id: "w2-call-check".to_string(),
            error: false,
            result_bytes: Some(1024),
            exit_code: Some(0),
        },
        base_ts + 16_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::TurnEnd {
            source_ts_ns: Some(base_ts + 20_000_000_000),
            session: s2.clone(),
            input_tokens: Some(8000),
            output_tokens: Some(1200),
            cached_tokens: Some(3000),
            text_bytes: Some(4096),
            cost_micros: Some(2500),
        },
        base_ts + 20_000_000_000,
        &mut events,
    );

    // -------------------------------------------------------------
    // Agent 3: worker-3
    // -------------------------------------------------------------
    rec.apply(
        Input::RunStart {
            source_ts_ns: Some(base_ts),
            provider: "jetski".to_string(),
            session_id: "sess-3".to_string(),
            agent: Some("worker-3".to_string()),
            task: Some("Verification task".to_string()),
            pid: Some(1003),
            cwd: Some("/home/dev/project".to_string()),
            model: Some("jetski-1".to_string()),
            parent_session: None,
            git_tree: None,
            role: None,
        },
        base_ts,
        &mut events,
    );

    rec.apply(
        Input::TurnStart {
            source_ts_ns: Some(base_ts + 10_000_000_000),
            session: s3.clone(),
            model: Some("jetski-1".to_string()),
        },
        base_ts + 10_000_000_000,
        &mut events,
    );

    // Build command overlapping worker-1's build (contention!)
    rec.apply(
        Input::ToolBegin {
            source_ts_ns: Some(base_ts + 11_000_000_000),
            session: s3.clone(),
            tool_use_id: "w3-call-1".to_string(),
            tool_name: "Bash".to_string(),
            input: ToolInput::Shell {
                argv: "cargo build".to_string(),
                cwd: None,
                git_tree: None,
            },
        },
        base_ts + 11_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::ToolEnd {
            source_ts_ns: Some(base_ts + 21_000_000_000),
            session: s3.clone(),
            tool_use_id: "w3-call-1".to_string(),
            error: false,
            result_bytes: Some(2048),
            exit_code: Some(0),
        },
        base_ts + 21_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::TurnEnd {
            source_ts_ns: Some(base_ts + 22_000_000_000),
            session: s3.clone(),
            input_tokens: Some(2000),
            output_tokens: Some(300),
            cached_tokens: Some(500),
            text_bytes: Some(1024),
            cost_micros: Some(800),
        },
        base_ts + 22_000_000_000,
        &mut events,
    );

    // Open wait on Subagent
    rec.apply(
        Input::Wait {
            source_ts_ns: Some(base_ts + 23_000_000_000),
            session: s3.clone(),
            target: Some("review:task-1".to_string()),
            on: Some(WaitOn::Subagent),
        },
        base_ts + 23_000_000_000,
        &mut events,
    );

    // Machine load1 samples
    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 10_000_000_000),
            track_key: "machine/load1".to_string(),
            value: 1200,
            unit: Some("load x 1000".to_string()),
        },
        base_ts + 10_000_000_000,
        &mut events,
    );
    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 15_000_000_000),
            track_key: "machine/load1".to_string(),
            value: 2500,
            unit: Some("load x 1000".to_string()),
        },
        base_ts + 15_000_000_000,
        &mut events,
    );
    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 20_000_000_000),
            track_key: "machine/load1".to_string(),
            value: 1800,
            unit: Some("load x 1000".to_string()),
        },
        base_ts + 20_000_000_000,
        &mut events,
    );

    // Agent CPU & RSS counter samples for overlapping builds
    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 2_000_000_000),
            track_key: "agent:worker-1/run:1/cpu".to_string(),
            value: 0,
            unit: Some("us".to_string()),
        },
        base_ts + 2_000_000_000,
        &mut events,
    );
    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 12_000_000_000),
            track_key: "agent:worker-1/run:1/cpu".to_string(),
            value: 3_000_000,
            unit: Some("us".to_string()),
        },
        base_ts + 12_000_000_000,
        &mut events,
    );
    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 5_000_000_000),
            track_key: "agent:worker-1/run:1/rss".to_string(),
            value: 50_000_000,
            unit: Some("bytes".to_string()),
        },
        base_ts + 5_000_000_000,
        &mut events,
    );
    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 12_000_000_000),
            track_key: "agent:worker-1/run:1/rss".to_string(),
            value: 0,
            unit: Some("bytes".to_string()),
        },
        base_ts + 12_000_000_000,
        &mut events,
    );

    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 11_000_000_000),
            track_key: "agent:worker-3/run:1/cpu".to_string(),
            value: 0,
            unit: Some("us".to_string()),
        },
        base_ts + 11_000_000_000,
        &mut events,
    );
    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 21_000_000_000),
            track_key: "agent:worker-3/run:1/cpu".to_string(),
            value: 4_000_000,
            unit: Some("us".to_string()),
        },
        base_ts + 21_000_000_000,
        &mut events,
    );
    rec.apply(
        Input::CounterSample {
            source_ts_ns: Some(base_ts + 15_000_000_000),
            track_key: "agent:worker-3/run:1/rss".to_string(),
            value: 80_000_000,
            unit: Some("bytes".to_string()),
        },
        base_ts + 15_000_000_000,
        &mut events,
    );

    // Build trace bytes
    let mut trace_bytes = Vec::new();
    encode_snapshot_header(
        rec.registry(),
        base_ts,
        Some(base_ts - 500_000_000),
        &mut trace_bytes,
    );
    let mut encoder = Encoder::new();
    let mut pkt = Vec::new();
    for ev in &events {
        pkt.clear();
        encoder.encode_event(ev, rec.registry(), &mut pkt);
        trace_bytes.extend_from_slice(&pkt);
    }
    fs::write(&trace_path, &trace_bytes).unwrap();

    // Query stats: must load clean
    let stats_out = run_query(
        &tp,
        &trace_path,
        "SELECT name, value, severity FROM stats WHERE severity IN ('error', 'data_loss') AND value > 0;",
    );
    let (_, stats_rows) = parse_tp_csv(&stats_out);
    assert_eq!(
        stats_rows.len(),
        0,
        "Trace must load without stats errors: {:?}",
        stats_rows
    );

    let has_pipe = supports_pipe_syntax(&tp, &trace_path);

    // =========================================================================
    // 1. agents-now
    // =========================================================================
    let sql_std = get_embedded_query("agents-now.sql").unwrap();
    let sql_pipe = get_embedded_query("agents-now.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp, &trace_path, sql_std));
    if has_pipe {
        let (_, rows_pipe) = parse_tp_csv(&run_query(&tp, &trace_path, sql_pipe));
        assert_eq!(
            rows_std, rows_pipe,
            "agents-now standard and pipe must agree"
        );
    }
    assert_eq!(
        rows_std,
        vec![
            vec!["worker-1".to_string(), "Idle".to_string(), "0".to_string()],
            vec![
                "worker-2".to_string(),
                "Idle".to_string(),
                "13000000000".to_string()
            ],
            vec![
                "worker-3".to_string(),
                "Waiting".to_string(),
                "10000000000".to_string()
            ],
        ]
    );

    // =========================================================================
    // 2. open-waits
    // =========================================================================
    let sql_std = get_embedded_query("open-waits.sql").unwrap();
    let sql_pipe = get_embedded_query("open-waits.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp, &trace_path, sql_std));
    if has_pipe {
        let (_, rows_pipe) = parse_tp_csv(&run_query(&tp, &trace_path, sql_pipe));
        assert_eq!(
            rows_std, rows_pipe,
            "open-waits standard and pipe must agree"
        );
    }
    assert_eq!(
        rows_std,
        vec![vec![
            "worker-3".to_string(),
            "1".to_string(),
            "review:task-1".to_string(),
            "WAIT_SUBAGENT".to_string(),
            "10000000000".to_string()
        ]]
    );

    // =========================================================================
    // 3. failing-commands
    // =========================================================================
    let sql_std = get_embedded_query("failing-commands.sql").unwrap();
    let sql_pipe = get_embedded_query("failing-commands.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp, &trace_path, sql_std));
    if has_pipe {
        let (_, rows_pipe) = parse_tp_csv(&run_query(&tp, &trace_path, sql_pipe));
        assert_eq!(
            rows_std, rows_pipe,
            "failing-commands standard and pipe must agree"
        );
    }
    assert_eq!(
        rows_std,
        vec![
            vec![
                "worker-1".to_string(),
                "TEST".to_string(),
                "cargo test -p core".to_string(),
                "101".to_string(),
                "5000000000".to_string()
            ],
            vec![
                "worker-1".to_string(),
                "TEST".to_string(),
                "cargo test -p core".to_string(),
                "101".to_string(),
                "5000000000".to_string()
            ],
            vec![
                "worker-1".to_string(),
                "TEST".to_string(),
                "cargo test -p core".to_string(),
                "101".to_string(),
                "5000000000".to_string()
            ],
        ]
    );

    // =========================================================================
    // 4. time-by-class
    // =========================================================================
    let sql_std = get_embedded_query("time-by-class.sql").unwrap();
    let sql_pipe = get_embedded_query("time-by-class.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp, &trace_path, sql_std));
    if has_pipe {
        let (_, rows_pipe) = parse_tp_csv(&run_query(&tp, &trace_path, sql_pipe));
        assert_eq!(
            rows_std, rows_pipe,
            "time-by-class standard and pipe must agree"
        );
    }
    assert_eq!(
        rows_std,
        vec![
            vec![
                "worker-1".to_string(),
                "BUILD".to_string(),
                "10000000000".to_string(),
                "1".to_string()
            ],
            vec![
                "worker-1".to_string(),
                "TEST".to_string(),
                "15000000000".to_string(),
                "3".to_string()
            ],
            vec![
                "worker-2".to_string(),
                "BUILD".to_string(),
                "2000000000".to_string(),
                "1".to_string()
            ],
            vec![
                "worker-3".to_string(),
                "BUILD".to_string(),
                "10000000000".to_string(),
                "1".to_string()
            ],
        ]
    );

    // =========================================================================
    // 5. edits-by-file
    // =========================================================================
    let sql_std = get_embedded_query("edits-by-file.sql").unwrap();
    let sql_pipe = get_embedded_query("edits-by-file.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp, &trace_path, sql_std));
    if has_pipe {
        let (_, rows_pipe) = parse_tp_csv(&run_query(&tp, &trace_path, sql_pipe));
        assert_eq!(
            rows_std, rows_pipe,
            "edits-by-file standard and pipe must agree"
        );
    }
    assert_eq!(
        rows_std,
        vec![
            vec![
                "src/lib.rs".to_string(),
                "1".to_string(),
                "40".to_string(),
                "0".to_string(),
                "1".to_string()
            ],
            vec![
                "src/main.rs".to_string(),
                "2".to_string(),
                "25".to_string(),
                "7".to_string(),
                "2".to_string()
            ],
        ]
    );

    // =========================================================================
    // 6. tokens-by-agent
    // =========================================================================
    let sql_std = get_embedded_query("tokens-by-agent.sql").unwrap();
    let sql_pipe = get_embedded_query("tokens-by-agent.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp, &trace_path, sql_std));
    if has_pipe {
        let (_, rows_pipe) = parse_tp_csv(&run_query(&tp, &trace_path, sql_pipe));
        assert_eq!(
            rows_std, rows_pipe,
            "tokens-by-agent standard and pipe must agree"
        );
    }
    assert_eq!(
        rows_std,
        vec![
            vec![
                "worker-1".to_string(),
                "5000".to_string(),
                "800".to_string(),
                "1000".to_string(),
                "6800".to_string()
            ],
            vec![
                "worker-2".to_string(),
                "8000".to_string(),
                "1200".to_string(),
                "3000".to_string(),
                "12200".to_string()
            ],
            vec![
                "worker-3".to_string(),
                "2000".to_string(),
                "300".to_string(),
                "500".to_string(),
                "2800".to_string()
            ],
        ]
    );

    // =========================================================================
    // 7. stalls
    // =========================================================================
    let sql_std = get_embedded_query("stalls.sql").unwrap();
    let sql_pipe = get_embedded_query("stalls.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp, &trace_path, sql_std));
    if has_pipe {
        let (_, rows_pipe) = parse_tp_csv(&run_query(&tp, &trace_path, sql_pipe));
        assert_eq!(rows_std, rows_pipe, "stalls standard and pipe must agree");
    }
    assert_eq!(
        rows_std,
        vec![
            vec![
                "worker-2".to_string(),
                "LOOP".to_string(),
                "tool input digest e3f85dfb5b619eeb repeated 4 times in a row".to_string(),
                "4".to_string()
            ],
            vec![
                "worker-1".to_string(),
                "FAILING".to_string(),
                "command 'cargo test -p core' failed 3 times in a row".to_string(),
                "3".to_string()
            ],
        ]
    );

    // =========================================================================
    // 8. contention
    // =========================================================================
    let sql_std = get_embedded_query("contention.sql").unwrap();
    let sql_pipe = get_embedded_query("contention.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp, &trace_path, sql_std));
    if has_pipe {
        let (_, rows_pipe) = parse_tp_csv(&run_query(&tp, &trace_path, sql_pipe));
        assert_eq!(
            rows_std, rows_pipe,
            "contention standard and pipe must agree"
        );
    }
    assert_eq!(
        rows_std,
        vec![
            vec![
                "1700000011000000000".to_string(),
                "1000000000".to_string(),
                "2".to_string(),
                "1200".to_string(),
            ],
            vec![
                "1700000013000000000".to_string(),
                "1000000000".to_string(),
                "2".to_string(),
                "1200".to_string(),
            ],
            vec![
                "1700000014000000000".to_string(),
                "2000000000".to_string(),
                "3".to_string(),
                "2500".to_string(),
            ],
            vec![
                "1700000016000000000".to_string(),
                "2000000000".to_string(),
                "2".to_string(),
                "2500".to_string(),
            ],
            vec![
                "1700000019000000000".to_string(),
                "2000000000".to_string(),
                "2".to_string(),
                "2500".to_string(),
            ],
        ]
    );

    // =========================================================================
    // 9. system-during-builds
    // =========================================================================
    let sql_std = get_embedded_query("system-during-builds.sql").unwrap();
    let sql_pipe = get_embedded_query("system-during-builds.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp, &trace_path, sql_std));
    if has_pipe {
        let (_, rows_pipe) = parse_tp_csv(&run_query(&tp, &trace_path, sql_pipe));
        assert_eq!(
            rows_std, rows_pipe,
            "system-during-builds standard and pipe must agree"
        );
    }
    assert_eq!(
        rows_std,
        vec![
            vec![
                "worker-1".to_string(),
                "Build".to_string(),
                "3000000000".to_string(),
                "50000000".to_string(),
            ],
            vec![
                "worker-1".to_string(),
                "Test".to_string(),
                "0".to_string(),
                "0".to_string(),
            ],
            vec![
                "worker-2".to_string(),
                "Build".to_string(),
                "0".to_string(),
                "0".to_string(),
            ],
            vec![
                "worker-3".to_string(),
                "Build".to_string(),
                "4000000000".to_string(),
                "80000000".to_string(),
            ],
        ]
    );

    // Verify CLI execute_query works for all named queries with leading comments
    for q in [
        "agents-now",
        "open-waits",
        "failing-commands",
        "time-by-class",
        "edits-by-file",
        "tokens-by-agent",
        "stalls",
        "contention",
        "system-during-builds",
    ] {
        let args = tachograph::cli::QueryArgs {
            query: q.to_string(),
            trace: Some(trace_path.clone()),
        };
        assert!(
            tachograph::query::execute_query(args).is_ok(),
            "CLI query {} failed",
            q
        );
    }
}

struct DaemonGuard {
    traced: Option<Child>,
    probes: Option<Child>,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.probes.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut child) = self.traced.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn find_perfetto_binary(name: &str) -> PathBuf {
    if let Ok(dir) = env::var("PERFETTO_BIN_DIR") {
        let p = PathBuf::from(dir).join(name);
        if p.is_file() {
            return p;
        }
    }
    let default_path =
        PathBuf::from("/usr/local/google/home/zezeozue/dev/perfetto/out/release").join(name);
    if default_path.is_file() {
        return default_path;
    }
    PathBuf::from(name)
}

#[test]
fn test_system_during_builds_with_real_traced() {
    if env::var("TACHO_TEST_TRACED").as_deref() != Ok("1") {
        eprintln!(
            "Skipping test_system_during_builds_with_real_traced: set TACHO_TEST_TRACED=1 to run"
        );
        return;
    }

    let traced_bin = find_perfetto_binary("traced");
    let probes_bin = find_perfetto_binary("traced_probes");
    let perfetto_bin = find_perfetto_binary("perfetto");
    let tp_bin = locate_trace_processor();

    let temp_dir = tempfile::tempdir().expect("failed to create tempdir");
    let producer_sock = temp_dir.path().join("producer.sock");
    let consumer_sock = temp_dir.path().join("consumer.sock");
    let runtime_dir = temp_dir.path().join("runtime");
    fs::create_dir_all(&runtime_dir).expect("failed to create runtime dir");

    // 1. Start unprivileged traced
    let mut traced_cmd = Command::new(&traced_bin);
    traced_cmd
        .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock)
        .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let traced_child = traced_cmd.spawn().expect("failed to start traced");

    // Wait for sockets
    for _ in 0..20 {
        if consumer_sock.exists() && producer_sock.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    // 2. Start unprivileged traced_probes
    let mut probes_cmd = Command::new(&probes_bin);
    probes_cmd
        .env("PERFETTO_PRODUCER_SOCK_NAME", &producer_sock)
        .env("PERFETTO_CONSUMER_SOCK_NAME", &consumer_sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let probes_child = probes_cmd.spawn().expect("failed to start traced_probes");

    let _daemon_guard = DaemonGuard {
        traced: Some(traced_child),
        probes: Some(probes_child),
    };

    env::set_var("TACHO_PERFETTO", &perfetto_bin);
    let perfetto = tacho_system::Perfetto::locate()
        .expect("locate Perfetto")
        .with_consumer_socket(&consumer_sock)
        .with_runtime_dir(&runtime_dir);

    assert!(perfetto.traced_running(), "traced must be running");

    // 3. Start system tracing session with Default profile
    perfetto
        .start(tacho_system::Profile::Default)
        .expect("start session");

    // Wait 250ms for initial process scan
    thread::sleep(Duration::from_millis(300));

    // 4. Spawn a real busy child process whose CPU and memory will be attributed
    let (start_wall_ns, start_boot_ns) = get_paired_clocks();

    let mut busy_child = Command::new("python3")
        .arg("-c")
        .arg("import time; data = bytearray(30 * 1024 * 1024); t = time.time() + 0.8; s = 0\nwhile time.time() < t: s += 1")
        .spawn()
        .expect("failed to spawn busy child");
    let child_pid = busy_child.id();

    // 5. Script agent trace with Run.pid = child_pid and a Build slice
    let mut rec = Recorder::new();
    let mut events = Vec::new();
    let s = SessionKey::new("claude", "sess-build");

    rec.apply(
        Input::RunStart {
            source_ts_ns: Some(start_wall_ns),
            provider: "claude".to_string(),
            session_id: "sess-build".to_string(),
            agent: Some("build-agent".to_string()),
            task: Some("Compile kernel module".to_string()),
            pid: Some(child_pid),
            cwd: Some("/home/dev".to_string()),
            model: Some("claude-3-5".to_string()),
            parent_session: None,
            git_tree: None,
            role: None,
        },
        start_wall_ns,
        &mut events,
    );

    rec.apply(
        Input::ToolBegin {
            source_ts_ns: Some(start_wall_ns),
            session: s.clone(),
            tool_use_id: "tool-b1".to_string(),
            tool_name: "Bash".to_string(),
            input: ToolInput::Shell {
                argv: "cargo build --release".to_string(),
                cwd: None,
                git_tree: None,
            },
        },
        start_wall_ns,
        &mut events,
    );

    let _ = busy_child.wait();
    let (end_wall_ns, _) = get_paired_clocks();

    rec.apply(
        Input::ToolEnd {
            source_ts_ns: Some(end_wall_ns),
            session: s.clone(),
            tool_use_id: "tool-b1".to_string(),
            error: false,
            result_bytes: Some(1024),
            exit_code: Some(0),
        },
        end_wall_ns,
        &mut events,
    );

    // Sleep a moment to ensure system stats poller catches the exit/RSS
    thread::sleep(Duration::from_millis(350));

    // 6. Clone system trace
    let sys_trace_path = temp_dir.path().join("system.pftrace");
    let mut sys_file = File::create(&sys_trace_path).expect("create sys trace file");
    perfetto.clone_into(&mut sys_file).expect("clone trace");
    drop(sys_file);

    // 7. Concatenate system trace packets + agent trace packets
    let merged_path = temp_dir.path().join("merged.pftrace");
    let sys_bytes = fs::read(&sys_trace_path).expect("read sys trace");

    let mut agent_bytes = Vec::new();
    encode_snapshot_header(
        rec.registry(),
        start_wall_ns,
        Some(start_boot_ns),
        &mut agent_bytes,
    );
    let mut encoder = Encoder::new();
    let mut pkt = Vec::new();
    for ev in &events {
        pkt.clear();
        encoder.encode_event(ev, rec.registry(), &mut pkt);
        agent_bytes.extend_from_slice(&pkt);
    }

    let mut merged_file = File::create(&merged_path).expect("create merged file");
    merged_file.write_all(&sys_bytes).unwrap();
    merged_file.write_all(&agent_bytes).unwrap();
    drop(merged_file);

    // 8. Run system-during-builds query on the merged trace
    let sql_std = get_embedded_query("system-during-builds.sql").unwrap();
    let sql_pipe = get_embedded_query("system-during-builds.pipe.sql").unwrap();
    let (_, rows_std) = parse_tp_csv(&run_query(&tp_bin, &merged_path, sql_std));
    let (_, rows_pipe) = parse_tp_csv(&run_query(&tp_bin, &merged_path, sql_pipe));

    assert_eq!(
        rows_std, rows_pipe,
        "system-during-builds std and pipe must agree on merged trace"
    );
    assert_eq!(rows_std.len(), 1, "Expected 1 build row for build-agent");
    assert_eq!(rows_std[0][0], "build-agent");
    assert_eq!(rows_std[0][1], "Build");

    let cpu_ns: u64 = rows_std[0][2].parse().expect("parse cpu_time_ns");
    let rss_bytes: u64 = rows_std[0][3].parse().expect("parse peak_rss_bytes");
    assert!(cpu_ns > 0, "cpu_time_ns must be > 0, got {}", cpu_ns);
    assert!(
        rss_bytes > 0,
        "peak_rss_bytes must be > 0, got {}",
        rss_bytes
    );

    let _ = perfetto.stop();
}
