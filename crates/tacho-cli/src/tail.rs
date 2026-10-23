//! Live transcript tailer (`tacho tail [PATH] [--once] [--agent NAME]`).
//!
//! Tails Claude Code on-disk `.jsonl` transcripts (defaulting to `~/.claude/projects`),
//! tracks per-file byte offsets in `$TACHO_HOME/tail-offsets.json`, uses Linux `inotify`
//! for low-latency wakeups on file writes, and streams parsed `Input` batches to the daemon.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tacho_ingest::streams::claude::StreamParser;

use crate::cli::TailArgs;
use crate::client::DaemonClient;

/// Persisted offset entry for a tracked transcript file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StoredOffset {
    pub dev: u64,
    pub inode: u64,
    pub offset: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredOffsetDe {
    Full {
        #[serde(default)]
        dev: u64,
        #[serde(default, alias = "ino")]
        inode: u64,
        offset: u64,
    },
    Tuple(u64, u64, u64),
    LegacyOffset(u64),
}

impl<'de> Deserialize<'de> for StoredOffset {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match StoredOffsetDe::deserialize(deserializer)? {
            StoredOffsetDe::Full { dev, inode, offset } => Ok(StoredOffset { dev, inode, offset }),
            StoredOffsetDe::Tuple(dev, inode, offset) => Ok(StoredOffset { dev, inode, offset }),
            StoredOffsetDe::LegacyOffset(offset) => Ok(StoredOffset {
                dev: 0,
                inode: 0,
                offset,
            }),
        }
    }
}

struct FileTailState {
    dev: u64,
    inode: u64,
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

    // Read agent override or TACHO_AGENT exactly once
    let default_agent: Option<String> = args
        .agent
        .or_else(|| std::env::var("TACHO_AGENT").ok())
        .filter(|s| !s.trim().is_empty());

    let mut saved_offsets: BTreeMap<String, StoredOffset> = load_offsets(&offsets_path);
    let mut states: HashMap<PathBuf, FileTailState> = HashMap::new();

    if args.once {
        let total_initial = scan_and_tail_once(
            &target_path,
            default_agent.as_deref(),
            &client,
            &mut saved_offsets,
            &mut states,
        )?;
        let _ = save_offsets(&offsets_path, &saved_offsets);
        println!(
            "Tailed {total_initial} events from {}.",
            target_path.display()
        );
        return Ok(());
    }

    let mut watcher = InotifyWatcher::new()?;
    register_target_watches(&mut watcher, &target_path);

    let initial_files = collect_jsonl_files(&target_path);
    let mut pending_retry_files = BTreeSet::new();
    let mut backoff = Duration::from_millis(250);

