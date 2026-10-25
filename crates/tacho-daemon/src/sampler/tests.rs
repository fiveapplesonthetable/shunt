//! Unit tests for the host, process-tree, cgroup, and git-worktree sampler.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::time::Duration;
use tacho_core::{AgentRole, SessionKey};

use crate::appender::ActiveRun;

use super::proc_tree::{parse_pid_list, sanitize_cmdline, ProcSampleInfo};
use super::*;

#[test]
fn test_read_loadavg() {
    let load = read_loadavg_milli();
    assert!(load.is_some());
    assert!(load.unwrap() >= 0);
}

#[test]
fn test_read_cpu() {
    let cpu = read_cpu_jiffies();
    assert!(cpu.is_some());
    let (busy, total) = cpu.unwrap();
    assert!(total >= busy);
}

#[test]
fn test_read_mem() {
    let mem = read_mem_used_bytes();
    assert!(mem.is_some());
    assert!(mem.unwrap() > 0);
}

#[test]
fn test_read_mount() {
    let used = read_mount_used_bytes("/");
    assert!(used.is_some());
    assert!(used.unwrap() > 0);
}

#[test]
fn test_sample_pid_tree() {
    let pid = std::process::id();
    let res = sample_pid_tree(pid);
    assert!(res.is_some());
    let (_cpu_us, rss_bytes) = res.unwrap();
    assert!(rss_bytes > 0);
}

#[test]
fn test_counter_sample_deduplication() {
    use crate::appender::spawn_appender;
    use crate::log::EventLog;
    use tacho_core::Recorder;

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("counter_test.sqlite");
    let log = EventLog::open(&db_path).unwrap();
    let recorder = Recorder::new();

    let (appender, _thread) = spawn_appender(recorder, log, 128);
    let mut last_samples = HashMap::new();

    // 1. Send first sample: value 42
    send_sample_if_changed(
        &appender,
        &mut last_samples,
        "machine/load1",
        42,
        Some("load"),
        1_000,
    );
    appender.flush();

    // 2. Send identical second sample: value 42 (should be skipped)
    send_sample_if_changed(
        &appender,
        &mut last_samples,
        "machine/load1",
        42,
        Some("load"),
        2_000,
    );
    appender.flush();

    // 3. Send changed third sample: value 50 (should be sent)
    send_sample_if_changed(
        &appender,
        &mut last_samples,
        "machine/load1",
        50,
        Some("load"),
        3_000,
    );
    appender.flush();

    let check_log = EventLog::open(&db_path).unwrap();
    let rows = check_log.load_all().unwrap();
    let load_events: Vec<_> = rows
        .iter()
        .filter(|e| e.track_key == "machine/load1")
        .collect();
    assert_eq!(load_events.len(), 2);
    assert_eq!(load_events[0].counter_value, Some(42));
    assert_eq!(load_events[1].counter_value, Some(50));
}

#[test]
fn bench_proc_sampler_ticks() {
    use crate::appender::spawn_appender;
    use crate::log::EventLog;
    use tacho_core::Recorder;

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("sampler_bench.sqlite");
    let log = EventLog::open(&db_path).unwrap();
    let recorder = Recorder::new();

    let (appender, _thread) = spawn_appender(recorder, log, 2048);
    let mounts = vec!["/".to_string()];
    let mut sampler = ProcSampler::new(&mounts);
    let mut run_track_buf = String::with_capacity(64);

    let iterations = 1000;
    for i in 0..iterations {
        let now_ns = 1_000_000_000 + i * 2_000_000_000;
        sampler.sample_tick(&appender, now_ns, &mut run_track_buf);
    }
    appender.flush();
    assert_eq!(appender.write_errors(), 0);
}

