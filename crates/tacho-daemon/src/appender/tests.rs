use super::*;
use crate::test_util::wait_for_file;
use std::path::Path;
use tacho_core::{SessionKey, ToolInput};

fn now_ns() -> u64 {
    wall_clock_ns()
}

fn run_start(session_id: &str, agent: &str, ts: u64) -> Input {
    Input::RunStart {
        source_ts_ns: Some(ts),
        provider: "claude".into(),
        session_id: session_id.into(),
        agent: Some(agent.into()),
        task: Some("work".into()),
        pid: None,
        cwd: None,
        model: None,
        parent_session: None,
        git_tree: None,
        role: None,
    }
}

/// Sends a run start plus four identical shell calls, which the loop detector flags.
fn trigger_loop_stall(appender: &AppenderHandle, session_id: &str, agent: &str, t0: u64) {
    appender
        .send_input(run_start(session_id, agent, t0), t0)
        .unwrap();
    let sess = SessionKey::new("claude", session_id);
    for i in 0..4 {
        let ts = t0 + 10_000 + i * 100_000;
        appender
            .send_input(
                Input::ToolBegin {
                    source_ts_ns: Some(ts),
                    session: sess.clone(),
                    tool_use_id: format!("t-{}-{}", agent, i),
                    tool_name: "Bash".into(),
                    input: ToolInput::Shell {
                        argv: format!("cat {}.txt", agent),
                        cwd: None,
                        git_tree: None,
                    },
                },
                ts,
            )
            .unwrap();
    }
    assert!(appender.flush());
}

/// Hook script: for `marker_agent` it atomically writes `<dir>/<agent>.done` with
/// `agent:kind`; any other agent runs `other` first.
fn hook_script(dir: &Path, other: &str) -> String {
    format!(
        r#"{other}
printf '%s:%s' "$TACHO_AGENT" "$TACHO_STALL_KIND" > "{dir}/$TACHO_AGENT.tmp"
mv "{dir}/$TACHO_AGENT.tmp" "{dir}/$TACHO_AGENT.done""#,
        dir = dir.display(),
        other = other
    )
}

fn open_log(dir: &Path) -> EventLog {
    EventLog::open(&dir.join("events.sqlite")).unwrap()
}

#[test]
fn test_run_track_key_helper() {
    assert_eq!(run_track_key("worker", 0), "agent:worker/run:1");
    assert_eq!(run_track_key("worker", 1), "agent:worker/run:1");
    assert_eq!(run_track_key("worker", 5), "agent:worker/run:5");
}

#[test]
fn test_on_stall_hook_timeout_kills_process_group_and_unblocks_second_hook() {
    let temp = tempfile::tempdir().unwrap();
    // The stuck agent's hook never finishes on its own; the worker must kill it after the
    // timeout and move on to the next queued hook.
    let hook = hook_script(
        temp.path(),
        r#"if [ "$TACHO_AGENT" = "stuck-worker" ]; then exec sleep 100000; fi"#,
    );
    let (appender, thread) = spawn_appender_with_hook_timeout(
        Recorder::new(),
        open_log(temp.path()),
        256,
        Some(hook),
        Duration::from_millis(300),
    );

    let t0 = now_ns();
    trigger_loop_stall(&appender, "sess-stuck", "stuck-worker", t0);
    trigger_loop_stall(&appender, "sess-second", "second-worker", t0 + 1_000_000);

    let marker = temp.path().join("second-worker.done");
    wait_for_file(&marker);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "second-worker:LOOP"
    );
    assert!(!temp.path().join("stuck-worker.done").exists());

    appender.shutdown();
    thread.join().unwrap();
}

#[test]
fn test_on_stall_hook_ignores_historical_replay() {
    let temp = tempfile::tempdir().unwrap();
    let hook = hook_script(temp.path(), ":");
    let (appender, thread) = spawn_appender_with_hook_timeout(
        Recorder::new(),
        open_log(temp.path()),
        256,
        Some(hook),
        Duration::from_secs(30),
    );

    // An hour-old stall (replayed history) must not fire; a fresh one afterwards must.
    // Hooks run in FIFO order, so once the fresh marker exists the old one would exist too.
    let old_ts = now_ns() - 3600 * 1_000_000_000;
    trigger_loop_stall(&appender, "sess-old", "old-worker", old_ts);
    trigger_loop_stall(&appender, "sess-new", "new-worker", now_ns());

    wait_for_file(&temp.path().join("new-worker.done"));
    assert!(
        !temp.path().join("old-worker.done").exists(),
        "historical replay stall must not fire on_stall hook"
    );
    appender.shutdown();
    thread.join().unwrap();
}

