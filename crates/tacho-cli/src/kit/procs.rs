//! `tacho job`, `tacho wait`, `tacho kill`, and `tacho note`:
//! detached process groups with `(pid, starttime)` metadata, kernel event waiting (`pidfd_open` +
//! `inotify` + `poll`, kernel event-driven) supporting both job names and numeric PIDs, and
//! strict `kill` verification of `(pid, starttime)`, `/proc/<pid>/cwd` worktree ownership, and `pgid == pid`.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tacho_core::decide::watch::procs::owns;
use tacho_core::decide::watch::{parse_note, with_note};

use super::{
    emit_decision_event, find_worktree_root, load_worktree_env, now_secs, open_fleet_store,
    write_atomic, FdEventWatcher,
};
use crate::cli::{JobArgs, KillArgs, NoteArgs, WaitArgs};

/// Hard ceiling on `tacho wait` blocking duration (9 minutes = 540 seconds).
pub const MAX_WAIT_SECS: u64 = 540;
const HEARTBEAT_INTERVAL_MS: u128 = 60_000;

/// Validate a job name: must be `1..=40` characters matching `^[a-z0-9][a-z0-9._-]*$`
/// (no slashes, no `..`, no control characters).
pub fn validate_job_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 40 {
        return Err(format!(
            "invalid job name '{name}': length must be between 1 and 40 characters"
        ));
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(format!(
            "invalid job name '{name}': path separators and '..' are not allowed"
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!(
            "invalid job name '{name}': must start with a lowercase ASCII letter or digit"
        ));
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!("invalid job name '{name}': only lowercase ASCII letters, digits, '.', '_', and '-' are allowed"));
    }
    Ok(())
}

/// Format `(pid, starttime)` for `.tachorun/<name>.pid`.
pub fn format_pid_record(pid: u32, starttime: Option<u64>) -> String {
    match starttime {
        Some(st) => format!("{pid} {st}\n"),
        None => format!("{pid}\n"),
    }
}

/// Parse `(pid, starttime)` from `.tachorun/<name>.pid`.
pub fn parse_pid_record(raw: &str) -> Option<(u32, Option<u64>)> {
    let mut parts = raw.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    if pid == 0 {
        return None;
    }
    let starttime = parts.next().and_then(|s| s.parse::<u64>().ok());
    Some((pid, starttime))
}

/// Read `/proc/<pid>/stat` field 22 (`starttime` in clock ticks since boot).
pub fn proc_pid_starttime(pid: u32) -> Option<u64> {
    let stat_raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat_raw.rfind(')')?;
    let fields: Vec<&str> = stat_raw[rparen + 1..].split_whitespace().collect();
    fields.get(19)?.parse::<u64>().ok()
}

/// Read the process group ID (`pgrp`) of `pid` from `/proc/<pid>/stat` (or `getpgid`).
pub fn proc_pid_pgid(pid: u32) -> Option<u32> {
    if let Ok(stat_raw) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        if let Some(rparen) = stat_raw.rfind(')') {
            let fields: Vec<&str> = stat_raw[rparen + 1..].split_whitespace().collect();
            if let Some(pgrp) = fields.get(2).and_then(|s| s.parse::<u32>().ok()) {
                return Some(pgrp);
            }
        }
    }
    let pg = unsafe { libc::getpgid(pid as i32) };
    if pg > 0 {
        Some(pg as u32)
    } else {
        None
    }
}

/// Read `/proc/<pid>/cwd` symlink target as a string.
pub fn proc_pid_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

/// Check whether `pid` is alive, `/proc/<pid>/stat` is readable, and not a zombie (`state != 'Z'`).
/// Fails closed (`false`) if `/proc/<pid>/stat` cannot be read or parsed.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc != 0 {
        return false;
    }
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(rparen) = stat.rfind(')') else {
        return false;
    };
    !stat[rparen + 1..].trim_start().starts_with('Z')
}

/// Check whether `pid` is alive AND its `/proc/<pid>/stat` `starttime` is readable and matches `expected_starttime`.
pub fn pid_alive_with_starttime(pid: u32, expected_starttime: Option<u64>) -> bool {
    let Some(exp) = expected_starttime.filter(|&st| st > 0) else {
        return false;
    };
    if !pid_alive(pid) {
        return false;
    }
    let Some(actual_st) = proc_pid_starttime(pid) else {
        return false;
    };
    actual_st == exp
}