#[test]
fn test_dead_worker_pid_reaping() {
    use crate::appender::spawn_appender;
    use crate::log::EventLog;
    use tacho_core::{Input, Recorder, SessionKey};

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("reap_test.sqlite");
    let log = EventLog::open(&db_path).unwrap();
    let recorder = Recorder::new();

    let (appender, _thread) = spawn_appender(recorder, log, 128);

    // Spawn a short-lived process and wait for it to exit
    let mut child = Command::new("true").spawn().unwrap();
    let child_pid = child.id();
    let _ = child.wait().unwrap();

    // Register active run with this dead PID
    let _sess = SessionKey::new("test", "sess-dead-worker");
    appender
        .send_input(
            Input::RunStart {
                source_ts_ns: Some(1_000_000),
                provider: "test".to_string(),
                session_id: "sess-dead-worker".to_string(),
                agent: Some("dead-worker".to_string()),
                task: None,
                pid: Some(child_pid),
                cwd: None,
                model: None,
                parent_session: None,
                git_tree: None,
                role: None,
            },
            1_000_000,
        )
        .unwrap();
    appender.flush();

    // Verify it was in active_runs
    assert_eq!(appender.active_runs().len(), 1);

    // Run sampler tick
    let mut proc_sampler = ProcSampler::new(&[]);
    let mut run_track_buf = String::new();
    proc_sampler.sample_tick(&appender, 2_000_000, &mut run_track_buf);
    appender.flush();

    // Verify active_runs is now 0 (reaped!)
    assert_eq!(appender.active_runs().len(), 0);

    // Verify live state shows agent state Exited
    let live = appender.live_state();
    let agent = live.agents.get("dead-worker").expect("agent exists");
    assert_eq!(agent.state, "Exited");
}

#[test]
fn test_proc_tree_slices_and_expanded_mem_counters() {
    use crate::appender::spawn_appender;
    use crate::log::EventLog;
    use tacho_core::{Input, Kind, Recorder};

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("proc_slices.sqlite");
    let log = EventLog::open(&db_path).unwrap();
    let recorder = Recorder::new();
    let (appender, _thread) = spawn_appender(recorder, log, 256);

    let mut child = Command::new("sh").args(["-c", "sleep 5"]).spawn().unwrap();
    let child_pid = child.id();

    appender
        .send_input(
            Input::RunStart {
                source_ts_ns: Some(1_000_000),
                provider: "claude".into(),
                session_id: "sess-procs".into(),
                agent: Some("proc-worker".into()),
                task: Some("build".into()),
                pid: Some(child_pid),
                cwd: None,
                model: None,
                parent_session: None,
                git_tree: None,
                role: None,
            },
            1_000_000,
        )
        .unwrap();
    appender.flush();

    let mut proc_sampler = ProcSampler::new(&[]);
    let mut buf = String::new();
    proc_sampler.sample_tick(&appender, 2_000_000, &mut buf);
    appender.flush();

    let _ = child.kill();
    let _ = child.wait();

    // Next tick reaps dead process and closes all open proc slices in slot tracks
    proc_sampler.sample_tick(&appender, 3_000_000, &mut buf);
    appender.flush();

    let check_log = EventLog::open(&db_path).unwrap();
    let rows = check_log.load_all().unwrap();

    let begins = rows
        .iter()
        .filter(|e| {
            e.track_key
                .starts_with("agent:proc-worker/run:1/procs/slot:")
                && e.kind == Kind::Custom
        })
        .count();
    let ends = rows
        .iter()
        .filter(|e| {
            e.track_key
                .starts_with("agent:proc-worker/run:1/procs/slot:")
                && e.kind == Kind::CustomEnded
        })
        .count();
    assert!(begins >= 1, "must record at least 1 process slice begin");
    assert_eq!(
        begins, ends,
        "every opened process slice must be closed when run ends"
    );
    assert!(rows
        .iter()
        .any(|e| e.track_key == "agent:proc-worker/run:1/proc_count"
            && e.counter_value.unwrap_or(0) >= 1));
    assert!(rows.iter().any(|e| e.track_key == "machine/mem_avail"));
    assert!(rows.iter().any(|e| e.track_key == "machine/mem_cached"));
}