#[test]
fn test_on_stall_hook_background_stragglers_are_killed() {
    let temp = tempfile::tempdir().unwrap();
    let pidfile = temp.path().join("straggler.pid");
    // The first hook leaves a background process behind and exits at once.
    let hook = hook_script(
        temp.path(),
        &format!(
            r#"if [ "$TACHO_AGENT" = "leaky" ]; then
  sleep 100000 </dev/null >/dev/null 2>&1 &
  echo $! > "{p}.tmp"; mv "{p}.tmp" "{p}"
fi"#,
            p = pidfile.display()
        ),
    );
    let (appender, thread) = spawn_appender_with_hook_timeout(
        Recorder::new(),
        open_log(temp.path()),
        256,
        Some(hook),
        Duration::from_secs(60),
    );
    let t0 = now_ns();
    trigger_loop_stall(&appender, "sess-leaky", "leaky", t0);
    trigger_loop_stall(&appender, "sess-after", "after", t0 + 1_000_000);
    // FIFO: the second hook finishing means the first was fully handled (killed + reaped).
    wait_for_file(&temp.path().join("after.done"));

    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Wait for the straggler's exit on a pidfd. If it was not killed this blocks, and the
    // suite's overall cap reports the failure.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if pidfd >= 0 {
        let mut pfd = libc::pollfd {
            fd: pidfd,
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut pfd, 1, -1) }, 1);
        unsafe { libc::close(pidfd) };
    }
    appender.shutdown();
    thread.join().unwrap();
}

#[test]
fn test_shutdown_is_reliable_with_a_full_queue_and_commits_queued_input() {
    let temp = tempfile::tempdir().unwrap();
    let (appender, thread) = spawn_appender(Recorder::new(), open_log(temp.path()), 1);
    appender.hooks().set_wedged(true);
    let t0 = now_ns();
    appender.send_input(run_start("s1", "a1", t0), t0).unwrap();
    // Depending on where the loop was when wedged, s1 is either committed already or sitting
    // in the queue; this second send fills (or times out on) the capacity-1 queue.
    let _ = appender.send_input(run_start("s2", "a2", t0 + 1), t0 + 1);
    // A Shutdown command cannot be queued now, but shutdown must still take effect.
    appender.shutdown();
    appender.hooks().set_wedged(false);
    thread.join().unwrap();

    let rows = open_log(temp.path()).load_all().unwrap();
    assert!(
        rows.iter().any(|e| e.agent.as_deref() == Some("a1")),
        "input accepted before shutdown must be committed"
    );
    assert!(
        !appender.flush(),
        "flush on a stopped appender must report failure"
    );
}

#[test]
fn test_wait_state_change_until_returns_on_cancel_and_on_change() {
    let temp = tempfile::tempdir().unwrap();
    let (appender, thread) = spawn_appender(Recorder::new(), open_log(temp.path()), 16);
    let cancel = Arc::new(AtomicBool::new(false));

    let seq0 = appender.state_seq();
    let waiter = {
        let appender = appender.clone();
        let cancel = cancel.clone();
        std::thread::spawn(move || appender.wait_state_change_until(seq0, &cancel))
    };
    cancel.store(true, Ordering::Release);
    appender.wake_state_waiters();
    assert_eq!(waiter.join().unwrap(), seq0);

    let waiter = {
        let appender = appender.clone();
        std::thread::spawn(move || appender.wait_state_change_until(seq0, &AtomicBool::new(false)))
    };
    let t0 = now_ns();
    appender.send_input(run_start("s", "a", t0), t0).unwrap();
    assert!(appender.flush());
    assert!(waiter.join().unwrap() > seq0);

    appender.shutdown();
    thread.join().unwrap();
}

#[test]
fn test_huge_batch_is_committed_in_full_and_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let (appender, thread) = spawn_appender(Recorder::new(), open_log(temp.path()), 4);
    let t0 = now_ns();
    // Far more than one transaction's worth, with unicode agent names.
    let batch: Vec<(Input, u64)> = (0..3_000u64)
        .map(|i| {
            (
                run_start(&format!("s{}", i), &format!("агент-{}-🚀", i), t0 + i),
                t0 + i,
            )
        })
        .collect();
    appender.send_input_batch(batch).unwrap();
    assert!(appender.flush());
    appender.shutdown();
    thread.join().unwrap();

    let rows = open_log(temp.path()).load_all().unwrap();
    let seqs: Vec<u64> = rows.iter().map(|e| e.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq must be strictly increasing"
    );
    for i in [0u64, 1_499, 2_999] {
        let name = format!("агент-{}-🚀", i);
        assert!(rows
            .iter()
            .any(|e| e.agent.as_deref() == Some(name.as_str())));
    }
    assert_eq!(appender.live_state().agents.len(), 3_000);
}

#[test]
fn test_active_runs_track_start_and_end() {
    let temp = tempfile::tempdir().unwrap();
    let (appender, thread) = spawn_appender(Recorder::new(), open_log(temp.path()), 16);
    let t0 = now_ns();
    let mut start = run_start("sess-run", "runner", t0);
    if let Input::RunStart { pid, cwd, .. } = &mut start {
        *pid = Some(4242);
        *cwd = Some("/tmp/ünïcode wt".into());
    }
    appender.send_input(start, t0).unwrap();
    assert!(appender.flush());
    let runs = appender.active_runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].pid, 4242);
    assert_eq!(runs[0].run_track, "agent:runner/run:1");

    appender
        .send_input(
            Input::RunEnd {
                source_ts_ns: Some(t0 + 10),
                session: SessionKey::new("claude", "sess-run"),
                exit_code: Some(0),
                reason: None,
            },
            t0 + 10,
        )
        .unwrap();
    assert!(appender.flush());
    assert!(appender.active_runs().is_empty());
    appender.shutdown();
    thread.join().unwrap();
}
