//! Periodic host machine and process resource sampler.
//!
//! Ultra-efficient, zero-allocation polling of `/proc/loadavg`, `/proc/stat`, `/proc/meminfo`,
//! `statvfs`, and active run process trees as specified in design section 5.3 and 8.
//!
//! Persistent file handles are reused across ticks with `pread` into stack buffers to avoid
//! file open/close syscalls and heap reallocations.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CString;
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tacho_core::{CustomOp, Input, SessionKey};

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

/// Parsed host memory statistics from `/proc/meminfo` (all in bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfoStats {
    pub used_bytes: u64,
    pub avail_bytes: u64,
    pub cached_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TrackedProc {
    pid: u32,
    ppid: u32,
    name: String,
    cmdline: String,
    last_cpu_us: u64,
    last_rss_bytes: u64,
    peak_rss_bytes: u64,
}

#[derive(Debug, Clone)]
struct WorktreeSnapshot {
    head_sha: Option<String>,
    reftable_marker: Option<String>,
    latest_mtime_ns: u64,
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
    reaped_sessions: HashSet<SessionKey>,
    proc_stacks: HashMap<SessionKey, Vec<(String, TrackedProc)>>,
    worktree_states: HashMap<String, WorktreeSnapshot>,
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
            reaped_sessions: HashSet::new(),
            proc_stacks: HashMap::new(),
            worktree_states: HashMap::new(),
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
        self.sample_meminfo().map(|m| m.used_bytes)
    }

    /// Reads `/proc/meminfo` using `pread` at offset 0 and returns detailed memory counters.
    #[inline]
    pub fn sample_meminfo(&mut self) -> Option<MemInfoStats> {
        let mut buf = [0u8; 2048];
        let file = self.meminfo_file.as_mut()?;
        let n = file.read_at(&mut buf, 0).ok()?;
        parse_meminfo(&buf[..n])
    }

    /// Discovers all PIDs in the process tree rooted at `root_pid` in parent-first BFS order.
    fn populate_tree_pids(&mut self, root_pid: u32) -> bool {
        self.tree_pids_scratch.clear();
        self.queue_scratch.clear();

        if !std::path::Path::new(&format!("/proc/{}", root_pid)).exists() {
            return false;
        }

        self.tree_pids_scratch.push(root_pid);
        let mut children_buf = [0u8; 512];
        let mut fast_success = true;
        let mut idx = 0;

        while idx < self.tree_pids_scratch.len() {
            let pid = self.tree_pids_scratch[idx];
            idx += 1;
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
                            }
                        }
                    }
                }
            } else {
                fast_success = false;
                break;
            }
        }

        if !fast_success {
            let full_tree = find_process_tree(root_pid);
            self.tree_pids_scratch.clear();
            self.tree_pids_scratch.extend(full_tree);
        }
        true
    }

    /// Samples the CPU time (in microseconds) and RSS (in bytes) of a process tree rooted at `root_pid`.
    pub fn sample_tree(&mut self, root_pid: u32) -> Option<(u64, u64)> {
        if !self.populate_tree_pids(root_pid) {
            return None;
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

    /// Samples the process tree rooted at `root_pid`, returning `(total_cpu_us, total_rss_bytes, live_procs)`.
    #[allow(clippy::type_complexity)]
    fn sample_tree_detailed(
        &mut self,
        root_pid: u32,
    ) -> Option<(u64, u64, Vec<(u32, u32, u64, u64, String)>)> {
        if !self.populate_tree_pids(root_pid) {
            return None;
        }

        let mut total_cpu_ticks = 0u64;
        let mut total_rss_bytes = 0u64;
        let mut live_procs = Vec::with_capacity(self.tree_pids_scratch.len());

        for &pid in &self.tree_pids_scratch {
            if let Some((ticks, rss, ppid, comm)) = read_process_stat_and_comm(pid, self.page_size)
            {
                total_cpu_ticks = total_cpu_ticks.saturating_add(ticks);
                total_rss_bytes = total_rss_bytes.saturating_add(rss);
                let proc_cpu_us = (ticks * 1_000_000) / self.clk_tck;
                live_procs.push((pid, ppid, proc_cpu_us, rss, comm));
            }
        }

        let cpu_us = (total_cpu_ticks * 1_000_000) / self.clk_tck;
        Some((cpu_us, total_rss_bytes, live_procs))
    }

    fn close_all_proc_slices(
        stack: &mut Vec<(String, TrackedProc)>,
        appender: &AppenderHandle,
        now_ns: u64,
    ) {
        while let Some((track_key, tp)) = stack.pop() {
            let mut args = BTreeMap::new();
            args.insert("pid".to_string(), tp.pid.to_string());
            args.insert("cpu_us".to_string(), tp.last_cpu_us.to_string());
            args.insert("rss_bytes".to_string(), tp.last_rss_bytes.to_string());
            args.insert("peak_rss_bytes".to_string(), tp.peak_rss_bytes.to_string());
            let _ = appender.send_input(
                Input::Custom {
                    source_ts_ns: Some(now_ns),
                    track: track_key,
                    op: CustomOp::End,
                    name: None,
                    id: Some(tp.pid.to_string()),
                    args,
                    value: None,
                    flow_out: None,
                    flow_in: None,
                },
                now_ns,
            );
        }
    }

    fn update_proc_slices(
        &mut self,
        session: &SessionKey,
        root_pid: u32,
        run_track: &str,
        live_procs: &[(u32, u32, u64, u64, String)],
        appender: &AppenderHandle,
        now_ns: u64,
    ) {
        let procs_track = format!("{}/procs", run_track);
        let stack = self.proc_stacks.entry(session.clone()).or_default();

        // Update stats on existing stack entries and find lowest index that exited or exec'd
        let mut first_invalid_idx = None;
        for (idx, (_, tp)) in stack.iter_mut().enumerate() {
            if let Some((_, _, cpu_us, rss, comm)) =
                live_procs.iter().find(|(p, _, _, _, _)| *p == tp.pid)
            {
                tp.last_cpu_us = *cpu_us;
                tp.last_rss_bytes = *rss;
                tp.peak_rss_bytes = tp.peak_rss_bytes.max(*rss);
                if tp.name != *comm && first_invalid_idx.is_none() {
                    first_invalid_idx = Some(idx);
                }
            } else if first_invalid_idx.is_none() {
                first_invalid_idx = Some(idx);
            }
        }

        // Pop in strict LIFO order down to `first_invalid_idx`
        if let Some(target_idx) = first_invalid_idx {
            while stack.len() > target_idx {
                if let Some((track_key, tp)) = stack.pop() {
                    let mut args = BTreeMap::new();
                    args.insert("pid".to_string(), tp.pid.to_string());
                    args.insert("cpu_us".to_string(), tp.last_cpu_us.to_string());
                    args.insert("rss_bytes".to_string(), tp.last_rss_bytes.to_string());
                    args.insert("peak_rss_bytes".to_string(), tp.peak_rss_bytes.to_string());
                    let _ = appender.send_input(
                        Input::Custom {
                            source_ts_ns: Some(now_ns),
                            track: track_key,
                            op: CustomOp::End,
                            name: None,
                            id: Some(tp.pid.to_string()),
                            args,
                            value: None,
                            flow_out: None,
                            flow_in: None,
                        },
                        now_ns,
                    );
                }
            }
        }

        // Open slices for any live process not currently on the stack (in parent-before-child BFS order)
        for (pid, ppid, cpu_us, rss, comm) in live_procs {
            if stack.iter().any(|(_, tp)| tp.pid == *pid) {
                continue;
            }
            let cmdline = read_process_cmdline(*pid, comm);
            let slice_name = if *pid == root_pid {
                format!("{} [pid {}]", comm, pid)
            } else {
                let short_cmd: String = cmdline.chars().take(48).collect();
                if short_cmd.is_empty() || short_cmd == *comm {
                    format!("{} [pid {}]", comm, pid)
                } else {
                    format!("{} [pid {}]: {}", comm, pid, short_cmd)
                }
            };

            let mut args = BTreeMap::new();
            args.insert("pid".to_string(), pid.to_string());
            args.insert("ppid".to_string(), ppid.to_string());
            args.insert("comm".to_string(), comm.clone());
            args.insert("cmdline".to_string(), cmdline.clone());
            args.insert("rss_bytes".to_string(), rss.to_string());

            let _ = appender.send_input(
                Input::Custom {
                    source_ts_ns: Some(now_ns),
                    track: procs_track.clone(),
                    op: CustomOp::Begin,
                    name: Some(slice_name),
                    id: Some(pid.to_string()),
                    args,
                    value: None,
                    flow_out: None,
                    flow_in: None,
                },
                now_ns,
            );

            stack.push((
                procs_track.clone(),
                TrackedProc {
                    pid: *pid,
                    ppid: *ppid,
                    name: comm.clone(),
                    cmdline,
                    last_cpu_us: *cpu_us,
                    last_rss_bytes: *rss,
                    peak_rss_bytes: *rss,
                },
            ));
        }
    }

    fn poll_worktree_progress(
        &mut self,
        agent_id: &str,
        cwd: &str,
        appender: &AppenderHandle,
        now_ns: u64,
    ) {
        let cwd_path = Path::new(cwd);
        if !cwd_path.is_dir() {
            return;
        }
        let git_dir = resolve_git_dir(cwd_path);
        let prev_reftable = self
            .worktree_states
            .get(cwd)
            .and_then(|s| s.reftable_marker.as_deref());
        let prev_sha = self
            .worktree_states
            .get(cwd)
            .and_then(|s| s.head_sha.as_deref());
        let (head_sha, reftable_marker) = git_dir
            .as_deref()
            .map(|gd| read_git_head_sha(cwd_path, gd, prev_reftable, prev_sha))
            .unwrap_or((None, None));
        let (latest_mtime_ns, newest_rel_path) = scan_worktree_latest_mtime_ns(cwd_path);

        let Some(prev) = self.worktree_states.get_mut(cwd) else {
            self.worktree_states.insert(
                cwd.to_string(),
                WorktreeSnapshot {
                    head_sha,
                    reftable_marker,
                    latest_mtime_ns,
                },
            );
            return;
        };
        prev.reftable_marker = reftable_marker;

        let mut committed = false;
        if head_sha.is_some() && head_sha != prev.head_sha {
            prev.head_sha = head_sha.clone();
            prev.latest_mtime_ns = prev.latest_mtime_ns.max(latest_mtime_ns);
            committed = true;
            let mut args = BTreeMap::new();
            if let Some(ref sha) = head_sha {
                args.insert("sha".to_string(), sha.clone());
            }
            args.insert("cwd".to_string(), cwd.to_string());
            let _ = appender.send_input(
                Input::Custom {
                    source_ts_ns: Some(now_ns),
                    track: format!("agent:{}", agent_id),
                    op: CustomOp::Instant,
                    name: Some("WorktreeCommit".to_string()),
                    id: head_sha,
                    args,
                    value: None,
                    flow_out: None,
                    flow_in: None,
                },
                now_ns,
            );
        }

        if !committed && latest_mtime_ns > prev.latest_mtime_ns && newest_rel_path.is_some() {
            prev.latest_mtime_ns = latest_mtime_ns;
            let mut args = BTreeMap::new();
            args.insert("cwd".to_string(), cwd.to_string());
            if let Some(rel) = newest_rel_path {
                args.insert("detail".to_string(), rel);
            }
            let _ = appender.send_input(
                Input::Custom {
                    source_ts_ns: Some(now_ns),
                    track: format!("agent:{}", agent_id),
                    op: CustomOp::Instant,
                    name: Some("WorktreeFileChange".to_string()),
                    id: None,
                    args,
                    value: None,
                    flow_out: None,
                    flow_in: None,
                },
                now_ns,
            );
        }
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

        // 3. /proc/meminfo -> machine/mem_used, mem_avail, mem_cached, swap_used
        if let Some(mem) = self.sample_meminfo() {
            send_sample_if_changed(
                appender,
                &mut self.last_samples,
                "machine/mem_used",
                mem.used_bytes as i64,
                Some("bytes"),
                now_ns,
            );
            send_sample_if_changed(
                appender,
                &mut self.last_samples,
                "machine/mem_avail",
                mem.avail_bytes as i64,
                Some("bytes"),
                now_ns,
            );
            send_sample_if_changed(
                appender,
                &mut self.last_samples,
                "machine/mem_cached",
                mem.cached_bytes as i64,
                Some("bytes"),
                now_ns,
            );
            send_sample_if_changed(
                appender,
                &mut self.last_samples,
                "machine/swap_used",
                mem.swap_used_bytes as i64,
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

        // 5. Per-run process tree sampling, child process slices, worktree progress, and dead worker reaping
        let active_runs = appender.active_runs();

        // Close any leftover proc slices for runs that ended normally and left `active_runs`
        let ended_sessions: Vec<SessionKey> = self
            .proc_stacks
            .keys()
            .filter(|s| !active_runs.iter().any(|r| &r.session == *s))
            .cloned()
            .collect();
        for s in ended_sessions {
            if let Some(mut stack) = self.proc_stacks.remove(&s) {
                Self::close_all_proc_slices(&mut stack, appender, now_ns);
            }
        }
        self.worktree_states.retain(|cwd, _| {
            active_runs
                .iter()
                .any(|r| r.cwd.as_deref() == Some(cwd.as_str()))
        });

        // Prune reaped_sessions for runs that are no longer active
        self.reaped_sessions
            .retain(|s| active_runs.iter().any(|r| &r.session == s));

        let mut polled_cwds = HashSet::new();
        for run in active_runs.iter() {
            if let Some(ref cwd) = run.cwd {
                if polled_cwds.insert(cwd.as_str()) {
                    // Prefer a non-supervisor worker run if multiple agents share the same cwd
                    let owner_agent = active_runs
                        .iter()
                        .find(|r| {
                            r.cwd.as_deref() == Some(cwd.as_str())
                                && r.agent_id != "supervisor"
                                && r.agent_id != "orchestrator"
                        })
                        .map(|r| r.agent_id.as_str())
                        .unwrap_or(&run.agent_id);
                    self.poll_worktree_progress(owner_agent, cwd, appender, now_ns);
                }
            }

            if run.pid > 0 {
                let is_dead = unsafe {
                    libc::kill(run.pid as i32, 0) != 0 && *libc::__errno_location() == libc::ESRCH
                };
                let is_zombie = if !is_dead {
                    is_process_zombie(run.pid)
                } else {
                    true
                };

                if (is_dead || is_zombie) && !self.reaped_sessions.contains(&run.session) {
                    if let Some(mut stack) = self.proc_stacks.remove(&run.session) {
                        Self::close_all_proc_slices(&mut stack, appender, now_ns);
                    }
                    self.reaped_sessions.insert(run.session.clone());
                    let _ = appender.send_input(
                        Input::RunEnd {
                            source_ts_ns: Some(now_ns),
                            session: run.session.clone(),
                            exit_code: Some(137),
                            reason: Some("worker process died unexpectedly".to_string()),
                        },
                        now_ns,
                    );
                    continue;
                }

                if let Some((cpu_us, rss_bytes, live_procs)) = self.sample_tree_detailed(run.pid) {
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

                    run_track_buf.truncate(run.run_track.len());
                    run_track_buf.push_str("/proc_count");
                    send_sample_if_changed(
                        appender,
                        &mut self.last_samples,
                        run_track_buf,
                        live_procs.len() as i64,
                        Some("procs"),
                        now_ns,
                    );

                    self.update_proc_slices(
                        &run.session,
                        run.pid,
                        &run.run_track,
                        &live_procs,
                        appender,
                        now_ns,
                    );
                }
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
    parse_meminfo(buf).map(|m| m.used_bytes)
}

/// Parses detailed memory statistics in bytes from `/proc/meminfo` bytes without allocation.
pub fn parse_meminfo(buf: &[u8]) -> Option<MemInfoStats> {
    let mut total_kb = None;
    let mut avail_kb = None;
    let mut buffers_kb = 0u64;
    let mut cached_kb = 0u64;
    let mut sreclaimable_kb = 0u64;
    let mut swap_total_kb = 0u64;
    let mut swap_free_kb = 0u64;

    let parse_first_num = |rest: &[u8]| -> Option<u64> {
        rest.split(|&b| b == b' ' || b == b'\t')
            .find(|s| !s.is_empty())
            .and_then(parse_u64_ascii)
    };

    for line in buf.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"MemTotal:") {
            total_kb = parse_first_num(rest);
        } else if let Some(rest) = line.strip_prefix(b"MemAvailable:") {
            avail_kb = parse_first_num(rest);
        } else if let Some(rest) = line.strip_prefix(b"Buffers:") {
            buffers_kb = parse_first_num(rest).unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix(b"Cached:") {
            cached_kb = parse_first_num(rest).unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix(b"SReclaimable:") {
            sreclaimable_kb = parse_first_num(rest).unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix(b"SwapTotal:") {
            swap_total_kb = parse_first_num(rest).unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix(b"SwapFree:") {
            swap_free_kb = parse_first_num(rest).unwrap_or(0);
        }
    }

    match (total_kb, avail_kb) {
        (Some(tot), Some(avail)) => Some(MemInfoStats {
            used_bytes: tot.saturating_sub(avail) * 1024,
            avail_bytes: avail * 1024,
            cached_bytes: (buffers_kb + cached_kb + sreclaimable_kb) * 1024,
            swap_used_bytes: swap_total_kb.saturating_sub(swap_free_kb) * 1024,
        }),
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

/// Checks if a process PID is in zombie state ('Z') in `/proc/<pid>/stat`.
#[inline]
pub fn is_process_zombie(pid: u32) -> bool {
    let path = format!("/proc/{}/stat", pid);
    if let Ok(file) = File::open(&path) {
        let mut buf = [0u8; 256];
        if let Ok(n) = file.read_at(&mut buf, 0) {
            let slice = &buf[..n];
            if let Some(rparen) = slice.iter().rposition(|&b| b == b')') {
                let rest = &slice[rparen + 1..];
                if let Some(state_tok) = rest
                    .split(|&b| b == b' ' || b == b'\t')
                    .find(|s| !s.is_empty())
                {
                    return state_tok == b"Z";
                }
            }
        }
    }
    false
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

/// Reads utime + stime (in clock ticks), RSS (in bytes), PPID, and process name (`comm`) from `/proc/<pid>/stat`.
pub fn read_process_stat_and_comm(pid: u32, page_size: u64) -> Option<(u64, u64, u32, String)> {
    let path = format!("/proc/{}/stat", pid);
    let file = File::open(&path).ok()?;
    let mut buf = [0u8; 1024];
    let n = file.read_at(&mut buf, 0).ok()?;
    let slice = &buf[..n];

    let lparen = slice.iter().position(|&b| b == b'(')?;
    let rparen = slice.iter().rposition(|&b| b == b')')?;
    if rparen <= lparen {
        return None;
    }
    let comm = String::from_utf8_lossy(&slice[lparen + 1..rparen]).to_string();
    let rest = &slice[rparen + 1..];

    let mut tokens = rest
        .split(|&b| b == b' ' || b == b'\t')
        .filter(|s| !s.is_empty());

    let _state = tokens.next()?;
    let ppid_slice = tokens.next()?;
    let ppid = parse_u64_ascii(ppid_slice)? as u32;

    for _ in 0..9 {
        tokens.next()?;
    }

    let utime = parse_u64_ascii(tokens.next()?)?;
    let stime = parse_u64_ascii(tokens.next()?)?;
    let cpu_ticks = utime.saturating_add(stime);

    for _ in 0..8 {
        tokens.next()?;
    }

    let rss_pages = parse_u64_ascii(tokens.next()?)?;
    let rss_bytes = rss_pages.saturating_mul(page_size);

    Some((cpu_ticks, rss_bytes, ppid, comm))
}

/// Reads the full command line for `pid` from `/proc/<pid>/cmdline`, falling back to `fallback_comm`.
pub fn read_process_cmdline(pid: u32, fallback_comm: &str) -> String {
    let path = format!("/proc/{}/cmdline", pid);
    if let Ok(file) = File::open(&path) {
        let mut buf = [0u8; 256];
        if let Ok(n) = file.read_at(&mut buf, 0) {
            if n > 0 {
                let mut s = String::with_capacity(n);
                for &b in &buf[..n] {
                    if b == 0 {
                        if !s.ends_with(' ') {
                            s.push(' ');
                        }
                    } else if b.is_ascii_graphic() || b == b' ' {
                        s.push(b as char);
                    }
                }
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    fallback_comm.to_string()
}

fn resolve_git_dir(cwd: &Path) -> Option<PathBuf> {
    let dot_git = cwd.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    if dot_git.is_file() {
        if let Ok(content) = fs::read_to_string(&dot_git) {
            if let Some(rest) = content.trim().strip_prefix("gitdir:") {
                let gitdir_path = Path::new(rest.trim());
                if gitdir_path.is_absolute() {
                    return Some(gitdir_path.to_path_buf());
                } else {
                    return Some(cwd.join(gitdir_path));
                }
            }
        }
    }
    None
}

fn read_git_head_sha(
    cwd: &Path,
    git_dir: &Path,
    prev_reftable: Option<&str>,
    prev_sha: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Ok(head_content) = fs::read_to_string(git_dir.join("HEAD")) else {
        return (None, None);
    };
    let trimmed = head_content.trim();
    if let Some(ref_rel) = trimmed.strip_prefix("ref:") {
        let ref_rel = ref_rel.trim();
        if ref_rel != "refs/heads/.invalid" {
            let direct_ref = git_dir.join(ref_rel);
            if let Ok(sha) = fs::read_to_string(&direct_ref) {
                let s = sha.trim();
                if !s.is_empty() {
                    return (Some(s.to_string()), None);
                }
            }
        }
        // Check commondir if this is a linked worktree
        let common_dir = fs::read_to_string(git_dir.join("commondir"))
            .ok()
            .map(|c| {
                let p = Path::new(c.trim());
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    git_dir.join(p)
                }
            })
            .unwrap_or_else(|| git_dir.to_path_buf());
        if ref_rel != "refs/heads/.invalid" {
            if let Ok(sha) = fs::read_to_string(common_dir.join(ref_rel)) {
                let s = sha.trim();
                if !s.is_empty() {
                    return (Some(s.to_string()), None);
                }
            }
            if let Ok(packed) = fs::read_to_string(common_dir.join("packed-refs")) {
                for line in packed.lines() {
                    if line.starts_with('#') || line.starts_with('^') {
                        continue;
                    }
                    let mut parts = line.split_whitespace();
                    if let (Some(sha), Some(rname)) = (parts.next(), parts.next()) {
                        if rname == ref_rel {
                            return (Some(sha.to_string()), None);
                        }
                    }
                }
            }
        }
        // Support modern Git reftable storage without spawning git on unchanged ticks
        let tables_list_path = if git_dir.join("reftable/tables.list").exists() {
            Some(git_dir.join("reftable/tables.list"))
        } else if common_dir.join("reftable/tables.list").exists() {
            Some(common_dir.join("reftable/tables.list"))
        } else {
            None
        };
        if let Some(tl_path) = tables_list_path {
            if let Ok(marker) = fs::read_to_string(tl_path) {
                if prev_reftable == Some(marker.as_str()) && prev_sha.is_some() {
                    return (prev_sha.map(|s| s.to_string()), Some(marker));
                }
                if let Ok(out) = std::process::Command::new("git")
                    .arg("-C")
                    .arg(cwd)
                    .args(["rev-parse", "HEAD"])
                    .output()
                {
                    if out.status.success() {
                        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !sha.is_empty() {
                            return (Some(sha), Some(marker));
                        }
                    }
                }
                return (prev_sha.map(|s| s.to_string()), Some(marker));
            }
        }
        (None, None)
    } else if !trimmed.is_empty() {
        (Some(trimmed.to_string()), None)
    } else {
        (None, None)
    }
}

fn scan_worktree_latest_mtime_ns(cwd: &Path) -> (u64, Option<String>) {
    let mut max_mtime_ns = 0u64;
    let mut newest_rel: Option<String> = None;

    let mut stack = vec![(cwd.to_path_buf(), 0usize)];
    let mut entries_visited = 0usize;
    const MAX_ENTRIES: usize = 256;

    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if entries_visited >= MAX_ENTRIES {
                return (max_mtime_ns, newest_rel);
            }
            entries_visited += 1;

            let fname = entry.file_name();
            let name = fname.to_string_lossy();
            if name == ".git"
                || name == "target"
                || name == "node_modules"
                || name == ".venv"
                || name == "dist"
                || name == "build"
            {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                if depth < 3 {
                    stack.push((path, depth + 1));
                }
            } else if meta.is_file() {
                let mtime = (meta.mtime() as u64)
                    .saturating_mul(1_000_000_000)
                    .saturating_add(meta.mtime_nsec() as u64);
                if mtime > max_mtime_ns {
                    max_mtime_ns = mtime;
                    newest_rel = path
                        .strip_prefix(cwd)
                        .ok()
                        .map(|p| p.to_string_lossy().to_string());
                }
            }
        }
    }

    (max_mtime_ns, newest_rel)
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
        let mut child = std::process::Command::new("true").spawn().unwrap();
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

        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .unwrap();
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

        // Next tick reaps dead process and closes all open proc slices in LIFO order
        proc_sampler.sample_tick(&appender, 3_000_000, &mut buf);
        appender.flush();

        let check_log = EventLog::open(&db_path).unwrap();
        let rows = check_log.load_all().unwrap();

        let begins = rows
            .iter()
            .filter(|e| e.track_key == "agent:proc-worker/run:1/procs" && e.kind == Kind::Custom)
            .count();
        let ends = rows
            .iter()
            .filter(|e| {
                e.track_key == "agent:proc-worker/run:1/procs" && e.kind == Kind::CustomEnded
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
    fn test_worktree_commit_and_file_change_progress_signals() {
        use crate::appender::spawn_appender;
        use crate::log::EventLog;
        use std::process::Command;
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
                    role: None,
                },
                1_000_000_000,
            )
            .unwrap();
        appender.flush();

        let mut proc_sampler = ProcSampler::new(&[]);
        let mut buf = String::new();

        // Tick 1: baseline snapshot
        proc_sampler.sample_tick(&appender, 2_000_000_000, &mut buf);
        appender.flush();

        // Modify a file in the worktree (advance mtime)
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(repo_dir.join("README.md"), "updated content\n").unwrap();
        proc_sampler.sample_tick(&appender, 3_000_000_000, &mut buf);
        appender.flush();

        // Commit the file in the worktree
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
        proc_sampler.sample_tick(&appender, 4_000_000_000, &mut buf);
        appender.flush();

        let check_log = EventLog::open(&db_path).unwrap();
        let rows = check_log.load_all().unwrap();
        assert!(
            rows.iter().any(|e| e.kind == Kind::Milestone
                && e.custom_name
                    .as_deref()
                    .is_some_and(|n| n.starts_with("WorktreeFileChange"))),
            "expected WorktreeFileChange milestone"
        );
        assert!(
            rows.iter().any(|e| e.kind == Kind::Milestone
                && e.custom_name
                    .as_deref()
                    .is_some_and(|n| n.starts_with("WorktreeCommit"))),
            "expected WorktreeCommit milestone"
        );
        let live = appender.live_state();
        let agent = live.agents.get("wt-worker").unwrap();
        assert_eq!(agent.commits_succeeded, 1);
        assert!(agent.last_edit_ts_ns.is_some());
    }

    #[test]
    fn test_on_stall_effect_hook_fires() {
        use crate::appender::spawn_appender_with_hook;
        use crate::log::EventLog;
        use tacho_core::{Input, Recorder, SessionKey, ToolInput};

        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("hook_test.sqlite");
        let marker_path = temp.path().join("stall_fired.txt");
        let log = EventLog::open(&db_path).unwrap();
        let recorder = Recorder::new();

        let hook_cmd = format!(
            "printf '%s:%s:%s' \"$TACHO_AGENT\" \"$TACHO_STALL_KIND\" \"$TACHO_SEQ\" > {}",
            marker_path.display()
        );
        let (appender, _thread) = spawn_appender_with_hook(recorder, log, 256, Some(hook_cmd));

        let sess = SessionKey::new("claude", "sess-hook");
        appender
            .send_input(
                Input::RunStart {
                    source_ts_ns: Some(1_000_000),
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
                1_000_000,
            )
            .unwrap();

        for i in 0..4 {
            let ts = 2_000_000 + i * 100_000;
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

        for _ in 0..40 {
            if marker_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let content = std::fs::read_to_string(&marker_path).expect("on_stall hook file created");
        assert!(
            content.starts_with("hook-worker:LOOP:"),
            "unexpected hook payload: {}",
            content
        );
    }
}