#[test]
fn test_slot_allocation_lifecycle() {
    use crate::appender::spawn_appender;
    use crate::log::EventLog;
    use tacho_core::{Kind, Recorder};

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("slot_test.sqlite");
    let log = EventLog::open(&db_path).unwrap();
    let recorder = Recorder::new();
    let (appender, _thread) = spawn_appender(recorder, log, 256);

    let mut sampler = ProcSampler::new(&[]);
    let session = SessionKey::new("claude", "sess-slot-test");
    let run_track = "agent:worker/run:1";

    let make_proc = |pid: u32, ppid: u32, comm_str: &str| {
        let mut comm = [0u8; 16];
        let b = comm_str.as_bytes();
        let len = b.len().min(16);
        comm[..len].copy_from_slice(&b[..len]);
        ProcSampleInfo {
            pid,
            start_ticks: 1,
            ppid,
            proc_cpu_us: 1000,
            rss_bytes: 4096,
            comm,
            comm_len: len as u8,
        }
    };

    // Tick 1: Root pid 100 + siblings 101, 102
    let procs1 = vec![
        make_proc(100, 1, "root"),
        make_proc(101, 100, "worker-1"),
        make_proc(102, 100, "worker-2"),
    ];
    sampler.update_proc_slices(&session, 100, run_track, &procs1, &appender, 1_000_000);
    appender.flush();

    let slots = sampler.proc_slots.get(&session).unwrap();
    assert_eq!(slots.get(&100).unwrap().slot, 0); // Root process gets slot 0
    assert_eq!(slots.get(&101).unwrap().slot, 1);
    assert_eq!(slots.get(&102).unwrap().slot, 2);

    // Tick 2: Sibling 101 exits. Survivor 102 keeps its slot (slot 2) and is NOT re-begun!
    let procs2 = vec![make_proc(100, 1, "root"), make_proc(102, 100, "worker-2")];
    sampler.update_proc_slices(&session, 100, run_track, &procs2, &appender, 2_000_000);
    appender.flush();

    let slots = sampler.proc_slots.get(&session).unwrap();
    assert_eq!(slots.len(), 2);
    assert_eq!(slots.get(&102).unwrap().slot, 2);

    let check_log = EventLog::open(&db_path).unwrap();
    let rows = check_log.load_all().unwrap();
    // Slot 1 must have an End event for pid 101
    assert!(rows.iter().any(
        |e| e.track_key == format!("{}/procs/slot:1", run_track) && e.kind == Kind::CustomEnded
    ));
    // Slot 2 must only have ONE Begin event across tick 1 and 2 (survivor not re-begun)
    let slot2_begins = rows
        .iter()
        .filter(|e| e.track_key == format!("{}/procs/slot:2", run_track) && e.kind == Kind::Custom)
        .count();
    assert_eq!(slot2_begins, 1, "survivor must not be re-begun");

    // Tick 3: New process 103 arrives -> reuses lowest free slot (slot 1)
    let procs3 = vec![
        make_proc(100, 1, "root"),
        make_proc(102, 100, "worker-2"),
        make_proc(103, 100, "worker-3"),
    ];
    sampler.update_proc_slices(&session, 100, run_track, &procs3, &appender, 3_000_000);
    appender.flush();

    let slots = sampler.proc_slots.get(&session).unwrap();
    assert_eq!(
        slots.get(&103).unwrap().slot,
        1,
        "new process must reuse lowest free slot"
    );

    // Tick 4: Process 102 execs (comm changes: "worker-2" -> "python3")
    // Must emit End on slot 2, then Begin on slot 2 (same slot!)
    let procs4 = vec![
        make_proc(100, 1, "root"),
        make_proc(102, 100, "python3"),
        make_proc(103, 100, "worker-3"),
    ];
    sampler.update_proc_slices(&session, 100, run_track, &procs4, &appender, 4_000_000);
    appender.flush();

    let slots = sampler.proc_slots.get(&session).unwrap();
    assert_eq!(
        slots.get(&102).unwrap().slot,
        2,
        "exec must retain same slot"
    );
    assert_eq!(slots.get(&102).unwrap().comm, "python3");

    let rows4 = check_log.load_all().unwrap();
    let slot2_ends = rows4
        .iter()
        .filter(|e| {
            e.track_key == format!("{}/procs/slot:2", run_track) && e.kind == Kind::CustomEnded
        })
        .count();
    let slot2_begins_new = rows4
        .iter()
        .filter(|e| e.track_key == format!("{}/procs/slot:2", run_track) && e.kind == Kind::Custom)
        .count();
    assert_eq!(slot2_ends, 1, "exec must emit End on same slot");
    assert_eq!(slot2_begins_new, 2, "exec must emit Begin on same slot");
}