/// Normalize a path string by canonicalizing if it exists on disk, or stripping trailing slashes.
fn normalize_path_str(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    fs::canonicalize(p)
        .map(|c| c.to_string_lossy().to_string())
        .unwrap_or_else(|_| p.trim_end_matches('/').to_string())
}

/// Pure verification of a `tacho kill` target before sending any signal:
/// 1. `(pid, starttime)` must match `/proc/<pid>/stat` (refuses recycled PIDs).
/// 2. Recorded `.cwd` (if present) and live `/proc/<pid>/cwd` must lie inside `worktree_root` (`owns`).
/// 3. Process must lead its own process group (`pgid == pid`).
pub fn verify_kill_target(
    pid: u32,
    recorded_starttime: Option<u64>,
    actual_starttime: Option<u64>,
    recorded_cwd: Option<&str>,
    actual_proc_cwd: Option<&str>,
    actual_pgid: Option<u32>,
    worktree_root: &str,
) -> Result<(), String> {
    let Some(act_st) = actual_starttime else {
        return Err(format!(
            "refusing to signal pid {pid}: could not read /proc/{pid}/stat starttime"
        ));
    };
    let Some(rec_st) = recorded_starttime.filter(|&st| st > 0) else {
        return Err(format!(
            "refusing to signal pid {pid}: no starttime recorded in pid file"
        ));
    };
    if act_st != rec_st {
        return Err(format!(
            "refusing to signal pid {pid}: starttime mismatch (recorded {rec_st}, actual {act_st}; PID was reused)"
        ));
    }

    let norm_root = normalize_path_str(worktree_root);
    if let Some(rec_cwd) = recorded_cwd {
        let trimmed = rec_cwd.trim();
        if !trimmed.is_empty() {
            let norm_rec = normalize_path_str(trimmed);
            if !owns(&norm_rec, &norm_root) && !owns(trimmed, worktree_root) {
                return Err(format!(
                    "refusing to signal pid {pid}: recorded cwd '{trimmed}' is outside worktree '{worktree_root}'"
                ));
            }
        }
    }

    let Some(proc_cwd) = actual_proc_cwd else {
        return Err(format!(
            "refusing to signal pid {pid}: could not read /proc/{pid}/cwd"
        ));
    };
    let norm_proc_cwd = normalize_path_str(proc_cwd);
    if !owns(&norm_proc_cwd, &norm_root) && !owns(proc_cwd, worktree_root) {
        return Err(format!(
            "refusing to signal pid {pid}: /proc/{pid}/cwd ('{proc_cwd}') is outside worktree '{worktree_root}'"
        ));
    }

    if actual_pgid != Some(pid) {
        return Err(format!(
            "refusing to signal pid {pid}: process is not a process group leader (pgid={actual_pgid:?})"
        ));
    }

    Ok(())
}

