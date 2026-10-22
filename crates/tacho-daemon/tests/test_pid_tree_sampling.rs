//! Test for per-run process-tree sampling (/proc/<pid>/stat and status).
//!
//! Spawns a real busy child process with descendants and verifies that the sampler thread
//! records positive CPU time and RSS values on agent:<agent>/run:<n>/cpu and rss tracks.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tacho_core::{Input, SessionKey};
use tacho_daemon::{Config, Daemon, DaemonPaths};

#[test]
fn test_per_run_process_tree_sampling() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("runtime");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();

    let mut config = Config::default();
    config.sampler.interval_ms = 100; // Fast sampling for test

    let daemon = Daemon::start(paths.clone(), config).unwrap();

    // 1. Spawn a real child process with a descendant process doing busy work
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("python3 -c 'import time; [x*x for x in range(2_000_000)]; time.sleep(1)' & python3 -c 'import time; [x*x for x in range(2_000_000)]; time.sleep(1)'; wait")
        .spawn()
        .expect("failed to spawn child process");

    let child_pid = child.id();
    let session = SessionKey::new("claude", "sess-tree-001");

    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    // 2. Register run with child_pid
    daemon
        .appender
        .send_input(
            Input::RunStart {
                source_ts_ns: Some(now_ns),
                provider: "claude".to_string(),
                session_id: "sess-tree-001".to_string(),
                agent: Some("tree-agent".to_string()),
                task: None,
                pid: Some(child_pid),
                cwd: None,
                model: None,
                parent_session: None,
                git_tree: None,
            },
            now_ns,
        )
        .unwrap();

    daemon.appender.flush();

    // Verify active_runs contains the child_pid
    let active = daemon.appender.active_runs();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].pid, child_pid);
    assert_eq!(active[0].run_track, "agent:tree-agent/run:1");

    // 3. Poll until sampler thread records cpu and rss counters in SQLite log
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut sampled_cpu = false;
    let mut sampled_rss = false;
    let mut observed_cpu_val = 0i64;
    let mut observed_rss_val = 0i64;

    let log = tacho_daemon::EventLog::open(&paths.events_db()).unwrap();

    while Instant::now() < deadline {
        daemon.appender.flush();
        if let Ok(events) = log.load_all() {
            for ev in events {
                if ev.track_key == "agent:tree-agent/run:1/cpu" {
                    if let Some(val) = ev.counter_value {
                        if val > 0 {
                            sampled_cpu = true;
                            observed_cpu_val = val;
                        }
                    }
                }
                if ev.track_key == "agent:tree-agent/run:1/rss" {
                    if let Some(val) = ev.counter_value {
                        if val > 0 {
                            sampled_rss = true;
                            observed_rss_val = val;
                        }
                    }
                }
            }
        }
        if sampled_cpu && sampled_rss {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let _ = child.wait();

    assert!(
        sampled_cpu,
        "Expected positive CPU counter sample on agent:tree-agent/run:1/cpu, observed {}",
        observed_cpu_val
    );
    assert!(
        sampled_rss,
        "Expected positive RSS counter sample on agent:tree-agent/run:1/rss, observed {}",
        observed_rss_val
    );

    // 4. End the run
    let end_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    daemon
        .appender
        .send_input(
            Input::RunEnd {
                source_ts_ns: Some(end_ts),
                session,
                exit_code: Some(0),
                reason: Some("completed".to_string()),
            },
            end_ts,
        )
        .unwrap();

    daemon.appender.flush();

    // Verify active_runs is now empty
    let active_after = daemon.appender.active_runs();
    assert_eq!(active_after.len(), 0);
}
