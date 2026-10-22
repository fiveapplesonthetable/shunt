//! Periodic host machine and process resource sampler.
//!
//! Ultra-efficient, zero-allocation polling of `/proc/loadavg`, `/proc/stat`, `/proc/meminfo`,
//! `statvfs`, and active run process trees as specified in design section 5.3 and 8.
//!
//! Persistent file handles are reused across ticks with `pread` into stack buffers to avoid
//! file open/close syscalls and heap reallocations.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tacho_core::Input;

use crate::appender::AppenderHandle;
use crate::config::SamplerConfig;

static CLK_TCK: OnceLock<u64> = OnceLock::new();
static PAGE_SIZE: OnceLock<u64> = OnceLock::new();

/// Returns the clock ticks per second (cached sysconf(_SC_CLK_TCK)).
#[inline]
pub fn get_clock_ticks_per_sec() -> u64 {
    *CLK_TCK.get_or_init(|| {
        let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if tck <= 0 {
            100
        } else {
            tck as u64
        }
    })
}

/// Returns the page size in bytes (cached sysconf(_SC_PAGESIZE)).
#[inline]
pub fn get_page_size() -> u64 {
    *PAGE_SIZE.get_or_init(|| {
        let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if ps <= 0 {
            4096
        } else {
            ps as u64
        }
    })
}

/// Persistent `/proc` sampler maintaining open file handles and scratch buffers
/// to eliminate open/close syscalls and heap allocations during polling ticks.
pub struct ProcSampler {
    loadavg_file: Option<File>,
    stat_file: Option<File>,
    meminfo_file: Option<File>,
    mount_cstrings: Vec<(String, CString)>,
    last_samples: HashMap<String, i64>,
    prev_cpu: Option<(u64, u64)>, // (busy, total)
    clk_tck: u64,
    page_size: u64,
    tree_pids_scratch: Vec<u32>,
    queue_scratch: Vec<u32>,
}

impl ProcSampler {
    /// Creates a new `ProcSampler`, pre-opening persistent handles for `/proc` virtual files.
    pub fn new(mounts: &[String]) -> Self {
        let mount_cstrings: Vec<(String, CString)> = mounts
            .iter()
            .filter_map(|m| CString::new(m.as_str()).ok().map(|c| (m.clone(), c)))
            .collect();

        Self {
            loadavg_file: File::open("/proc/loadavg").ok(),
            stat_file: File::open("/proc/stat").ok(),
            meminfo_file: File::open("/proc/meminfo").ok(),
            mount_cstrings,
            last_samples: HashMap::new(),
            prev_cpu: None,
            clk_tck: get_clock_ticks_per_sec(),
            page_size: get_page_size(),
            tree_pids_scratch: Vec::with_capacity(32),
            queue_scratch: Vec::with_capacity(32),
        }
    }

    /// Reads `/proc/loadavg` using `pread` at offset 0 into a stack buffer.
    #[inline]
    pub fn sample_loadavg(&mut self) -> Option<i64> {
        let mut buf = [0u8; 128];
        let file = self.loadavg_file.as_mut()?;
        let n = file.read_at(&mut buf, 0).ok()?;
        parse_loadavg_milli(&buf[..n])
    }

    /// Reads `/proc/stat` using `pread` at offset 0 into a stack buffer.
    #[inline]
    pub fn sample_cpu_jiffies(&mut self) -> Option<(u64, u64)> {
        let mut buf = [0u8; 2048];
        let file = self.stat_file.as_mut()?;
        let n = file.read_at(&mut buf, 0).ok()?;
        parse_cpu_jiffies(&buf[..n])
    }

    /// Reads `/proc/meminfo` using `pread` at offset 0 into a stack buffer.
    #[inline]
    pub fn sample_mem_used_bytes(&mut self) -> Option<u64> {
        let mut buf = [0u8; 2048];
        let file = self.meminfo_file.as_mut()?;
        let n = file.read_at(&mut buf, 0).ok()?;
        parse_mem_used_bytes(&buf[..n])
    }