/// Executes `tacho job <name> -- <cmd...>`.
pub fn execute_job(args: JobArgs) -> io::Result<()> {
    if let Err(msg) = validate_job_name(&args.name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
    }
    if args.cmd.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "job command cannot be empty",
        ));
    }

    let cwd = std::env::current_dir()?;
    let root = find_worktree_root(&cwd);
    let run_dir = root.join(".tachorun");
    fs::create_dir_all(&run_dir)?;

    let pid_file = run_dir.join(format!("{}.pid", args.name));
    let log_file_path = run_dir.join(format!("{}.log", args.name));
    let exit_file = run_dir.join(format!("{}.exit", args.name));
    let cmd_file = run_dir.join(format!("{}.cmd", args.name));
    let cwd_file = run_dir.join(format!("{}.cwd", args.name));

    if let Ok(raw_pid) = fs::read_to_string(&pid_file) {
        if let Some((old_pid, old_st)) = parse_pid_record(&raw_pid) {
            if pid_alive_with_starttime(old_pid, old_st) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "job '{}' is already running (pid {old_pid}); use 'tacho wait {}' or 'tacho kill {}'",
                        args.name, args.name, args.name
                    ),
                ));
            }
        }
    }

    let _ = fs::remove_file(&exit_file);
    let log_file = File::create(&log_file_path)?;
    let log_err = log_file.try_clone()?;

    let job_tmp_dir = run_dir.join(format!("tmp-{}", args.name));
    fs::create_dir_all(&job_tmp_dir)?;
    fs::set_permissions(&job_tmp_dir, fs::Permissions::from_mode(0o700))?;

    let env_vars = load_worktree_env(&root);

    // Wrap the command so `.tachorun/<name>.exit` is written atomically as soon as it terminates,
    // while `exec` or child wait preserves the process group leader PID.
    let inner_cmd = if args.cmd.len() == 1 {
        args.cmd[0].clone()
    } else {
        args.cmd
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let exit_path_escaped = exit_file.to_string_lossy().replace('\'', "'\\''");
    let wrapper_script = format!(
        "set +e; ({inner_cmd}); __rc=$?; printf '%d\\n' \"$__rc\" > '{exit_path_escaped}.tmp' && mv -f '{exit_path_escaped}.tmp' '{exit_path_escaped}'; exit \"$__rc\""
    );

    let mut command = Command::new("bash");
    command
        .args(["-c", &wrapper_script])
        .current_dir(&root)
        .env("TACHO_JOB_NAME", &args.name)
        .env("TMPDIR", &job_tmp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err))
        .process_group(0);
    for (k, v) in &env_vars {
        command.env(k, v);
    }

    let child = command.spawn()?;
    let pid = child.id();
    let starttime = proc_pid_starttime(pid);

    write_atomic(&pid_file, format_pid_record(pid, starttime).as_bytes())?;
    write_atomic(&cmd_file, format!("{}\n", args.cmd.join(" ")).as_bytes())?;
    write_atomic(
        &cwd_file,
        format!("{}\n", root.to_string_lossy()).as_bytes(),
    )?;

    let store = open_fleet_store()?;
    let because = format!(
        "started background job '{}' (pid {pid}, starttime={starttime:?}): {}",
        args.name,
        args.cmd.join(" ")
    );
    store.record_decision("watch", "job_start", &args.name, &because)?;
    emit_decision_event("system", "watch:job_start", &because);

    println!(
        "Started job '{}' (pid {pid}) -> {}",
        args.name,
        log_file_path.display()
    );
    Ok(())
}

/// Wait on a numeric PID using kernel `pidfd_open` + `poll` (event-driven),
/// returning `Ok(0)` when the process exits or `Ok(3)` if it is still running at `cap_secs`.
pub fn wait_for_numeric_pid(pid: u32, cap_secs: u64) -> io::Result<i32> {
    let initial_st = proc_pid_starttime(pid);
    if !pid_alive_with_starttime(pid, initial_st) {
        return Ok(0);
    }

    let mut watcher = FdEventWatcher::new()?;
    let _ = watcher.register_pid(pid);
    let start = Instant::now();
    let deadline = start + Duration::from_secs(cap_secs);
    let mut last_heartbeat = start;

    loop {
        if !pid_alive_with_starttime(pid, initial_st) {
            return Ok(0);
        }
        let now = Instant::now();
        if now >= deadline {
            println!("[tacho wait] pid {pid} still running after {cap_secs}s timeout (exit 3)");
            return Ok(3);
        }
        if now.duration_since(last_heartbeat).as_millis() >= HEARTBEAT_INTERVAL_MS {
            let elapsed = now.duration_since(start).as_secs();
            println!("[tacho wait] pid {pid} still running ({elapsed}s elapsed)...");
            last_heartbeat = now;
        }
        let rem_ms = deadline.saturating_duration_since(now).as_millis().min(250) as i32;
        watcher.wait_ms(rem_ms.max(1));
    }
}

/// Surface and delete any one-time burner notice in `.tachorun/NOTICE.txt` or `.tacho-run/NOTICE.txt`.
fn drain_burner_notice(root: &Path) {
    for dir_name in [".tachorun", ".tacho-run"] {
        let notice_path = root.join(dir_name).join("NOTICE.txt");
        if notice_path.is_file() {
            if let Ok(msg) = fs::read_to_string(&notice_path) {
                let _ = fs::remove_file(&notice_path);
                if !msg.trim().is_empty() {
                    eprintln!("[tacho notice] {}", msg.trim());
                }
            }
        }
    }
}