#[test]
fn test_worktree_commit_and_file_change_progress_signals() {
    use crate::appender::spawn_appender;
    use crate::log::EventLog;
    use tacho_core::{Input, Kind, Recorder};

    let temp = tempfile::tempdir().unwrap();
    let repo_dir = temp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();

    assert!(Command::new("git")
        .args(["init"])
        .current_dir(&repo_dir)
        .status()
        .unwrap()
        .success());
    let _ = Command::new("git")
        .args(["config", "user.email", "test@tacho.dev"])
        .current_dir(&repo_dir)
        .status();
    let _ = Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&repo_dir)
        .status();
    std::fs::write(repo_dir.join("README.md"), "init\n").unwrap();
    std::fs::write(repo_dir.join(".gitignore"), "ignored.txt\n").unwrap();
    assert!(Command::new("git")
        .args(["add", "."])
        .current_dir(&repo_dir)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&repo_dir)
        .status()
        .unwrap()
        .success());

    let db_path = temp.path().join("wt_test.sqlite");
    let log = EventLog::open(&db_path).unwrap();
    let recorder = Recorder::new();
    let (appender, _thread) = spawn_appender(recorder, log, 256);

    appender
        .send_input(
            Input::RunStart {
                source_ts_ns: Some(1_000_000_000),
                provider: "claude".into(),
                session_id: "sess-wt".into(),
                agent: Some("wt-worker".into()),
                task: Some("worktree task".into()),
                pid: None,
                cwd: Some(repo_dir.to_string_lossy().to_string()),
                model: None,
                parent_session: None,
                git_tree: None,
                role: Some(AgentRole::Worker),
            },
            1_000_000_000,
        )
        .unwrap();
    appender.flush();

    let mut proc_sampler = ProcSampler::new(&[]);
    proc_sampler.set_worktree_scan_secs(0); // Allow immediate scanning in tests
    let mut buf = String::new();

    // Tick 1: baseline snapshot
    proc_sampler.sample_tick(&appender, 2_000_000_000, &mut buf);
    appender.flush();

    // 1. Write ignored file -> must NOT emit WorktreeFileChange
    std::fs::write(repo_dir.join("ignored.txt"), "cache output\n").unwrap();
    proc_sampler.sample_tick(&appender, 3_000_000_000, &mut buf);
    appender.flush();

    let check_log = EventLog::open(&db_path).unwrap();
    let rows1 = check_log.load_all().unwrap();
    assert!(
        !rows1.iter().any(|e| e.kind == Kind::Milestone
            && e.custom_name
                .as_deref()
                .is_some_and(|n| n.starts_with("WorktreeFileChange"))),
        "ignored file change must NOT emit WorktreeFileChange"
    );

    // 2. Modify tracked file -> must emit WorktreeFileChange
    std::fs::write(repo_dir.join("README.md"), "updated content\n").unwrap();
    proc_sampler.sample_tick(&appender, 4_000_000_000, &mut buf);
    appender.flush();

    let rows2 = check_log.load_all().unwrap();
    assert!(
        rows2.iter().any(|e| e.kind == Kind::Milestone
            && e.custom_name
                .as_deref()
                .is_some_and(|n| n.starts_with("WorktreeFileChange"))),
        "tracked file edit must emit WorktreeFileChange"
    );

    // 3. Commit tracked file -> must emit WorktreeCommit
    assert!(Command::new("git")
        .args(["add", "."])
        .current_dir(&repo_dir)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["commit", "-m", "second commit"])
        .current_dir(&repo_dir)
        .status()
        .unwrap()
        .success());
    proc_sampler.sample_tick(&appender, 5_000_000_000, &mut buf);
    appender.flush();

    let rows3 = check_log.load_all().unwrap();
    assert!(
        rows3.iter().any(|e| e.kind == Kind::Milestone
            && e.custom_name
                .as_deref()
                .is_some_and(|n| n.starts_with("WorktreeCommit"))),
        "commit must emit WorktreeCommit"
    );

    // 4. Checkout older commit -> must emit WorktreeHeadMoved (NOT WorktreeCommit)
    assert!(Command::new("git")
        .args(["checkout", "HEAD~1"])
        .current_dir(&repo_dir)
        .status()
        .unwrap()
        .success());
    proc_sampler.sample_tick(&appender, 6_000_000_000, &mut buf);
    appender.flush();

    let rows4 = check_log.load_all().unwrap();
    assert!(
        rows4.iter().any(|e| matches!(
            &e.body.body,
            Some(tacho_proto::tacho::event::Body::Milestone(m))
                if m.name.as_deref() == Some("WorktreeHeadMoved")
        )),
        "checkout-back must emit WorktreeHeadMoved"
    );

    let live = appender.live_state();
    let agent = live.agents.get("wt-worker").unwrap();
    assert_eq!(
        agent.commits_succeeded, 1,
        "checkout-back must NOT increment commits_succeeded"
    );
    assert!(agent.last_edit_ts_ns.is_some());
}