    /// Samples the CPU time (in microseconds) and RSS (in bytes) of a process tree rooted at `root_pid`.
    pub fn sample_tree(&mut self, root_pid: u32) -> Option<(u64, u64)> {
        self.tree_pids_scratch.clear();
        self.queue_scratch.clear();

        // Check if root process exists
        if !std::path::Path::new(&format!("/proc/{}", root_pid)).exists() {
            return None;
        }

        self.tree_pids_scratch.push(root_pid);
        self.queue_scratch.push(root_pid);

        // Try fast tree walk via /proc/<pid>/task/<pid>/children (O(tree size), no /proc scan)
        let mut children_buf = [0u8; 512];
        let mut fast_success = true;

        while let Some(pid) = self.queue_scratch.pop() {
            let path = format!("/proc/{}/task/{}/children", pid, pid);
            if let Ok(file) = File::open(&path) {
                if let Ok(n) = file.read_at(&mut children_buf, 0) {
                    let slice = &children_buf[..n];
                    for token in slice
                        .split(|&b| b == b' ' || b == b'\t' || b == b'\n')
                        .filter(|s| !s.is_empty())
                    {
                        if let Some(child_pid) = parse_u64_ascii(token) {
                            let cpid = child_pid as u32;
                            if !self.tree_pids_scratch.contains(&cpid) {
                                self.tree_pids_scratch.push(cpid);
                                self.queue_scratch.push(cpid);
                            }
                        }
                    }
                }
            } else {
                fast_success = false;
                break;
            }
        }

        // Fall back to scanning /proc if /proc/<pid>/task/<pid>/children is unavailable
        if !fast_success {
            let full_tree = find_process_tree(root_pid);
            self.tree_pids_scratch.clear();
            self.tree_pids_scratch.extend(full_tree);
        }

        let mut total_cpu_ticks = 0u64;
        let mut total_rss_bytes = 0u64;

        for &pid in &self.tree_pids_scratch {
            if let Some((ticks, rss, _ppid)) = read_process_stat_fields(pid, self.page_size) {
                total_cpu_ticks = total_cpu_ticks.saturating_add(ticks);
                total_rss_bytes = total_rss_bytes.saturating_add(rss);
            }
        }

        let cpu_us = (total_cpu_ticks * 1_000_000) / self.clk_tck;
        Some((cpu_us, total_rss_bytes))
    }

    /// Performs one complete sampling tick, emitting counter samples only for values that changed.
    pub fn sample_tick(
        &mut self,
        appender: &AppenderHandle,
        now_ns: u64,
        run_track_buf: &mut String,
    ) {
        // 1. /proc/loadavg -> machine/load1
        if let Some(load1_milli) = self.sample_loadavg() {
            send_sample_if_changed(
                appender,
                &mut self.last_samples,
                "machine/load1",
                load1_milli,
                Some("load x 1000"),
                now_ns,
            );
        }

        // 2. /proc/stat -> machine/cpu_busy
        if let Some((busy, total)) = self.sample_cpu_jiffies() {
            if let Some((prev_busy, prev_total)) = self.prev_cpu {
                let busy_delta = busy.saturating_sub(prev_busy);
                let total_delta = total.saturating_sub(prev_total);
                if let Some(per_mille) = (busy_delta * 1000).checked_div(total_delta) {
                    send_sample_if_changed(
                        appender,
                        &mut self.last_samples,
                        "machine/cpu_busy",
                        per_mille.min(1000) as i64,
                        Some("per mille"),
                        now_ns,
                    );
                }
            }
            self.prev_cpu = Some((busy, total));
        }

        // 3. /proc/meminfo -> machine/mem_used
        if let Some(mem_used_bytes) = self.sample_mem_used_bytes() {
            send_sample_if_changed(
                appender,
                &mut self.last_samples,
                "machine/mem_used",
                mem_used_bytes as i64,
                Some("bytes"),
                now_ns,
            );
        }

        // 4. statvfs for configured mount points
        for i in 0..self.mount_cstrings.len() {
            let (ref mount_str, ref c_path) = self.mount_cstrings[i];
            if let Some(bytes_used) = sample_mount_statvfs(c_path) {
                run_track_buf.clear();
                run_track_buf.push_str("machine/disk:");
                run_track_buf.push_str(mount_str);
                send_sample_if_changed(
                    appender,
                    &mut self.last_samples,
                    run_track_buf,
                    bytes_used as i64,
                    Some("bytes used"),
                    now_ns,
                );
            }
        }

        // 5. Per-run process tree sampling
        let active_runs = appender.active_runs();
        for run in active_runs.iter() {
            if let Some((cpu_us, rss_bytes)) = self.sample_tree(run.pid) {
                run_track_buf.clear();
                run_track_buf.push_str(&run.run_track);
                run_track_buf.push_str("/cpu");
                send_sample_if_changed(
                    appender,
                    &mut self.last_samples,
                    run_track_buf,
                    cpu_us as i64,
                    Some("us"),
                    now_ns,
                );

                run_track_buf.truncate(run.run_track.len());
                run_track_buf.push_str("/rss");
                send_sample_if_changed(
                    appender,
                    &mut self.last_samples,
                    run_track_buf,
                    rss_bytes as i64,
                    Some("bytes"),
                    now_ns,
                );
            }
        }
    }
}