/// Executes `tacho wait <name|pid> [--timeout 540]`.
///
/// Accepts either a named job in `.tachorun/<name>.pid` OR a numeric PID.
/// Returns `0` (or the job's exit code) when finished, or `3` if still running at the cap.
pub fn execute_wait(args: WaitArgs) -> io::Result<i32> {
    let cwd = std::env::current_dir()?;
    let root = find_worktree_root(&cwd);
    drain_burner_notice(&root);

    let cap_secs = args.timeout.unwrap_or(MAX_WAIT_SECS).min(MAX_WAIT_SECS);
    let run_dir = root.join(".tachorun");
    let pid_file = run_dir.join(format!("{}.pid", args.name));
    let log_file_path = run_dir.join(format!("{}.log", args.name));
    let exit_file = run_dir.join(format!("{}.exit", args.name));

    // If no named job record exists and `args.name` is a numeric PID, wait directly on that PID!
    if !pid_file.exists() && !exit_file.exists() {
        if let Ok(numeric_pid) = args.name.trim().parse::<u32>() {
            if numeric_pid > 0 {
                return wait_for_numeric_pid(numeric_pid, cap_secs);
            }
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("job '{}' not found in {}", args.name, run_dir.display()),
        ));
    }

    let (pid, recorded_st) = fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| parse_pid_record(&s))
        .unwrap_or((0, None));

    let mut watcher = FdEventWatcher::new()?;
    watcher.watch_path(&run_dir);
    if log_file_path.exists() {
        watcher.watch_path(&log_file_path);
    }
    if pid > 0 {
        let _ = watcher.register_pid(pid);
    }

    let start = Instant::now();
    let deadline = start + Duration::from_secs(cap_secs);
    let mut last_heartbeat = start;
    let mut log_offset = 0u64;

    let stream_log = |offset: &mut u64| {
        if let Ok(mut f) = File::open(&log_file_path) {
            if f.seek(SeekFrom::Start(*offset)).is_ok() {
                let mut buf = Vec::new();
                if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    *offset += buf.len() as u64;
                    let _ = io::stdout().write_all(&buf);
                    let _ = io::stdout().flush();
                }
            }
        }
    };

    loop {
        drain_burner_notice(&root);
        stream_log(&mut log_offset);

        if let Ok(exit_str) = fs::read_to_string(&exit_file) {
            if let Ok(code) = exit_str.trim().parse::<i32>() {
                stream_log(&mut log_offset);
                return Ok(code);
            }
        }

        if pid == 0 || !pid_alive_with_starttime(pid, recorded_st) {
            stream_log(&mut log_offset);
            let code = fs::read_to_string(&exit_file)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .unwrap_or(0);
            write_atomic(&exit_file, format!("{code}\n").as_bytes())?;
            return Ok(code);
        }

        let now = Instant::now();
        if now >= deadline {
            stream_log(&mut log_offset);
            println!(
                "[tacho wait] job '{}' (pid {pid}) still running after {cap_secs}s timeout (exit 3)",
                args.name
            );
            return Ok(3);
        }

        if now.duration_since(last_heartbeat).as_millis() >= HEARTBEAT_INTERVAL_MS {
            let elapsed = now.duration_since(start).as_secs();
            println!(
                "[tacho wait] job '{}' (pid {pid}) still running ({elapsed}s elapsed)...",
                args.name
            );
            last_heartbeat = now;
        }

        let rem_ms = deadline.saturating_duration_since(now).as_millis().min(250) as i32;
        watcher.wait_ms(rem_ms.max(1));
    }
}