    match tail_files(
        &initial_files,
        default_agent.as_deref(),
        &client,
        &mut saved_offsets,
        &mut states,
    ) {
        Ok(n) => {
            if n > 0 {
                let _ = save_offsets(&offsets_path, &saved_offsets);
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to send tailed inputs to daemon: {e}; retrying...");
            pending_retry_files.extend(initial_files);
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop));

    while !stop.load(Ordering::Relaxed) {
        let timeout_ms = backoff.as_millis().min(5000) as i32;
        let events = match watcher.read_events(timeout_ms) {
            Ok(evs) => evs,
            Err(e) => {
                eprintln!("inotify error: {e}");
                Vec::new()
            }
        };

        let mut files_to_process = std::mem::take(&mut pending_retry_files);
        if watcher.is_polling_fallback() {
            files_to_process.extend(collect_jsonl_files(&target_path));
        }

        for (mask, path) in events {
            if (mask & libc::IN_CREATE) != 0 && path.is_dir() {
                watcher.add_dir(&path);
                if let Ok(entries) = fs::read_dir(&path) {
                    for entry in entries.flatten() {
                        let ep = entry.path();
                        if is_matching_jsonl_file(&ep, &target_path) {
                            files_to_process.insert(ep);
                        }
                    }
                }
            } else if (mask
                & (libc::IN_MODIFY | libc::IN_CLOSE_WRITE | libc::IN_CREATE | libc::IN_MOVED_TO))
                != 0
                && is_matching_jsonl_file(&path, &target_path)
            {
                files_to_process.insert(path);
            }
        }

        if files_to_process.is_empty() {
            continue;
        }

        let file_list: Vec<PathBuf> = files_to_process.into_iter().collect();
        match tail_files(
            &file_list,
            default_agent.as_deref(),
            &client,
            &mut saved_offsets,
            &mut states,
        ) {
            Ok(emitted) => {
                if emitted > 0 {
                    let _ = save_offsets(&offsets_path, &saved_offsets);
                }
                backoff = Duration::from_millis(250);
            }
            Err(e) => {
                eprintln!("Warning: failed to send tailed inputs to daemon: {e}; retrying...");
                pending_retry_files.extend(file_list);
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }

    Ok(())
}

/// Scans the target path for all JSONL transcripts and tails newly available data once.
fn scan_and_tail_once(
    target_path: &Path,
    agent_override: Option<&str>,
    client: &DaemonClient,
    saved_offsets: &mut BTreeMap<String, StoredOffset>,
    states: &mut HashMap<PathBuf, FileTailState>,
) -> Result<usize, String> {
    let files = collect_jsonl_files(target_path);
    tail_files(&files, agent_override, client, saved_offsets, states)
}

fn tail_files(
    files: &[PathBuf],
    agent_override: Option<&str>,
    client: &DaemonClient,
    saved_offsets: &mut BTreeMap<String, StoredOffset>,
    states: &mut HashMap<PathBuf, FileTailState>,
) -> Result<usize, String> {
    let mut staged_results = Vec::new();
    let mut pending_inputs = Vec::new();

    for file_path in files {
        let key = file_path.to_string_lossy().to_string();
        let meta = match fs::metadata(file_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let file_dev = meta.dev();
        let file_ino = meta.ino();
        let file_len = meta.len();

        let saved_entry = saved_offsets.get(&key).copied();
        let (effective_saved, rotation_detected) = match saved_entry {
            Some(saved) => {
                let inode_changed = saved.inode != 0
                    && (saved.inode != file_ino || (saved.dev != 0 && saved.dev != file_dev));
                let truncated = file_len < saved.offset;
                if inode_changed || truncated {
                    (0, true)
                } else {
                    (saved.offset, false)
                }
            }
            None => (0, false),
        };

        if rotation_detected {
            states.remove(file_path);
        } else if let Some(state) = states.get(file_path) {
            let inode_changed = state.inode != 0
                && (state.inode != file_ino || (state.dev != 0 && state.dev != file_dev));
            let truncated = file_len < state.offset;
            if inode_changed || truncated {
                states.remove(file_path);
            }
        }

        let current_offset = states
            .get(file_path)
            .map(|s| s.offset)
            .unwrap_or(effective_saved);

        if file_len <= current_offset && !rotation_detected {
            continue;
        }

        let mut file = match File::open(file_path) {
            Ok(f) => f,
            Err(_) => continue,
        };

        let mut state = states.remove(file_path).unwrap_or_else(|| FileTailState {
            dev: file_dev,
            inode: file_ino,
            offset: 0,
            parser: StreamParser::for_transcript(None, agent_override.map(String::from)),
        });

        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        // If resuming from saved_offsets, fast-forward parser state through 0..effective_saved without re-emitting.
        if state.offset < effective_saved && file.seek(SeekFrom::Start(state.offset)).is_ok() {
            let mut reader = BufReader::new(&mut file);
            let mut line_buf = Vec::new();
            while state.offset < effective_saved {
                line_buf.clear();
                match reader.read_until(b'\n', &mut line_buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if line_buf.last() != Some(&b'\n') {
                            break;
                        }
                        state.offset += n as u64;
                        let line_str = String::from_utf8_lossy(&line_buf);
                        let _ = state.parser.feed_line(&line_str, now_ns);
                    }
                    Err(_) => break,
                }
            }
        }

        if file.seek(SeekFrom::Start(state.offset)).is_err() {
            states.insert(file_path.clone(), state);
            continue;
        }

        let mut reader = BufReader::new(&mut file);
        let mut line_buf = Vec::new();
        let mut line_offset = state.offset;
        let mut file_inputs = Vec::new();

        loop {
            line_buf.clear();
            match reader.read_until(b'\n', &mut line_buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Only consume complete lines; keep partial trailing line for next wake
                    if line_buf.last() != Some(&b'\n') {
                        break;
                    }
                    line_offset += n as u64;
                    let line_str = String::from_utf8_lossy(&line_buf);
                    let now_ns = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    let inputs = state.parser.feed_line(&line_str, now_ns);
                    file_inputs.extend(inputs);
                }
                Err(_) => break,
            }
        }

        state.offset = line_offset;
        state.dev = file_dev;
        state.inode = file_ino;

        staged_results.push((
            file_path.clone(),
            key,
            StoredOffset {
                dev: file_dev,
                inode: file_ino,
                offset: line_offset,
            },
            state,
        ));
        pending_inputs.extend(file_inputs);
    }

    if !pending_inputs.is_empty() {
        if let Err(e) = client.send_inputs_batch(&pending_inputs) {
            // Unsent lines leave offsets un-advanced; drop staged states so they rebuild on retry
            return Err(format!("failed to send tailed inputs to daemon: {e}"));
        }
    }

    let emitted_count = pending_inputs.len();
    for (path, key, stored, state) in staged_results {
        saved_offsets.insert(key, stored);
        states.insert(path, state);
    }

    Ok(emitted_count)
}

fn is_matching_jsonl_file(path: &Path, target: &Path) -> bool {
    if target.is_file() {
        path == target
    } else {
        !path.is_dir() && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
    }
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

fn load_offsets(path: &Path) -> BTreeMap<String, StoredOffset> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_offsets(path: &Path, offsets: &BTreeMap<String, StoredOffset>) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create directory '{}': {e}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(offsets)
        .map_err(|e| format!("Failed to serialize offsets: {e}"))?;

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "tail-offsets.json".to_string());
    let pid = std::process::id();
    let thread_id = format!("{:?}", std::thread::current().id());
    let clean_tid: String = thread_id.chars().filter(|c| c.is_alphanumeric()).collect();
    let tmp_path = parent.join(format!(".{}.tmp.{}.{}", file_name, pid, clean_tid));

    {
        let mut tmp_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|e| {
                format!(
                    "Failed to create temporary offsets file '{}': {e}",
                    tmp_path.display()
                )
            })?;

        tmp_file
            .write_all(format!("{json}\n").as_bytes())
            .map_err(|e| {
                format!(
                    "Failed to write temporary offsets file '{}': {e}",
                    tmp_path.display()
                )
            })?;
        tmp_file.flush().map_err(|e| {
            format!(
                "Failed to flush temporary offsets file '{}': {e}",
                tmp_path.display()
            )
        })?;
        tmp_file.sync_all().map_err(|e| {
            format!(
                "Failed to fsync temporary offsets file '{}': {e}",
                tmp_path.display()
            )
        })?;
    }

    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        format!(
            "Failed to atomically rename '{}' to '{}': {e}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

fn register_target_watches(watcher: &mut InotifyWatcher, target: &Path) {
    if target.is_file() {
        if let Some(parent) = target.parent() {
            watcher.add_dir(parent);
        }
    } else if target.is_dir() {
        watcher.add_dir(target);
        if let Ok(entries) = fs::read_dir(target) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    watcher.add_dir(&p);
                }
            }
        }
    }
}