/// Spawns the background sampler thread.
pub fn spawn_sampler(
    config: SamplerConfig,
    appender: AppenderHandle,
    stop_flag: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tacho-sampler".to_string())
        .spawn(move || {
            run_sampler_loop(config, appender, stop_flag);
        })
        .expect("failed to spawn sampler thread")
}

fn run_sampler_loop(config: SamplerConfig, appender: AppenderHandle, stop_flag: Arc<AtomicBool>) {
    let interval = Duration::from_millis(config.interval_ms.max(100));
    let mut proc_sampler = ProcSampler::new(&config.mounts);
    let mut run_track_buf = String::with_capacity(64);

    while !stop_flag.load(Ordering::Acquire) {
        let loop_start = Instant::now();
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        proc_sampler.sample_tick(&appender, now_ns, &mut run_track_buf);

        let elapsed = loop_start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

/// Sends counter samples only when the value has changed since the previous sample.
///
/// Lookups are performed against `&str` BEFORE any string allocations occur.
#[inline]
pub fn send_sample_if_changed(
    appender: &AppenderHandle,
    last_samples: &mut HashMap<String, i64>,
    track_key: &str,
    value: i64,
    unit: Option<&str>,
    now_ns: u64,
) {
    if let Some(&last) = last_samples.get(track_key) {
        if last == value {
            return;
        }
    }
    last_samples.insert(track_key.to_string(), value);
    let _ = appender.send_input(
        Input::CounterSample {
            source_ts_ns: Some(now_ns),
            track_key: track_key.to_string(),
            value,
            unit: unit.map(|s| s.to_string()),
        },
        now_ns,
    );
}

/// Parses unsigned 64-bit integer from ASCII byte slice without allocation.
#[inline]
pub fn parse_u64_ascii(bytes: &[u8]) -> Option<u64> {
    let mut val = 0u64;
    let mut empty = true;
    for &b in bytes {
        if b.is_ascii_digit() {
            val = val.checked_mul(10)?.checked_add((b - b'0') as u64)?;
            empty = false;
        } else {
            break;
        }
    }
    if empty {
        None
    } else {
        Some(val)
    }
}

/// Parses signed 64-bit integer from ASCII byte slice without allocation.
#[inline]
pub fn parse_i64_ascii(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let (is_neg, digits) = if bytes[0] == b'-' {
        (true, &bytes[1..])
    } else {
        (false, bytes)
    };
    let val = parse_u64_ascii(digits)? as i64;
    Some(if is_neg { -val } else { val })
}

/// Parses 1-minute load average scaled by 1000 from `/proc/loadavg` bytes without allocation.
pub fn parse_loadavg_milli(buf: &[u8]) -> Option<i64> {
    let mut iter = buf
        .split(|&b| b == b' ' || b == b'\t')
        .filter(|s| !s.is_empty());
    let first = iter.next()?;
    let dot_pos = first.iter().position(|&b| b == b'.');
    let (int_part, frac_part) = match dot_pos {
        Some(pos) => (&first[..pos], &first[pos + 1..]),
        None => (first, &b""[..]),
    };
    let whole = parse_i64_ascii(int_part)?;
    let mut milli = whole * 1000;
    if !frac_part.is_empty() {
        let mut frac_val = 0i64;
        let mut divisor = 1i64;
        for &b in frac_part.iter().take(3) {
            if b.is_ascii_digit() {
                frac_val = frac_val * 10 + (b - b'0') as i64;
                divisor *= 10;
            }
        }
        while divisor < 1000 {
            frac_val *= 10;
            divisor *= 10;
        }
        milli += frac_val;
    }
    Some(milli)
}

/// Parses CPU busy and total jiffies from `/proc/stat` bytes without allocation.
pub fn parse_cpu_jiffies(buf: &[u8]) -> Option<(u64, u64)> {
    for line in buf.split(|&b| b == b'\n') {
        if line.starts_with(b"cpu ") {
            let mut fields = line[4..]
                .split(|&b| b == b' ' || b == b'\t')
                .filter(|s| !s.is_empty());
            let user = parse_u64_ascii(fields.next()?)?;
            let nice = parse_u64_ascii(fields.next()?)?;
            let system = parse_u64_ascii(fields.next()?)?;
            let idle = parse_u64_ascii(fields.next()?)?;
            let iowait = fields.next().and_then(parse_u64_ascii).unwrap_or(0);
            let irq = fields.next().and_then(parse_u64_ascii).unwrap_or(0);
            let softirq = fields.next().and_then(parse_u64_ascii).unwrap_or(0);
            let steal = fields.next().and_then(parse_u64_ascii).unwrap_or(0);

            let busy = user + nice + system + irq + softirq + steal;
            let total = busy + idle + iowait;
            return Some((busy, total));
        }
    }
    None
}

/// Parses used memory in bytes from `/proc/meminfo` bytes without allocation.
pub fn parse_mem_used_bytes(buf: &[u8]) -> Option<u64> {
    let mut total_kb = None;
    let mut avail_kb = None;

    for line in buf.split(|&b| b == b'\n') {
        if line.starts_with(b"MemTotal:") {
            let rest = &line[9..];
            let mut tokens = rest
                .split(|&b| b == b' ' || b == b'\t')
                .filter(|s| !s.is_empty());
            total_kb = tokens.next().and_then(parse_u64_ascii);
        } else if line.starts_with(b"MemAvailable:") {
            let rest = &line[13..];
            let mut tokens = rest
                .split(|&b| b == b' ' || b == b'\t')
                .filter(|s| !s.is_empty());
            avail_kb = tokens.next().and_then(parse_u64_ascii);
        }
        if total_kb.is_some() && avail_kb.is_some() {
            break;
        }
    }

    match (total_kb, avail_kb) {
        (Some(tot), Some(avail)) => Some(tot.saturating_sub(avail) * 1024),
        _ => None,
    }
}

/// Samples mount usage via statvfs without string allocation.
#[inline]
pub fn sample_mount_statvfs(c_path: &CString) -> Option<u64> {
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let res = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if res == 0 {
        let total = (stat.f_blocks as u64) * (stat.f_frsize as u64);
        let free = (stat.f_bfree as u64) * (stat.f_frsize as u64);
        Some(total.saturating_sub(free))
    } else {
        None
    }
}

/// Reads utime + stime (in clock ticks), RSS (in bytes), and PPID for a process from `/proc/<pid>/stat`.
///
/// Uses a single `pread` into a stack buffer and parses tokens after the last `')'`.
/// Drops any need to open `/proc/<pid>/status`.
pub fn read_process_stat_fields(pid: u32, page_size: u64) -> Option<(u64, u64, u32)> {
    let path = format!("/proc/{}/stat", pid);
    let file = File::open(&path).ok()?;
    let mut buf = [0u8; 1024];
    let n = file.read_at(&mut buf, 0).ok()?;
    let slice = &buf[..n];

    let rparen = slice.iter().rposition(|&b| b == b')')?;
    let rest = &slice[rparen + 1..];

    let mut tokens = rest
        .split(|&b| b == b' ' || b == b'\t')
        .filter(|s| !s.is_empty());

    let _state = tokens.next()?; // Token 0
    let ppid_slice = tokens.next()?; // Token 1: field 4 (ppid)
    let ppid = parse_u64_ascii(ppid_slice)? as u32;

    // Tokens 2..10
    for _ in 0..9 {
        tokens.next()?;
    }

    let utime_slice = tokens.next()?; // Token 11: field 14 (utime)
    let stime_slice = tokens.next()?; // Token 12: field 15 (stime)

    let utime = parse_u64_ascii(utime_slice)?;
    let stime = parse_u64_ascii(stime_slice)?;
    let cpu_ticks = utime.saturating_add(stime);

    // Tokens 13..20
    for _ in 0..8 {
        tokens.next()?;
    }

    let rss_slice = tokens.next()?; // Token 21: field 24 (rss in pages)
    let rss_pages = parse_u64_ascii(rss_slice)?;
    let rss_bytes = rss_pages.saturating_mul(page_size);

    Some((cpu_ticks, rss_bytes, ppid))
}

// ---------------------------------------------------------------------------
// Backward-compatible public functions for existing callers and unit tests
// ---------------------------------------------------------------------------

/// Reads `/proc/loadavg` and returns 1-minute load scaled by 1000.
pub fn read_loadavg_milli() -> Option<i64> {
    let mut buf = [0u8; 128];
    let file = File::open("/proc/loadavg").ok()?;
    let n = file.read_at(&mut buf, 0).ok()?;
    parse_loadavg_milli(&buf[..n])
}

/// Reads `/proc/stat` and returns (busy_jiffies, total_jiffies).
pub fn read_cpu_jiffies() -> Option<(u64, u64)> {
    let mut buf = [0u8; 2048];
    let file = File::open("/proc/stat").ok()?;
    let n = file.read_at(&mut buf, 0).ok()?;
    parse_cpu_jiffies(&buf[..n])
}

/// Reads `/proc/meminfo` and returns used memory in bytes (MemTotal - MemAvailable).
pub fn read_mem_used_bytes() -> Option<u64> {
    let mut buf = [0u8; 2048];
    let file = File::open("/proc/meminfo").ok()?;
    let n = file.read_at(&mut buf, 0).ok()?;
    parse_mem_used_bytes(&buf[..n])
}

/// Reads filesystem usage via statvfs.
pub fn read_mount_used_bytes(mount: &str) -> Option<u64> {
    let c_path = CString::new(mount).ok()?;
    sample_mount_statvfs(&c_path)
}

/// Samples the CPU time (in microseconds) and RSS (in bytes) of a process tree rooted at `root_pid`.
pub fn sample_pid_tree(root_pid: u32) -> Option<(u64, u64)> {
    let mut sampler = ProcSampler::new(&[]);
    sampler.sample_tree(root_pid)
}

/// Discovers all descendant PIDs rooted at `root_pid`.
pub fn find_process_tree(root_pid: u32) -> Vec<u32> {
    use std::collections::{HashMap, HashSet, VecDeque};

    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();
    let page_size = get_page_size();

    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if let Ok(pid) = name.parse::<u32>() {
                if let Some((_, _, ppid)) = read_process_stat_fields(pid, page_size) {
                    children_map.entry(ppid).or_default().push(pid);
                }
            }
        }
    }

    let mut tree = Vec::new();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();

    queue.push_back(root_pid);
    visited.insert(root_pid);

    while let Some(pid) = queue.pop_front() {
        tree.push(pid);
        if let Some(children) = children_map.get(&pid) {
            for &child in children {
                if visited.insert(child) {
                    queue.push_back(child);
                }
            }
        }
    }

    tree
}

