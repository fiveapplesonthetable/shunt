//! Live transcript tailer (`tacho tail [PATH] [--once] [--agent NAME]`).
//!
//! Tails Claude Code on-disk `.jsonl` transcripts (defaulting to `~/.claude/projects`),
//! tracks per-file byte offsets in `$TACHO_HOME/tail-offsets.json`, uses Linux `inotify`
//! for low-latency wakeups on file writes, and streams parsed `Input` batches to the daemon.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tacho_ingest::streams::claude::StreamParser;

use crate::cli::TailArgs;
use crate::client::DaemonClient;

/// Handle for stopping a live `tacho tail` follow loop immediately via an `AtomicBool`
/// and a non-blocking wakeup self-pipe (`pipe2(O_CLOEXEC | O_NONBLOCK)`).
#[derive(Clone)]
pub struct FollowHandle {
    stop: Arc<AtomicBool>,
    wake_read: Arc<OwnedFd>,
    wake_write: Arc<OwnedFd>,
}

impl FollowHandle {
    /// Creates a new `FollowHandle` backed by a non-blocking wakeup self-pipe.
    pub fn new() -> io::Result<Self> {
        let mut fds = [-1i32; 2];
        // SAFETY: `fds` is a valid 2-element `[c_int; 2]` buffer.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `pipe2` succeeded and returned two fresh owned file descriptors.
        let (read_fd, write_fd) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Ok(Self {
            stop: Arc::new(AtomicBool::new(false)),
            wake_read: Arc::new(read_fd),
            wake_write: Arc::new(write_fd),
        })
    }

    /// Signals the follow loop to stop and writes `1u8` to the wakeup pipe to unblock `poll(..., -1)`.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let byte: u8 = 1;
        // SAFETY: `self.wake_write` is a valid open non-blocking pipe file descriptor.
        let _ = unsafe {
            libc::write(
                self.wake_write.as_raw_fd(),
                (&byte as *const u8).cast::<libc::c_void>(),
                1,
            )
        };
    }

    /// Returns `true` if stop has been requested.
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Raw file descriptor for the read end of the wakeup self-pipe.
    pub fn wake_read_fd(&self) -> RawFd {
        self.wake_read.as_raw_fd()
    }

    /// Shared `AtomicBool` stop flag.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Registers `SIGINT` and `SIGTERM` to set the stop flag and write to the wakeup pipe.
    pub fn register_signals(&self) -> io::Result<()> {
        signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&self.stop))?;
        signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&self.stop))?;
        signal_hook::low_level::pipe::register(
            signal_hook::consts::SIGINT,
            self.wake_write.try_clone()?,
        )?;
        signal_hook::low_level::pipe::register(
            signal_hook::consts::SIGTERM,
            self.wake_write.try_clone()?,
        )?;
        Ok(())
    }
}

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
    let handle = FollowHandle::new().map_err(|e| format!("failed to create wakeup pipe: {e}"))?;
    if !args.once {
        handle
            .register_signals()
            .map_err(|e| format!("failed to register signal handlers: {e}"))?;
    }
    execute_tail_with_handle(args, handle)
}

/// Executes `tacho tail` using an explicit [`FollowHandle`] for event-driven stop control.
pub fn execute_tail_with_handle(args: TailArgs, handle: FollowHandle) -> Result<(), String> {
    let client = DaemonClient::new();
    let home = crate::paths::resolve_home();
    crate::paths::ensure_dir_0700(&home).map_err(|e| e.to_string())?;
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
        save_offsets(&offsets_path, &saved_offsets)?;
        println!(
            "Tailed {total_initial} events from {}.",
            target_path.display()
        );
        return Ok(());
    }

    let mut watcher = InotifyWatcher::new()?;
    register_target_watches(&mut watcher, &target_path)?;

    let initial_files = collect_jsonl_files(&target_path);
    let mut pending_retry_files = BTreeSet::new();

    match tail_files(
        &initial_files,
        default_agent.as_deref(),
        &client,
        &mut saved_offsets,
        &mut states,
    ) {
        Ok(n) => {
            if n > 0 {
                save_offsets(&offsets_path, &saved_offsets)?;
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to send tailed inputs to daemon: {e}; retrying...");
            pending_retry_files.extend(initial_files);
        }
    }

    let runtime_dir = client
        .api_sock()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::paths::resolve_runtime);

    if !pending_retry_files.is_empty() && !handle.is_stopped() {
        wait_for_retry_or_stop(
            &runtime_dir,
            watcher.raw_fd(),
            handle.wake_read_fd(),
            &handle.stop,
        )?;
    }

    while !handle.is_stopped() {
        let events = if pending_retry_files.is_empty() {
            watcher.read_events(handle.wake_read_fd())?
        } else {
            watcher.drain_events()?
        };

        if handle.is_stopped() {
            break;
        }

        let mut files_to_process = std::mem::take(&mut pending_retry_files);

        for (mask, path) in events {
            if (mask & libc::IN_CREATE) != 0 && path.is_dir() {
                watcher.add_dir(&path)?;
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
                    save_offsets(&offsets_path, &saved_offsets)?;
                }
            }
            Err(e) => {
                eprintln!("Warning: failed to send tailed inputs to daemon: {e}; retrying...");
                pending_retry_files.extend(file_list);
                wait_for_retry_or_stop(
                    &runtime_dir,
                    watcher.raw_fd(),
                    handle.wake_read_fd(),
                    &handle.stop,
                )?;
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

fn register_target_watches(watcher: &mut InotifyWatcher, target: &Path) -> Result<(), String> {
    if target.is_file() {
        let parent = target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        watcher.add_dir(parent)?;
    } else if target.is_dir() {
        watcher.add_dir(target)?;
        let entries = fs::read_dir(target)
            .map_err(|e| format!("failed to read directory '{}': {e}", target.display()))?;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                watcher.add_dir(&p)?;
            }
        }
    } else if let Some(parent) = target.parent().filter(|p| p.is_dir()) {
        watcher.add_dir(parent)?;
    } else {
        return Err(format!("target path '{}' does not exist", target.display()));
    }
    Ok(())
}

