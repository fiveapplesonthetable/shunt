use super::*;
use std::io::Write;
use std::sync::Arc;
use tacho_daemon::{Config, Daemon, DaemonPaths};

#[test]
fn test_tail_claude_transcript_incremental() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("run");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let daemon = Daemon::start(paths, Config::default()).unwrap();

    let projects_dir = temp.path().join("projects").join("my-repo");
    fs::create_dir_all(&projects_dir).unwrap();
    let jsonl_path = projects_dir.join("sess-tail-1.jsonl");

    // Write first 2 lines of an on-disk Claude transcript
    {
        let mut f = File::create(&jsonl_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","sessionId":"sess-tail-1","timestamp":"2026-04-12T18:30:00.000Z","cwd":"/workspace/repo","message":{{"content":"Fix bug"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","sessionId":"sess-tail-1","timestamp":"2026-04-12T18:30:02.000Z","message":{{"model":"claude-opus-4-6","content":[{{"type":"tool_use","id":"toolu_t1","name":"Bash","input":{{"command":"cargo test"}}}}],"usage":{{"input_tokens":100,"output_tokens":25}}}}}}"#
        )
        .unwrap();
    }

    let client = DaemonClient::custom(&home, &runtime);
    let mut offsets = BTreeMap::new();
    let mut states = HashMap::new();

    let n1 = scan_and_tail_once(
        &temp.path().join("projects"),
        Some("worker-tail"),
        &client,
        &mut offsets,
        &mut states,
    )
    .unwrap();
    assert!(
        n1 >= 4,
        "expected RunStart + Prompt + TurnStart + ToolBegin + TurnEnd, got {n1}"
    );

    // Append tool_result line and tail again from fresh state map (testing offset resume)
    {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&jsonl_path)
            .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","sessionId":"sess-tail-1","timestamp":"2026-04-12T18:30:05.000Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_t1","content":"ok","is_error":false}}]}}}}"#
        )
        .unwrap();
    }

    let mut fresh_states = HashMap::new();
    let n2 = scan_and_tail_once(
        &temp.path().join("projects"),
        Some("worker-tail"),
        &client,
        &mut offsets,
        &mut fresh_states,
    )
    .unwrap();
    assert_eq!(n2, 1, "expected only the newly appended ToolEnd event");

    daemon.appender.flush();
    let live = daemon.appender.live_state();
    assert!(
        live.agents.contains_key("worker-tail"),
        "daemon should have recorded worker-tail from tailed transcript"
    );
}

#[test]
fn test_tail_partial_line_not_consumed_until_newline() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("run");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let daemon = Daemon::start(paths, Config::default()).unwrap();

    let projects_dir = temp.path().join("projects").join("my-repo");
    fs::create_dir_all(&projects_dir).unwrap();
    let jsonl_path = projects_dir.join("sess-partial.jsonl");

    let line1 = r#"{"type":"user","sessionId":"sess-partial","timestamp":"2026-04-12T18:30:00.000Z","cwd":"/workspace/repo","message":{"content":"Hello"}}"#;
    let line2_part1 =
        r#"{"type":"assistant","sessionId":"sess-partial","timestamp":"2026-04-12T18:30:02.000Z","#;
    let line2_part2 = r#""message":{"model":"claude-opus-4-6","content":[{"type":"tool_use","id":"toolu_p1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":10,"output_tokens":5}}}"#;

    // Write complete line 1, followed by incomplete line 2 (no trailing newline)
    {
        let mut f = File::create(&jsonl_path).unwrap();
        writeln!(f, "{line1}").unwrap();
        write!(f, "{line2_part1}").unwrap();
    }

    let client = DaemonClient::custom(&home, &runtime);
    let mut offsets = BTreeMap::new();
    let mut states = HashMap::new();

    let n1 = scan_and_tail_once(
        &temp.path().join("projects"),
        Some("worker-partial"),
        &client,
        &mut offsets,
        &mut states,
    )
    .unwrap();
    assert!(n1 > 0, "first complete line should be ingested");

    let key = jsonl_path.to_string_lossy().to_string();
    let offset1 = offsets.get(&key).expect("offset recorded").offset;
    let expected_line1_len = line1.len() as u64 + 1;
    assert_eq!(
        offset1, expected_line1_len,
        "offset must not advance past partial line"
    );

    // Append remaining fragment of line 2 with newline
    {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&jsonl_path)
            .unwrap();
        writeln!(f, "{line2_part2}").unwrap();
    }

    let n2 = scan_and_tail_once(
        &temp.path().join("projects"),
        Some("worker-partial"),
        &client,
        &mut offsets,
        &mut states,
    )
    .unwrap();
    assert!(n2 > 0, "completed line should now be ingested");

    let offset2 = offsets.get(&key).expect("offset recorded").offset;
    assert!(
        offset2 > offset1,
        "offset must advance after line completes"
    );

    daemon.appender.flush();
}