#[test]
fn test_cgroup_integration_accounting() {
    use crate::appender::spawn_appender;
    use crate::cgroup::{unit_name, AgentCgroup};
    use crate::log::EventLog;
    use tacho_core::{Input, Recorder};

    // Skip test gracefully if busctl --user is not available or failing
    let busctl_check = Command::new("busctl").arg("--user").arg("list").output();
    if busctl_check.is_err() || !busctl_check.as_ref().unwrap().status.success() {
        eprintln!("skipping cgroup integration test: busctl --user unavailable");
        return;
    }

    struct ScopeGuard {
        unit: String,
        child_pid: u32,
    }

    impl Drop for ScopeGuard {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.child_pid as i32), libc::SIGKILL);
                libc::kill(self.child_pid as i32, libc::SIGKILL);
            }
            let _ = Command::new("systemctl")
                .args(["--user", "stop", &self.unit])
                .output();
        }
    }

    // Spawn a daemonized burner that escapes the process tree via setsid. It does a fixed
    // amount of CPU work, announces completion with an atomically created marker, then
    // idles until its parent dies, so the test waits on the marker instead of on a clock
    // and nothing outlives the test.
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("burned");
    let script = format!(
        "setsid sh -c 'i=0; while [ $i -lt 1000000 ]; do i=$((i+1)); done; \
         touch {m}.tmp; mv {m}.tmp {m}; exec tail --pid=$PPID -f /dev/null' & exec sleep 100000",
        m = marker.display()
    );
    let mut child = match Command::new("sh").args(["-c", &script]).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping cgroup integration test: failed to spawn: {}", e);
            return;
        }
    };

    let child_pid = child.id();
    let unit = unit_name("cgroup-worker", 1, child_pid);
    let _guard = ScopeGuard {
        unit: unit.clone(),
        child_pid,
    };

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("cgroup_test.sqlite");
    let log = EventLog::open(&db_path).unwrap();
    let recorder = Recorder::new();
    let (appender, _thread) = spawn_appender(recorder, log, 256);

    let now_ns = 1_000_000_000;
    appender
        .send_input(
            Input::RunStart {
                source_ts_ns: Some(now_ns),
                provider: "claude".into(),
                session_id: "sess-cg".into(),
                agent: Some("cgroup-worker".into()),
                task: Some("cgroup test".into()),
                pid: Some(child_pid),
                cwd: None,
                model: None,
                parent_session: None,
                git_tree: None,
                role: None,
            },
            now_ns,
        )
        .unwrap();
    appender.flush();

    let mut proc_sampler = ProcSampler::new(&[]);
    let mut buf = String::new();

    // Tick 1: trigger attach
    proc_sampler.sample_tick(&appender, now_ns + 10_000_000, &mut buf);
    appender.flush();

    let cg = match AgentCgroup::of_pid(child_pid, &unit) {
        Some(cg) => cg,
        None => {
            eprintln!("skipping cgroup integration test: cgroup attach failed or scope not found");
            return;
        }
    };
    assert!(cg.dir.exists(), "scope cgroup dir must exist: {:?}", cg.dir);

    // Wait until the burner has finished its fixed CPU work inside the cgroup.
    crate::test_util::wait_for_file(&marker);

    // Tick 2: sample CPU and processes
    proc_sampler.sample_tick(&appender, now_ns + 650_000_000, &mut buf);
    appender.flush();

    let check_log = EventLog::open(&db_path).unwrap();
    let rows = check_log.load_all().unwrap();

    // 1. Assert <run>/cpu grew by > 100_000 us (100 ms)
    let cpu_samples: Vec<i64> = rows
        .iter()
        .filter(|e| e.track_key == "agent:cgroup-worker/run:1/cpu" && e.counter_value.is_some())
        .filter_map(|e| e.counter_value)
        .collect();
    assert!(
        !cpu_samples.is_empty(),
        "expected at least one cpu counter sample"
    );
    let max_cpu = *cpu_samples.iter().max().unwrap();
    assert!(
        max_cpu > 100_000,
        "cgroup cumulative CPU must grow > 100_000 us, got {}",
        max_cpu
    );

    // 2. Assert the daemonized burner appears in a procs slot slice name
    let has_burner_slice = rows.iter().any(|e| {
        e.track_key
            .starts_with("agent:cgroup-worker/run:1/procs/slot:")
            && e.custom_name
                .as_deref()
                .is_some_and(|name| name.contains("tail --pid"))
    });
    assert!(
        has_burner_slice,
        "daemonized burner process must appear in a procs slot slice"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn test_on_stall_effect_hook_fires() {
    use crate::appender::spawn_appender_with_hook;
    use crate::log::EventLog;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tacho_core::{Input, Recorder, SessionKey, ToolInput};

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("hook_test.sqlite");
    let marker_path = temp.path().join("stall_fired.txt");
    let log = EventLog::open(&db_path).unwrap();
    let recorder = Recorder::new();

    // Written to a temp file and renamed, so the marker appears with complete content.
    let hook_cmd = format!(
        "printf '%s:%s:%s' \"$TACHO_AGENT\" \"$TACHO_STALL_KIND\" \"$TACHO_SEQ\" > {p}.tmp && mv {p}.tmp {p}",
        p = marker_path.display()
    );
    let (appender, _thread) = spawn_appender_with_hook(recorder, log, 256, Some(hook_cmd));

    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let sess = SessionKey::new("claude", "sess-hook");
    appender
        .send_input(
            Input::RunStart {
                source_ts_ns: Some(now_ns),
                provider: "claude".into(),
                session_id: "sess-hook".into(),
                agent: Some("hook-worker".into()),
                task: Some("impl".into()),
                pid: None,
                cwd: None,
                model: None,
                parent_session: None,
                git_tree: None,
                role: None,
            },
            now_ns,
        )
        .unwrap();

    for i in 0..4 {
        let ts = now_ns + 10_000 + i * 100_000;
        appender
            .send_input(
                Input::ToolBegin {
                    source_ts_ns: Some(ts),
                    session: sess.clone(),
                    tool_use_id: format!("t-{}", i),
                    tool_name: "Bash".into(),
                    input: ToolInput::Shell {
                        argv: "cat missing.txt".into(),
                        cwd: None,
                        git_tree: None,
                    },
                },
                ts,
            )
            .unwrap();
    }
    appender.flush();

    crate::test_util::wait_for_file(&marker_path);
    let content = std::fs::read_to_string(&marker_path).unwrap();

    assert!(
        content.starts_with("hook-worker:LOOP:"),
        "unexpected hook payload: {}",
        content
    );
}

#[test]
fn test_parsers_reject_malformed_and_overflowing_input() {
    assert_eq!(parse_i64_ascii(b"-9223372036854775808"), Some(i64::MIN));
    assert_eq!(parse_i64_ascii(b"9223372036854775808"), None);
    assert_eq!(parse_i64_ascii(b"-"), None);
    assert_eq!(parse_u64_ascii(b"99999999999999999999999"), None);
    assert_eq!(parse_loadavg_milli(b"99999999999999999.5 1 1"), None);
    assert_eq!(parse_loadavg_milli(b"0.5"), Some(500));
    assert_eq!(parse_loadavg_milli(b""), None);
    assert_eq!(parse_cpu_jiffies(b"cpu  garbage"), None);
    assert_eq!(parse_meminfo(b"MemTotal: lots\n"), None);
    let pids: Vec<u32> = parse_pid_list(b"12 34\n99999999999 5x 6 \t7").collect();
    assert_eq!(pids, vec![12, 34, 6, 7]);
}

#[test]
fn test_sanitize_cmdline_keeps_unicode_and_drops_control_bytes() {
    assert_eq!(
        sanitize_cmdline(b"cargo\0test\0--\0\xc3\xbcber/\xe6\x97\xa5\xe6\x9c\xac\0"),
        "cargo test -- \u{fc}ber/\u{65e5}\u{672c}"
    );
    assert_eq!(sanitize_cmdline(b"a\x1b[31m\0\0\0b\n\t c"), "a[31m b c");
    assert_eq!(sanitize_cmdline(b"\xff\xfe"), "\u{fffd}\u{fffd}");
    assert_eq!(sanitize_cmdline(b"\0\0"), "");
}

#[test]
fn test_tree_walk_finds_children_forked_from_non_main_threads_beyond_512_bytes() {
    // This test runs on a harness worker thread, not the process's main thread, and
    // spawns enough children that the per-thread `children` list exceeds 512 bytes.
    let mut kids: Vec<std::process::Child> = (0..120)
        .map(|_| {
            Command::new("sleep")
                .arg("100000")
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut sampler = ProcSampler::new(&[]);
    assert!(sampler.populate_tree_pids(std::process::id()));
    let found: HashSet<u32> = sampler.tree_pids_scratch.iter().copied().collect();
    let missing: Vec<u32> = kids
        .iter()
        .map(|k| k.id())
        .filter(|p| !found.contains(p))
        .collect();
    for k in &mut kids {
        let _ = k.kill();
        let _ = k.wait();
    }
    assert!(
        missing.is_empty(),
        "tree walk missed children {:?}",
        missing
    );
}

#[test]
fn test_run_git_handles_output_larger_than_a_pipe_buffer() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo)
        .status()
        .unwrap()
        .success());
    // ~4000 untracked files with long names: `ls-files -o -z` prints well over 64 KiB.
    let long = "ünïcødé-".repeat(6);
    for i in 0..4000 {
        std::fs::write(repo.join(format!("{}{:05}.txt", long, i)), b"x").unwrap();
    }
    let (_, newest, hash) = scan_worktree_files(repo, Duration::from_secs(600)).unwrap();
    assert!(newest.unwrap().starts_with(&long));
    assert_ne!(hash, 0);

    let err = run_git(repo, &["no-such-subcommand"], Duration::from_secs(600)).unwrap_err();
    assert!(err.to_string().contains("failed"));
}