/// Reads the PPID of a process from `/proc/<pid>/stat`.
pub fn read_process_ppid(pid: u32) -> Option<u32> {
    let (_, _, ppid) = read_process_stat_fields(pid, get_page_size())?;
    Some(ppid)
}

/// Reads utime + stime (in clock ticks) for a process from `/proc/<pid>/stat`.
pub fn read_process_cpu_ticks(pid: u32) -> Option<u64> {
    let (cpu_ticks, _, _) = read_process_stat_fields(pid, get_page_size())?;
    Some(cpu_ticks)
}

/// Reads VmRSS (in bytes) for a process from `/proc/<pid>/stat` (using page size conversion).
pub fn read_process_rss_bytes(pid: u32) -> Option<u64> {
    let (_, rss_bytes, _) = read_process_stat_fields(pid, get_page_size())?;
    Some(rss_bytes)
}

#[cfg(test)]
mod tests {
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
        let start = Instant::now();

        for i in 0..iterations {
            let now_ns = 1_000_000_000 + i * 2_000_000_000;
            sampler.sample_tick(&appender, now_ns, &mut run_track_buf);
        }

        let elapsed = start.elapsed();
        let us_per_tick = (elapsed.as_nanos() as f64) / (iterations as f64) / 1000.0;

        println!("\n============================================================");
        println!("ProcSampler Efficiency Benchmark ({} ticks):", iterations);
        println!("Elapsed time: {:.3?}", elapsed);
        println!("Average latency: {:.2} µs / tick", us_per_tick);
        println!("============================================================\n");

        assert!(
            us_per_tick < 1000.0,
            "Sampling tick must be sub-millisecond, got {:.2} µs",
            us_per_tick
        );
    }
}