#[test]
fn test_tail_invalid_utf8_skipped_and_later_lines_ingested() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("run");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let daemon = Daemon::start(paths, Config::default()).unwrap();

    let projects_dir = temp.path().join("projects").join("my-repo");
    fs::create_dir_all(&projects_dir).unwrap();
    let jsonl_path = projects_dir.join("sess-utf8.jsonl");

    let line1 = r#"{"type":"user","sessionId":"sess-utf8","timestamp":"2026-04-12T18:30:00.000Z","cwd":"/workspace/repo","message":{"content":"Start"}}"#;
    let bad_line = b"{\"type\":\"corrupted\",\xFF\xFE\xFD,\"sessionId\":\"sess-utf8\"}\n";
    let line3 = r#"{"type":"assistant","sessionId":"sess-utf8","timestamp":"2026-04-12T18:30:02.000Z","message":{"model":"claude-opus-4-6","content":[{"type":"tool_use","id":"toolu_u1","name":"Bash","input":{"command":"pwd"}}],"usage":{"input_tokens":10,"output_tokens":5}}}"#;

    {
        let mut f = File::create(&jsonl_path).unwrap();
        writeln!(f, "{line1}").unwrap();
        f.write_all(bad_line).unwrap();
        writeln!(f, "{line3}").unwrap();
    }

    let client = DaemonClient::custom(&home, &runtime);
    let mut offsets = BTreeMap::new();
    let mut states = HashMap::new();

    let n = scan_and_tail_once(
        &temp.path().join("projects"),
        Some("worker-utf8"),
        &client,
        &mut offsets,
        &mut states,
    )
    .unwrap();
    assert!(
        n >= 4,
        "valid lines before and after invalid utf-8 must be ingested, got {n}"
    );

    let key = jsonl_path.to_string_lossy().to_string();
    let total_file_len = fs::metadata(&jsonl_path).unwrap().len();
    let final_offset = offsets.get(&key).expect("offset recorded").offset;
    assert_eq!(
        final_offset, total_file_len,
        "tailer must not get stuck and advance through end of file"
    );

    daemon.appender.flush();
}

#[test]
fn test_tail_truncation_and_rotation_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let runtime = temp.path().join("run");
    let paths = DaemonPaths::custom(&home, &runtime).unwrap();
    let daemon = Daemon::start(paths, Config::default()).unwrap();

    let projects_dir = temp.path().join("projects").join("my-repo");
    fs::create_dir_all(&projects_dir).unwrap();
    let jsonl_path = projects_dir.join("sess-rot.jsonl");

    let line_user = r#"{"type":"user","sessionId":"sess-rot","timestamp":"2026-04-12T18:30:00.000Z","cwd":"/workspace/repo","message":{"content":"Run 1"}}"#;
    let line_tool = r#"{"type":"assistant","sessionId":"sess-rot","timestamp":"2026-04-12T18:30:02.000Z","message":{"model":"claude-opus-4-6","content":[{"type":"tool_use","id":"toolu_r1","name":"Bash","input":{"command":"echo 1"}}],"usage":{"input_tokens":10,"output_tokens":5}}}"#;

    {
        let mut f = File::create(&jsonl_path).unwrap();
        writeln!(f, "{line_user}").unwrap();
        writeln!(f, "{line_tool}").unwrap();
    }

    let client = DaemonClient::custom(&home, &runtime);
    let mut offsets = BTreeMap::new();
    let mut states = HashMap::new();

    let n1 = scan_and_tail_once(
        &temp.path().join("projects"),
        Some("worker-rot"),
        &client,
        &mut offsets,
        &mut states,
    )
    .unwrap();
    assert!(n1 > 0);

    let key = jsonl_path.to_string_lossy().to_string();
    let orig_offset = offsets.get(&key).expect("offset recorded").offset;
    assert!(orig_offset > 0);

    // 1. Truncation: overwrite with shorter content
    let line_trunc = r#"{"type":"user","sessionId":"sess-rot","timestamp":"2026-04-12T18:30:10.000Z","cwd":"/workspace/repo","message":{"content":"Restarted short"}}"#;
    {
        let mut f = File::create(&jsonl_path).unwrap();
        writeln!(f, "{line_trunc}").unwrap();
    }
    let trunc_len = fs::metadata(&jsonl_path).unwrap().len();
    assert!(
        trunc_len < orig_offset,
        "new file length must be strictly less than previous offset to test truncation"
    );

    let n2 = scan_and_tail_once(
        &temp.path().join("projects"),
        Some("worker-rot"),
        &client,
        &mut offsets,
        &mut states,
    )
    .unwrap();
    assert!(
        n2 > 0,
        "truncated file should restart from 0 and ingest new content"
    );
    let new_offset = offsets.get(&key).expect("offset recorded").offset;
    assert_eq!(
        new_offset, trunc_len,
        "offset should match truncated file length"
    );

    // 2. Rotation: replace file with a newly created file (different inode)
    let rotated_path = projects_dir.join("sess-rot.tmp");
    let line_rotated = r#"{"type":"user","sessionId":"sess-rot","timestamp":"2026-04-12T18:30:20.000Z","cwd":"/workspace/repo","message":{"content":"Rotated file content with longer text to exceed trunc_len"}}"#;
    {
        let mut f = File::create(&rotated_path).unwrap();
        writeln!(f, "{line_rotated}").unwrap();
    }
    let orig_ino = offsets.get(&key).unwrap().inode;
    let new_ino = fs::metadata(&rotated_path).unwrap().ino();
    assert_ne!(
        orig_ino, new_ino,
        "replacement file must have different inode"
    );

    fs::rename(&rotated_path, &jsonl_path).unwrap();

    let n3 = scan_and_tail_once(
        &temp.path().join("projects"),
        Some("worker-rot"),
        &client,
        &mut offsets,
        &mut states,
    )
    .unwrap();
    assert!(
        n3 > 0,
        "rotated file should be detected by inode and ingested from 0"
    );
    let final_entry = offsets.get(&key).unwrap();
    assert_eq!(
        final_entry.inode, new_ino,
        "stored inode must update to new file inode"
    );

    daemon.appender.flush();
}