#[test]
fn test_recycled_pid_in_proc_slot_is_ended_and_begun_again() {
    use crate::appender::spawn_appender;
    use crate::log::EventLog;
    use tacho_core::{Kind, Recorder};

    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("recycle.sqlite");
    let (appender, thread) = spawn_appender(Recorder::new(), EventLog::open(&db_path).unwrap(), 64);
    let mut sampler = ProcSampler::new(&[]);
    let session = SessionKey::new("claude", "sess-recycle");
    let proc_info = |pid: u32, start_ticks: u64, comm: &str| {
        let mut c = [0u8; 16];
        c[..comm.len()].copy_from_slice(comm.as_bytes());
        ProcSampleInfo {
            pid,
            start_ticks,
            ppid: 1,
            proc_cpu_us: 0,
            rss_bytes: 0,
            comm: c,
            comm_len: comm.len() as u8,
        }
    };
    let track = "agent:w/run:1";
    sampler.update_proc_slices(
        &session,
        1,
        track,
        &[proc_info(1, 10, "root"), proc_info(7, 20, "cc")],
        &appender,
        1_000,
    );
    // Same pid 7 and same comm, but a different start time: a new process.
    sampler.update_proc_slices(
        &session,
        1,
        track,
        &[proc_info(1, 10, "root"), proc_info(7, 99, "cc")],
        &appender,
        2_000,
    );
    assert!(appender.flush());
    appender.shutdown();
    thread.join().unwrap();

    let rows = EventLog::open(&db_path).unwrap().load_all().unwrap();
    let on_slot = |kind: Kind| {
        rows.iter()
            .filter(|e| e.track_key.starts_with("agent:w/run:1/procs/slot:") && e.kind == kind)
            .count()
    };
    assert_eq!(
        on_slot(Kind::Custom),
        3,
        "root + old pid 7 + recycled pid 7 begins"
    );
    assert_eq!(
        on_slot(Kind::CustomEnded),
        1,
        "the old pid 7 slice must end"
    );
    assert_eq!(sampler.proc_slots[&session][&7].start_ticks, 99);
}