/// Blocks on `poll` until the daemon runtime directory changes (`api.sock` restart),
/// a transcript file changes, or the stop wakeup pipe fires.
fn wait_for_retry_or_stop(
    runtime_dir: &Path,
    transcript_inotify_fd: RawFd,
    wake_fd: RawFd,
    stop: &AtomicBool,
) -> Result<(), String> {
    if stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let watch = crate::client::watch_dir(runtime_dir);
    let mut pfds = [
        libc::pollfd {
            fd: watch.as_ref().map_or(-1, |w| w.as_raw_fd()),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: transcript_inotify_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let timeout_ms = if watch.is_some() && transcript_inotify_fd >= 0 {
        -1
    } else {
        10
    };
    // SAFETY: `pfds` contains valid file descriptors (or -1 ignored by poll).
    let res = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
    if res < 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(format!("poll failed while waiting for daemon retry: {err}"));
        }
    } else if (pfds[2].revents & libc::POLLIN) != 0 {
        drain_pipe_fd(wake_fd);
    }
    Ok(())
}

fn drain_pipe_fd(fd: RawFd) {
    let mut buf = [0u8; 64];
    // SAFETY: `fd` is a valid non-blocking pipe file descriptor and `buf` is a local array.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) } > 0 {}
}

/// Thin wrapper managing Linux inotify file descriptor and watches.
struct InotifyWatcher {
    fd: i32,
    wd_to_dir: HashMap<i32, PathBuf>,
    dir_to_wd: HashMap<PathBuf, i32>,
}

impl InotifyWatcher {
    /// Initializes a non-blocking inotify instance with `FD_CLOEXEC`, falling back to
    /// `fd = -1` when the host is out of inotify instances.
    fn new() -> Result<Self, String> {
        // SAFETY: libc::inotify_init1 takes valid flags. IN_NONBLOCK ensures non-blocking reads,
        // and IN_CLOEXEC prevents file descriptor leaks across process execution.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        Ok(Self {
            fd,
            wd_to_dir: HashMap::new(),
            dir_to_wd: HashMap::new(),
        })
    }

    fn raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Adds a directory watch descriptor for transcript modifications and directory creation.
    fn add_dir(&mut self, dir: &Path) -> Result<(), String> {
        if self.dir_to_wd.contains_key(dir) {
            return Ok(());
        }
        if self.fd < 0 {
            if !dir.is_dir() {
                return Err(format!(
                    "inotify_add_watch failed for '{}': No such file or directory",
                    dir.display()
                ));
            }
            let synthetic_wd = (self.dir_to_wd.len() as i32) + 1;
            self.wd_to_dir.insert(synthetic_wd, dir.to_path_buf());
            self.dir_to_wd.insert(dir.to_path_buf(), synthetic_wd);
            return Ok(());
        }
        let c_path = CString::new(dir.as_os_str().as_bytes())
            .map_err(|e| format!("invalid directory path '{}': {e}", dir.display()))?;
        const WATCH_MASK: u32 = libc::IN_MODIFY
            | libc::IN_CLOSE_WRITE
            | libc::IN_CREATE
            | libc::IN_MOVED_TO
            | libc::IN_IGNORED;
        // SAFETY: self.fd is a valid inotify file descriptor, and c_path is a null-terminated C string.
        let wd = unsafe { libc::inotify_add_watch(self.fd, c_path.as_ptr(), WATCH_MASK) };
        if wd < 0 {
            let err = io::Error::last_os_error();
            return Err(format!(
                "inotify_add_watch failed for '{}': {err}",
                dir.display()
            ));
        }
        self.wd_to_dir.insert(wd, dir.to_path_buf());
        self.dir_to_wd.insert(dir.to_path_buf(), wd);
        Ok(())
    }

    /// Blocks in `poll` on the inotify descriptor and `wake_fd`, then drains and
    /// returns parsed inotify events.
    fn read_events(&mut self, wake_fd: RawFd) -> Result<Vec<(u32, PathBuf)>, String> {
        let mut pfds = [
            libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout_ms = if self.fd >= 0 { -1 } else { 10 };
        // SAFETY: `pfds` points to 2 descriptors (`self.fd` or -1, and `wake_fd`).
        let res = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
        if res < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(format!("inotify poll failed: {err}"));
        }
        if (pfds[1].revents & libc::POLLIN) != 0 {
            drain_pipe_fd(wake_fd);
            return Ok(Vec::new());
        }
        if self.fd < 0 {
            return self.drain_events();
        }
        if res == 0 || (pfds[0].revents & libc::POLLIN) == 0 {
            return Ok(Vec::new());
        }

        self.drain_events()
    }

    /// Reads all currently queued events from the non-blocking inotify descriptor.
    fn drain_events(&mut self) -> Result<Vec<(u32, PathBuf)>, String> {
        if self.fd < 0 {
            let mut events = Vec::new();
            let dirs: Vec<PathBuf> = self.dir_to_wd.keys().cloned().collect();
            for dir in dirs {
                if let Ok(entries) = fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if p.is_dir() {
                            events.push((libc::IN_CREATE, p));
                        } else {
                            events.push((libc::IN_MODIFY, p));
                        }
                    }
                }
            }
            return Ok(events);
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
mod tests;