#[test]
fn test_tail_offsets_atomic_persistence_and_corrupted_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let offsets_file = temp.path().join("tail-offsets.json");

    // 1. Corrupted / truncated tail-offsets.json falls back cleanly to empty map
    fs::write(&offsets_file, b"{\"truncated\":").unwrap();
    assert!(load_offsets(&offsets_file).is_empty());

    let mut offsets = BTreeMap::new();
    offsets.insert(
        "/path/to/sess.jsonl".to_string(),
        StoredOffset {
            dev: 42,
            inode: 12345,
            offset: 999,
        },
    );

    save_offsets(&offsets_file, &offsets).unwrap();
    let loaded = load_offsets(&offsets_file);
    assert_eq!(loaded, offsets);

    // 2. Concurrent atomic save_offsets across 8 threads never leaves a corrupted file
    let path_arc = Arc::new(offsets_file.clone());
    let mut handles = Vec::new();
    for t in 0..8u64 {
        let p = Arc::clone(&path_arc);
        handles.push(std::thread::spawn(move || {
            for iter in 0..10u64 {
                let mut m = BTreeMap::new();
                m.insert(
                    format!("/path/t{t}.jsonl"),
                    StoredOffset {
                        dev: t,
                        inode: 1000 + iter,
                        offset: iter * 128,
                    },
                );
                save_offsets(&p, &m).unwrap();
                let read_back = fs::read_to_string(&*p).unwrap();
                let parsed: BTreeMap<String, StoredOffset> =
                    serde_json::from_str(&read_back).expect("atomic save must never tear JSON");
                assert!(!parsed.is_empty());
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // Verify temporary files are cleaned up
    let entries: Vec<_> = fs::read_dir(temp.path()).unwrap().flatten().collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].file_name(), "tail-offsets.json");
}

#[test]
fn test_daemon_send_error_keeps_offsets_unadvanced() {
    let temp = tempfile::tempdir().unwrap();
    let projects_dir = temp.path().join("projects").join("my-repo");
    fs::create_dir_all(&projects_dir).unwrap();
    let jsonl_path = projects_dir.join("sess-err.jsonl");

    {
        let mut f = File::create(&jsonl_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","sessionId":"sess-err","timestamp":"2026-04-12T18:30:00.000Z","cwd":"/workspace/repo","message":{{"content":"Will fail send"}}}}"#
        )
        .unwrap();
    }

    let bad_run = temp.path().join("bad_run");
    fs::write(&bad_run, b"not-a-directory").unwrap();
    let bad_client = DaemonClient::custom(temp.path().join("bad_home"), bad_run);
    let mut offsets = BTreeMap::new();
    let mut states = HashMap::new();

    let res = scan_and_tail_once(
        &temp.path().join("projects"),
        Some("worker-err"),
        &bad_client,
        &mut offsets,
        &mut states,
    );

    assert!(res.is_err(), "send should fail when daemon is unreachable");
    let key = jsonl_path.to_string_lossy().to_string();
    assert!(
        !offsets.contains_key(&key),
        "offsets must not be advanced when send fails"
    );
    assert!(
        !states.contains_key(&jsonl_path),
        "in-memory state must not retain partial progress when send fails"
    );
}

#[test]
fn test_follow_handle_stop_wakes_inotify_poll_and_add_dir_fails_loudly() {
    let temp = tempfile::tempdir().unwrap();
    let mut watcher = InotifyWatcher::new().expect("inotify init must succeed");
    watcher
        .add_dir(temp.path())
        .expect("watching existing dir must succeed");

    let missing_dir = temp.path().join("does-not-exist");
    let err = watcher
        .add_dir(&missing_dir)
        .expect_err("add_dir on nonexistent path must fail loudly");
    assert!(
        err.contains("inotify_add_watch failed"),
        "expected loud inotify_add_watch failure, got: {err}"
    );

    let handle = FollowHandle::new().expect("FollowHandle::new must succeed");
    let handle_for_thread = handle.clone();
    let join = std::thread::spawn(move || {
        watcher
            .read_events(handle_for_thread.wake_read_fd())
            .expect("read_events must unblock cleanly on FollowHandle::stop()")
    });

    handle.stop();
    let evs = join.join().expect("reader thread panicked");
    assert!(evs.is_empty());
    assert!(handle.is_stopped());
}