#[test]
fn test_root_pid_recycled_reads_as_gone() {
    let me = std::process::id();
    let run = ActiveRun {
        session: SessionKey::new("claude", "sess-root"),
        agent_id: "w".into(),
        run_track: "agent:w/run:1".into(),
        pid: me,
        cwd: None,
        role: AgentRole::Worker,
    };
    let mut sampler = ProcSampler::new(&[]);
    assert!(
        !sampler.run_process_gone(&run),
        "a live process is not gone"
    );
    // Pretend we first saw a different process under this pid.
    sampler
        .root_start_ticks
        .insert(run.session.clone(), u64::MAX);
    assert!(
        sampler.run_process_gone(&run),
        "a recycled pid must read as gone"
    );
    let dead = ActiveRun {
        pid: u32::MAX - 1,
        ..run
    };
    assert!(sampler.run_process_gone(&dead));
}

#[test]
fn test_decide_due_only_on_event_request_or_refresh() {
    let mut s = ProcSampler::new(&[]);
    s.set_decide_refresh_secs(60);
    let sec = 1_000_000_000u64;
    assert!(s.decide_due(0, 0), "first tick always decides");

    s.last_decide_seq = Some(7);
    s.last_decide_ns = 100 * sec;
    assert!(
        !s.decide_due(7, 101 * sec),
        "no event, no request, refresh not due"
    );
    assert!(s.decide_due(8, 101 * sec), "a decide-relevant commit");
    assert!(s.decide_due(7, 160 * sec), "coarse refresh elapsed");

    s.decide_requested.store(true, Ordering::Release);
    assert!(s.decide_due(7, 101 * sec), "explicit request");
}
