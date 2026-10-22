//! Live transcript tailer (`tacho tail [PATH] [--once] [--agent NAME]`).
//!
//! Tails Claude Code on-disk `.jsonl` transcripts (defaulting to `~/.claude/projects`),
//! tracks per-file byte offsets in `$TACHO_HOME/tail-offsets.json`, uses Linux `inotify`
//! for low-latency wakeups on file writes, and streams parsed `Input` batches to the daemon.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tacho_ingest::streams::claude::StreamParser;

use crate::cli::TailArgs;
use crate::client::DaemonClient;

struct FileTailState {
    offset: u64,
    parser: StreamParser,
}

/// Executes `tacho tail`.
pub fn execute_tail(args: TailArgs) -> Result<(), String> {
    let client = DaemonClient::new();
    let home = crate::paths::resolve_home();
    let _ = crate::paths::ensure_dir_0700(&home);
    let offsets_path = home.join("tail-offsets.json");

    let target_path = match args.path {
        Some(p) => p,
        None => {
            let user_home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| "HOME not set and no path specified".to_string())?;
            user_home.join(".claude").join("projects")
        }
    };

    let mut saved_offsets: BTreeMap<String, u64> = load_offsets(&offsets_path);
    let mut states: HashMap<PathBuf, FileTailState> = HashMap::new();

    let total_initial = scan_and_tail_once(
        &target_path,
        args.agent.as_deref(),
        &client,
        &mut saved_offsets,
        &mut states,
    )?;
    save_offsets(&offsets_path, &saved_offsets);

    if args.once {
        println!(
            "Tailed {total_initial} events from {}.",
            target_path.display()
        );
        return Ok(());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop));

    let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    let mut watched_dirs = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        if inotify_fd >= 0 {
            register_inotify_watches(inotify_fd, &target_path, &mut watched_dirs);
            wait_inotify_or_timeout(inotify_fd, 250);
        } else {
            std::thread::sleep(Duration::from_millis(250));
        }

        let n = scan_and_tail_once(
            &target_path,
            args.agent.as_deref(),
            &client,
            &mut saved_offsets,
            &mut states,
        )?;
        if n > 0 {
            save_offsets(&offsets_path, &saved_offsets);
        }
    }

    if inotify_fd >= 0 {
        unsafe {
            libc::close(inotify_fd);
        }
    }
    Ok(())
}

fn scan_and_tail_once(
    target_path: &Path,
    agent_override: Option<&str>,
    client: &DaemonClient,
    saved_offsets: &mut BTreeMap<String, u64>,
    states: &mut HashMap<PathBuf, FileTailState>,
) -> Result<usize, String> {
    let files = collect_jsonl_files(target_path);
    let mut emitted_count = 0usize;
    let mut pending_inputs = Vec::new();

    for file_path in files {
        let key = file_path.to_string_lossy().to_string();
        let meta = match fs::metadata(&file_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let file_len = meta.len();

        let saved_offset = saved_offsets.get(&key).copied().unwrap_or(0);
        let effective_saved = if file_len < saved_offset {
            states.remove(&file_path);
            0
        } else {
            saved_offset
        };

        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        let default_agent = agent_override
            .map(String::from)
            .or_else(|| std::env::var("TACHO_AGENT").ok().filter(|s| !s.is_empty()));

        let state = states
            .entry(file_path.clone())
            .or_insert_with(|| FileTailState {
                offset: 0,
                parser: StreamParser::for_transcript(None, default_agent.clone()),
            });

        if file_len <= effective_saved && state.offset == effective_saved {
            continue;
        }

        let mut file = match File::open(&file_path) {
            Ok(f) => f,
            Err(_) => continue,
        };

        // If state.offset < effective_saved (fresh process resuming from saved_offsets),
        // fast-forward parser state through 0..effective_saved without re-emitting.
        if state.offset < effective_saved {
            let _ = file.seek(SeekFrom::Start(state.offset));
            let mut reader = BufReader::new(&mut file);
            let mut line = String::new();
            while state.offset < effective_saved {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(n) => {
                        if !line.ends_with('\n') {
                            break;
                        }
                        state.offset += n as u64;
                        let _ = state.parser.feed_line(&line, now_ns);
                    }
                    Err(_) => break,
                }
            }
        }

        if file.seek(SeekFrom::Start(state.offset)).is_err() {
            continue;
        }
        let mut reader = BufReader::new(&mut file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(n) => {
                    // Only advance offset on complete newline-terminated JSONL lines
                    if !line.ends_with('\n') {
                        break;
                    }
                    state.offset += n as u64;
                    let now_ns = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    let inputs = state.parser.feed_line(&line, now_ns);
                    emitted_count += inputs.len();
                    pending_inputs.extend(inputs);
                }
                Err(_) => break,
            }
        }

        saved_offsets.insert(key, state.offset);
    }

    if !pending_inputs.is_empty() {
        client
            .send_inputs_batch(&pending_inputs)
            .map_err(|e| format!("failed to send tailed inputs to daemon: {e}"))?;
    }

    Ok(emitted_count)
}

fn collect_jsonl_files(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if target.is_file() {
        out.push(target.to_path_buf());
        return out;
    }
    if !target.is_dir() {
        return out;
    }
    if let Ok(entries) = fs::read_dir(target) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(p);
            } else if p.is_dir() {
                if let Ok(sub_entries) = fs::read_dir(&p) {
                    for sub in sub_entries.flatten() {
                        let sp = sub.path();
                        if sp.is_file() && sp.extension().and_then(|e| e.to_str()) == Some("jsonl")
                        {
                            out.push(sp);
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn load_offsets(path: &Path) -> BTreeMap<String, u64> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_offsets(path: &Path, offsets: &BTreeMap<String, u64>) {
    if let Ok(json) = serde_json::to_string_pretty(offsets) {
        let _ = fs::write(path, json);
    }
}

fn register_inotify_watches(inotify_fd: i32, target: &Path, watched: &mut Vec<PathBuf>) {
    let mut dirs = Vec::new();
    if target.is_file() {
        if let Some(parent) = target.parent() {
            dirs.push(parent.to_path_buf());
        }
    } else if target.is_dir() {
        dirs.push(target.to_path_buf());
        if let Ok(entries) = fs::read_dir(target) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                }
            }
        }
    }

    for dir in dirs {
        if !watched.contains(&dir) {
            if let Ok(c_path) = CString::new(dir.as_os_str().as_bytes()) {
                let wd = unsafe {
                    libc::inotify_add_watch(
                        inotify_fd,
                        c_path.as_ptr(),
                        libc::IN_MODIFY | libc::IN_CREATE | libc::IN_MOVED_TO,
                    )
                };
                if wd >= 0 {
                    watched.push(dir);
                }
            }
        }
    }
}

fn wait_inotify_or_timeout(inotify_fd: i32, timeout_ms: i32) {
    let mut pfd = libc::pollfd {
        fd: inotify_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let res = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if res > 0 && (pfd.revents & libc::POLLIN) != 0 {
        let mut buf = [0u8; 4096];
        loop {
            let n =
                unsafe { libc::read(inotify_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
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
}