/// Executes `tacho kill <name> [--now]`.
///
/// Verifies `(pid, starttime)`, verifies `/proc/<pid>/cwd` (and `.cwd`) is inside the worktree,
/// and verifies `pgid == pid` before signalling the process group.
pub fn execute_kill(args: KillArgs) -> io::Result<()> {
    if let Err(msg) = validate_job_name(&args.name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
    }
    let cwd = std::env::current_dir()?;
    let root = find_worktree_root(&cwd);
    let run_dir = root.join(".tachorun");
    let pid_file = run_dir.join(format!("{}.pid", args.name));
    let exit_file = run_dir.join(format!("{}.exit", args.name));
    let cwd_file = run_dir.join(format!("{}.cwd", args.name));

    let raw_pid = fs::read_to_string(&pid_file).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("job '{}' not found in {}", args.name, run_dir.display()),
        )
    })?;
    let Some((pid, recorded_st)) = parse_pid_record(&raw_pid) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid pid record in {}", pid_file.display()),
        ));
    };

    if !pid_alive(pid) {
        println!("Job '{}' (pid {pid}) is already stopped.", args.name);
        return Ok(());
    }

    let actual_st = proc_pid_starttime(pid);
    let recorded_cwd = fs::read_to_string(&cwd_file).ok();
    let actual_proc_cwd = proc_pid_cwd(pid);
    let actual_pgid = proc_pid_pgid(pid);
    let root_str = root.to_string_lossy().to_string();

    if let Err(refusal) = verify_kill_target(
        pid,
        recorded_st,
        actual_st,
        recorded_cwd.as_deref(),
        actual_proc_cwd.as_deref(),
        actual_pgid,
        &root_str,
    ) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, refusal));
    }

    let sig = if args.now {
        libc::SIGKILL
    } else {
        libc::SIGTERM
    };
    unsafe {
        let _ = libc::kill(-(pid as i32), sig);
        let _ = libc::kill(pid as i32, sig);
    }

    let mut escalated = args.now;
    if !args.now {
        let mut watcher = FdEventWatcher::new()?;
        let _ = watcher.register_pid(pid);
        let grace_deadline = Instant::now() + Duration::from_secs(5);
        while pid_alive_with_starttime(pid, actual_st) && Instant::now() < grace_deadline {
            let rem_ms = grace_deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(200) as i32;
            watcher.wait_ms(rem_ms.max(1));
        }
        if pid_alive_with_starttime(pid, actual_st) {
            escalated = true;
            unsafe {
                let _ = libc::kill(-(pid as i32), libc::SIGKILL);
                let _ = libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }

    let exit_code = if escalated { 137 } else { 143 };
    write_atomic(&exit_file, format!("{exit_code}\n").as_bytes())?;

    let store = open_fleet_store()?;
    let because = format!("killed job '{}' (pid {pid}, now={})", args.name, args.now);
    store.record_decision("watch", "job_kill", &args.name, &because)?;
    emit_decision_event("system", "watch:job_kill", &because);

    println!("Stopped job '{}' (pid {pid}).", args.name);
    Ok(())
}

/// Executes `tacho note [<key>] "<phase>: <line>"`.
pub fn execute_note(args: NoteArgs) -> io::Result<()> {
    let now = now_secs();
    let note =
        parse_note(&args.note, now).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut store = open_fleet_store()?;
    let cwd = std::env::current_dir()?;
    let root = find_worktree_root(&cwd);
    let env_map = load_worktree_env(&root);

    let agent_key = if !args.key.trim().is_empty() {
        args.key.trim().to_string()
    } else {
        std::env::var("TACHO_AGENT")
            .ok()
            .or_else(|| env_map.get("TACHO_SLUG").cloned())
            .or_else(|| env_map.get("SLUG").cloned())
            .or_else(|| root.file_name().and_then(|s| s.to_str()).map(String::from))
            .unwrap_or_else(|| "local".to_string())
    };

    let because = format!("{}: {}", note.phase, note.text);
    let because_for_store = because.clone();
    let note_clone = note.clone();
    let key_clone = agent_key.clone();
    store.mutate(Some(("watch", "note", &agent_key, &because)), move |st| {
        let existing = st.notes.get(&key_clone).map(Vec::as_slice).unwrap_or(&[]);
        let updated = with_note(existing, note_clone.clone());
        st.notes.insert(key_clone.clone(), updated);
        if let Some(a) = st.agents.get_mut(&key_clone) {
            a.status_note = because_for_store.clone();
        }
    })?;
    emit_decision_event(&format!("agent:{agent_key}"), "watch:note", &because);
    println!(
        "Recorded note for {agent_key}: [{}] {}",
        note.phase, note.text
    );
    Ok(())
}