/// Thin wrapper managing Linux inotify file descriptor and watches.
struct InotifyWatcher {
    fd: i32,
    wd_to_dir: HashMap<i32, PathBuf>,
    dir_to_wd: HashMap<PathBuf, i32>,
}

impl InotifyWatcher {
    /// Initializes non-blocking inotify instance with `FD_CLOEXEC`, falling back to
    /// poll-driven directory scans if the user or system inotify limit is exhausted.
    fn new() -> Result<Self, String> {
        // SAFETY: libc::inotify_init1 takes valid flags. IN_NONBLOCK ensures poll-driven non-blocking I/O,
        // and IN_CLOEXEC prevents file descriptor leaks across process execution.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        Ok(Self {
            fd,
            wd_to_dir: HashMap::new(),
            dir_to_wd: HashMap::new(),
        })
    }

    fn is_polling_fallback(&self) -> bool {
        self.fd < 0
    }

    /// Adds a directory watch descriptor for transcript modifications and directory creation.
    fn add_dir(&mut self, dir: &Path) -> bool {
        if self.fd < 0 {
            return false;
        }
        if self.dir_to_wd.contains_key(dir) {
            return true;
        }
        let c_path = match CString::new(dir.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return false,
        };
        const WATCH_MASK: u32 = libc::IN_MODIFY
            | libc::IN_CLOSE_WRITE
            | libc::IN_CREATE
            | libc::IN_MOVED_TO
            | libc::IN_IGNORED;
        // SAFETY: self.fd is a valid inotify file descriptor, and c_path is a null-terminated C string.
        let wd = unsafe { libc::inotify_add_watch(self.fd, c_path.as_ptr(), WATCH_MASK) };
        if wd < 0 {
            return false;
        }
        self.wd_to_dir.insert(wd, dir.to_path_buf());
        self.dir_to_wd.insert(dir.to_path_buf(), wd);
        true
    }

    /// Polls for inotify events with `timeout_ms` and returns parsed events.
    fn read_events(&mut self, timeout_ms: i32) -> Result<Vec<(u32, PathBuf)>, String> {
        if self.fd < 0 {
            // Poll-based fallback when inotify instances are exhausted
            unsafe {
                let _ = libc::poll(std::ptr::null_mut(), 0, timeout_ms.min(100));
            }
            return Ok(Vec::new());
        }
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: &mut pfd points to a valid pollfd struct for exactly 1 descriptor.
        let res = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if res < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(format!("inotify poll failed: {err}"));
        }
        if res == 0 || (pfd.revents & libc::POLLIN) == 0 {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            // SAFETY: self.fd is open and valid. buf is a valid writable memory buffer.
            let n =
                unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock
                    || err.kind() == io::ErrorKind::Interrupted
                {
                    break;
                }
                return Err(format!("inotify read failed: {err}"));
            }
            if n == 0 {
                break;
            }

            let num_bytes = n as usize;
            let mut offset = 0;
            let header_size = std::mem::size_of::<libc::inotify_event>();

            while offset + header_size <= num_bytes {
                let mut header_bytes = [0u8; std::mem::size_of::<libc::inotify_event>()];
                header_bytes.copy_from_slice(&buf[offset..offset + header_size]);
                // SAFETY: libc::inotify_event contains only integer primitives (wd, mask, cookie, len)
                // without invalid bit patterns, and header_bytes matches the struct's size exactly.
                let event: libc::inotify_event = unsafe { std::mem::transmute(header_bytes) };

                let name_len = event.len as usize;
                let total_size = header_size + name_len;
                if offset + total_size > num_bytes {
                    break;
                }

                if (event.mask & libc::IN_IGNORED) != 0 {
                    if let Some(removed_dir) = self.wd_to_dir.remove(&event.wd) {
                        self.dir_to_wd.remove(&removed_dir);
                    }
                }

                if name_len > 0 {
                    let name_bytes = &buf[offset + header_size..offset + total_size];
                    let actual_len = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_len);
                    let name_os = OsStr::from_bytes(&name_bytes[..actual_len]);
                    if let Some(parent_dir) = self.wd_to_dir.get(&event.wd) {
                        let full_path = parent_dir.join(name_os);
                        events.push((event.mask, full_path));
                    }
                }

                offset += total_size;
            }
        }

        Ok(events)
    }
}

impl Drop for InotifyWatcher {
    fn drop(&mut self) {
        if self.fd >= 0 {
            // SAFETY: self.fd was created by inotify_init1 and is valid to close.
            let _ = unsafe { libc::close(self.fd) };
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
        let line2_part1 = r#"{"type":"assistant","sessionId":"sess-partial","timestamp":"2026-04-12T18:30:02.000Z","#;
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
        let expected_line1_len = line1.len() as u64 + 1; // including '\n'
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
    fn test_tail_offsets_atomic_persistence() {
        let temp = tempfile::tempdir().unwrap();
        let offsets_file = temp.path().join("tail-offsets.json");

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

        // Point client to non-existent runtime dir where daemon is not running
        let bad_client =
            DaemonClient::custom(temp.path().join("bad_home"), temp.path().join("bad_run"));
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
}
