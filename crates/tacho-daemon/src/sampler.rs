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
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tacho_core::{AgentRole, CustomOp, Input, SessionKey};

use crate::appender::AppenderHandle;
use crate::cgroup::AgentCgroup;
use crate::config::SamplerConfig;

static CLK_TCK: OnceLock<u64> = OnceLock::new();
static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
static CGROUP_ATTACH_ERR_LOGGED: AtomicBool = AtomicBool::new(false);

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
struct TrackedProc {
    slot: usize,
    pid: u32,
    ppid: u32,
    comm: String,
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
    last_file_scan: Instant,
    file_hash: u64,
}

/// Stack-buffered process sample details for zero-allocation tick comparisons.
#[derive(Debug, Clone, Copy)]
struct ProcSampleInfo {
    pid: u32,
    ppid: u32,
    proc_cpu_us: u64,
    rss_bytes: u64,
    comm: [u8; 16],
    comm_len: u8,
}

impl ProcSampleInfo {
    #[inline]
    fn comm_bytes(&self) -> &[u8] {
        &self.comm[..self.comm_len as usize]
    }
}

/// One sample of an agent's process tree.
struct TreeSample {
    /// CPU of live tree members plus children they reaped (from `/proc`).
    tree_cpu_us: u64,
    /// Cumulative CPU of the agent's cgroup scope, when attached.
    cgroup_cpu_us: Option<u64>,
    /// Sum of member RSS.
    rss_bytes: u64,
    procs: Vec<ProcSampleInfo>,
}

/// Per-session CPU bookkeeping that keeps `<run>/cpu` cumulative and monotonic
/// across the switch from `/proc` accounting to cgroup accounting.
#[derive(Default)]
struct CpuAccount {
    /// CPU attributed before the cgroup started counting (tree CPU at attach time).
    cgroup_base_us: Option<u64>,
    last_us: u64,
}

impl CpuAccount {
    fn update(&mut self, s: &TreeSample) -> u64 {
        let total = match s.cgroup_cpu_us {
            Some(cg) => {
                let base = *self
                    .cgroup_base_us
                    .get_or_insert(s.tree_cpu_us.saturating_sub(cg));
                base.saturating_add(cg)
            }
            None => s.tree_cpu_us,
        };
        // Cumulative by definition: an unreaped intermediate parent exiting can make the
        // /proc sum dip, which must not read as negative CPU.
        self.last_us = self.last_us.max(total);
        self.last_us
    }
}

/// Persistent `/proc` sampler maintaining open file handles and scratch buffers
/// to eliminate open/close syscalls and heap allocations during polling ticks.
pub struct ProcSampler {
    cpu_accounts: HashMap<SessionKey, CpuAccount>,
    loadavg_file: Option<File>,
    stat_file: Option<File>,
    meminfo_file: Option<File>,
    mount_cstrings: Vec<(String, CString)>,
    last_samples: HashMap<String, i64>,
    prev_cpu: Option<(u64, u64)>, // (busy, total)
    clk_tck: u64,
    page_size: u64,
    tree_pids_scratch: Vec<u32>,
    visited_scratch: HashSet<u32>,
    cgroup_pids_scratch: Vec<u32>,
    reaped_sessions: HashSet<SessionKey>,
    attach_attempted: HashSet<SessionKey>,
    proc_slots: HashMap<SessionKey, HashMap<u32, TrackedProc>>,
    session_run_tracks: HashMap<SessionKey, String>,
    worktree_states: HashMap<String, WorktreeSnapshot>,
    cgroup_enabled: bool,
    worktree_scan_secs: u64,
}

impl ProcSampler {
    /// Creates a new `ProcSampler`, pre-opening persistent handles for `/proc` virtual files.
    pub fn new(mounts: &[String]) -> Self {
        let mount_cstrings: Vec<(String, CString)> = mounts
            .iter()
            .filter_map(|m| CString::new(m.as_str()).ok().map(|c| (m.clone(), c)))
            .collect();

        Self {
            cpu_accounts: HashMap::new(),
            loadavg_file: File::open("/proc/loadavg").ok(),
            stat_file: File::open("/proc/stat").ok(),
            meminfo_file: File::open("/proc/meminfo").ok(),
            mount_cstrings,
            last_samples: HashMap::new(),
            prev_cpu: None,
            clk_tck: get_clock_ticks_per_sec(),
            page_size: get_page_size(),
            tree_pids_scratch: Vec::with_capacity(32),
            visited_scratch: HashSet::with_capacity(32),
            cgroup_pids_scratch: Vec::with_capacity(32),
            reaped_sessions: HashSet::new(),
            attach_attempted: HashSet::new(),
            proc_slots: HashMap::new(),
            session_run_tracks: HashMap::new(),
            worktree_states: HashMap::new(),
            cgroup_enabled: true,
            worktree_scan_secs: 30,
        }
    }

    /// Sets whether cgroup accounting is enabled.
    pub fn set_cgroup_enabled(&mut self, enabled: bool) {
        self.cgroup_enabled = enabled;
    }

    /// Sets the minimum interval between worktree file-change scans in seconds.
    pub fn set_worktree_scan_secs(&mut self, secs: u64) {
        self.worktree_scan_secs = secs;
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
        self.visited_scratch.clear();

        let mut path_buf = [0u8; 64];
        let proc_path = match format_proc_pid_path(&mut path_buf, root_pid) {
            Some(p) => p,
            None => return false,
        };
        if !Path::new(proc_path).exists() {
            return false;
        }

        self.tree_pids_scratch.push(root_pid);
        self.visited_scratch.insert(root_pid);

        let mut children_buf = [0u8; 512];
        let mut fast_success = true;
        let mut idx = 0;

        while idx < self.tree_pids_scratch.len() {
            let pid = self.tree_pids_scratch[idx];
            idx += 1;
            let mut c_path_buf = [0u8; 64];
            if let Some(path_str) = format_proc_task_children(&mut c_path_buf, pid) {
                if let Ok(file) = File::open(path_str) {
                    if let Ok(n) = file.read_at(&mut children_buf, 0) {
                        let slice = &children_buf[..n];
                        for token in slice
                            .split(|&b| b == b' ' || b == b'\t' || b == b'\n')
                            .filter(|s| !s.is_empty())
                        {
                            if let Some(child_pid) = parse_u64_ascii(token) {
                                let cpid = child_pid as u32;
                                if self.visited_scratch.insert(cpid) {
                                    self.tree_pids_scratch.push(cpid);
                                }
                            }
                        }
                        continue;
                    }
                }
            }
            fast_success = false;
            break;
        }

        if !fast_success {
            let full_tree = find_process_tree(root_pid);
            self.tree_pids_scratch.clear();
            self.visited_scratch.clear();
            for pid in full_tree {
                if self.visited_scratch.insert(pid) {
                    self.tree_pids_scratch.push(pid);
                }
            }
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

    /// Samples the process tree rooted at `root_pid` optionally augmented by cgroup members.
    fn sample_tree_detailed(
        &mut self,
        root_pid: u32,
        cgroup: Option<&AgentCgroup>,
    ) -> Option<TreeSample> {
        if !self.populate_tree_pids(root_pid) {
            return None;
        }

        // Union with cgroup.procs to catch reparented daemons
        if let Some(cg) = cgroup {
            self.cgroup_pids_scratch.clear();
            cg.pids(&mut self.cgroup_pids_scratch);
            for &cpid in &self.cgroup_pids_scratch {
                if self.visited_scratch.insert(cpid) {
                    self.tree_pids_scratch.push(cpid);
                }
            }
        }

        let mut total_proc_cpu_ticks = 0u64;
        let mut total_rss_bytes = 0u64;
        let mut live_procs = Vec::with_capacity(self.tree_pids_scratch.len());
        let mut stat_buf = [0u8; 1024];

        for &pid in &self.tree_pids_scratch {
            if let Some(st) = read_process_stat_and_comm_slice(pid, self.page_size, &mut stat_buf) {
                let (ticks, rss, ppid) = (st.cpu_ticks, st.rss_bytes, st.ppid);
                // Reaped children no longer appear in the walk, but their CPU lives on in the
                // parent's cutime/cstime, so the tree total does not drop when a build exits.
                total_proc_cpu_ticks = total_proc_cpu_ticks
                    .saturating_add(ticks)
                    .saturating_add(st.reaped_child_ticks);
                total_rss_bytes = total_rss_bytes.saturating_add(rss);
                let proc_cpu_us = (ticks * 1_000_000) / self.clk_tck;

                let comm_slice = &stat_buf[st.comm_start..st.comm_end];
                let mut comm = [0u8; 16];
                let copy_len = comm_slice.len().min(16);
                comm[..copy_len].copy_from_slice(&comm_slice[..copy_len]);

                live_procs.push(ProcSampleInfo {
                    pid,
                    ppid,
                    proc_cpu_us,
                    rss_bytes: rss,
                    comm,
                    comm_len: copy_len as u8,
                });
            }
        }

        let tree_cpu_us = (total_proc_cpu_ticks * 1_000_000) / self.clk_tck;
        let cgroup_cpu_us = cgroup.and_then(|cg| cg.cpu_usage_us());

        Some(TreeSample {
            tree_cpu_us,
            cgroup_cpu_us,
            rss_bytes: total_rss_bytes,
            procs: live_procs,
        })
    }

    fn close_all_proc_slices(
        session_slots: &mut HashMap<u32, TrackedProc>,
        run_track: &str,
        appender: &AppenderHandle,
        now_ns: u64,
    ) {
        for (_, tp) in session_slots.drain() {
            let slot_track = format!("{}/procs/slot:{}", run_track, tp.slot);
            emit_proc_end(appender, &slot_track, &tp, now_ns);
        }
    }

    fn update_proc_slices(
        &mut self,
        session: &SessionKey,
        root_pid: u32,
        run_track: &str,
        live_procs: &[ProcSampleInfo],
        appender: &AppenderHandle,
        now_ns: u64,
    ) {
        let session_procs = self.proc_slots.entry(session.clone()).or_default();
        self.session_run_tracks
            .insert(session.clone(), run_track.to_string());

        // 1. Detect exited processes: pids in session_procs but not in live_procs
        let live_pids: HashSet<u32> = live_procs.iter().map(|lp| lp.pid).collect();
        let mut exited_pids = Vec::new();
        for (&pid, tp) in session_procs.iter() {
            if !live_pids.contains(&pid) {
                let slot_track = format!("{}/procs/slot:{}", run_track, tp.slot);
                emit_proc_end(appender, &slot_track, tp, now_ns);
                exited_pids.push(pid);
            }
        }
        for pid in exited_pids {
            session_procs.remove(&pid);
        }

        // 2. Update existing live processes or handle exec (comm change)
        for lp in live_procs {
            if let Some(tp) = session_procs.get_mut(&lp.pid) {
                tp.last_cpu_us = lp.proc_cpu_us;
                tp.last_rss_bytes = lp.rss_bytes;
                tp.peak_rss_bytes = tp.peak_rss_bytes.max(lp.rss_bytes);

                if tp.comm.as_bytes() != lp.comm_bytes() {
                    // Exec! Comm changed -> End on slot, then Begin on SAME slot!
                    let slot_track = format!("{}/procs/slot:{}", run_track, tp.slot);
                    emit_proc_end(appender, &slot_track, tp, now_ns);

                    let new_comm = String::from_utf8_lossy(lp.comm_bytes()).into_owned();
                    tp.comm = new_comm.clone();
                    tp.cmdline = read_process_cmdline(lp.pid, &new_comm);
                    tp.ppid = lp.ppid;

                    let slice_name = make_slice_name(lp.pid, root_pid, &tp.comm, &tp.cmdline);
                    emit_proc_begin(appender, &slot_track, tp, &slice_name, now_ns);
                }
            }
        }

        // 3. Collect new processes (in live_procs but not in session_procs)
        // Ensure root_pid is allocated first if it is among new processes
        let mut new_procs: Vec<&ProcSampleInfo> = live_procs
            .iter()
            .filter(|lp| !session_procs.contains_key(&lp.pid))
            .collect();
        new_procs.sort_by_key(|lp| if lp.pid == root_pid { 0 } else { 1 });

        if !new_procs.is_empty() {
            let mut used_slots: HashSet<usize> = session_procs.values().map(|tp| tp.slot).collect();

            for lp in new_procs {
                // Find lowest free slot index k
                let mut slot = 0;
                while used_slots.contains(&slot) {
                    slot += 1;
                }
                used_slots.insert(slot);

                let comm = String::from_utf8_lossy(lp.comm_bytes()).into_owned();
                let cmdline = read_process_cmdline(lp.pid, &comm);
                let slice_name = make_slice_name(lp.pid, root_pid, &comm, &cmdline);

                let tp = TrackedProc {
                    slot,
                    pid: lp.pid,
                    ppid: lp.ppid,
                    comm,
                    cmdline,
                    last_cpu_us: lp.proc_cpu_us,
                    last_rss_bytes: lp.rss_bytes,
                    peak_rss_bytes: lp.rss_bytes,
                };

                let slot_track = format!("{}/procs/slot:{}", run_track, slot);
                emit_proc_begin(appender, &slot_track, &tp, &slice_name, now_ns);
                session_procs.insert(lp.pid, tp);
            }
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
        if git_dir.is_none() {
            return;
        }

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

        let Some(prev) = self.worktree_states.get_mut(cwd) else {
            // First time seeing this worktree: baseline snapshot
            let (latest_mtime_ns, _newest, file_hash) =
                scan_worktree_files(cwd_path, Duration::from_secs(5)).unwrap_or((0, None, 0));

            self.worktree_states.insert(
                cwd.to_string(),
                WorktreeSnapshot {
                    head_sha,
                    reftable_marker,
                    latest_mtime_ns,
                    last_file_scan: Instant::now(),
                    file_hash,
                },
            );
            return;
        };
        prev.reftable_marker = reftable_marker;

        let mut head_changed = false;
        if let Some(new_sha) = head_sha
            .as_ref()
            .filter(|s| Some(*s) != prev.head_sha.as_ref())
        {
            head_changed = true;
            let old_sha_opt = prev.head_sha.take();

            if let Some(ref old_sha) = old_sha_opt {
                let range_old_to_new = format!("{}..{}", old_sha, new_sha);
                let range_new_to_old = format!("{}..{}", new_sha, old_sha);

                let old_to_new = run_git(
                    cwd_path,
                    &["rev-list", "--count", &range_old_to_new],
                    Duration::from_secs(2),
                )
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);

                let new_to_old = run_git(
                    cwd_path,
                    &["rev-list", "--count", &range_new_to_old],
                    Duration::from_secs(2),
                )
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);

                if new_to_old == 0 && old_to_new > 0 {
                    // Forward-only commit
                    let mut args = BTreeMap::new();
                    args.insert("sha".to_string(), new_sha.clone());
                    args.insert("commits".to_string(), old_to_new.to_string());
                    args.insert("cwd".to_string(), cwd.to_string());

                    let _ = appender.send_input(
                        Input::Custom {
                            source_ts_ns: Some(now_ns),
                            track: format!("agent:{}", agent_id),
                            op: CustomOp::Instant,
                            name: Some("WorktreeCommit".to_string()),
                            id: Some(new_sha.clone()),
                            args,
                            value: None,
                            flow_out: None,
                            flow_in: None,
                        },
                        now_ns,
                    );
                } else {
                    // Non forward-only (checkout, reset, rebase, etc.) -> WorktreeHeadMoved
                    let mut args = BTreeMap::new();
                    args.insert("from".to_string(), old_sha.clone());
                    args.insert("to".to_string(), new_sha.clone());
                    args.insert("cwd".to_string(), cwd.to_string());

                    let _ = appender.send_input(
                        Input::Custom {
                            source_ts_ns: Some(now_ns),
                            track: format!("agent:{}", agent_id),
                            op: CustomOp::Instant,
                            name: Some("WorktreeHeadMoved".to_string()),
                            id: Some(new_sha.clone()),
                            args,
                            value: None,
                            flow_out: None,
                            flow_in: None,
                        },
                        now_ns,
                    );
                }
            } else {
                let count = run_git(
                    cwd_path,
                    &["rev-list", "--count", new_sha],
                    Duration::from_secs(2),
                )
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(1);

                let mut args = BTreeMap::new();
                args.insert("sha".to_string(), new_sha.clone());
                args.insert("commits".to_string(), count.to_string());
                args.insert("cwd".to_string(), cwd.to_string());

                let _ = appender.send_input(
                    Input::Custom {
                        source_ts_ns: Some(now_ns),
                        track: format!("agent:{}", agent_id),
                        op: CustomOp::Instant,
                        name: Some("WorktreeCommit".to_string()),
                        id: Some(new_sha.clone()),
                        args,
                        value: None,
                        flow_out: None,
                        flow_in: None,
                    },
                    now_ns,
                );
            }
            prev.head_sha = head_sha;
        }

        // Check file modifications at most once every worktree_scan_secs
        let should_scan_files = !head_changed
            && prev.last_file_scan.elapsed() >= Duration::from_secs(self.worktree_scan_secs);

        if should_scan_files {
            prev.last_file_scan = Instant::now();
            if let Ok((max_mtime_ns, newest_rel, hash)) =
                scan_worktree_files(cwd_path, Duration::from_secs(5))
            {
                if hash != prev.file_hash {
                    prev.file_hash = hash;
                    prev.latest_mtime_ns = max_mtime_ns;

                    if let Some(rel) = newest_rel {
                        let mut args = BTreeMap::new();
                        args.insert("cwd".to_string(), cwd.to_string());
                        args.insert("detail".to_string(), rel);

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
            }
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
            .proc_slots
            .keys()
            .filter(|s| !active_runs.iter().any(|r| &r.session == *s))
            .cloned()
            .collect();
        for s in ended_sessions {
            if let Some(mut slots) = self.proc_slots.remove(&s) {
                let run_track = self.session_run_tracks.remove(&s).unwrap_or_default();
                Self::close_all_proc_slices(&mut slots, &run_track, appender, now_ns);
            }
        }
        self.session_run_tracks
            .retain(|s, _| active_runs.iter().any(|r| &r.session == s));
        self.worktree_states.retain(|cwd, _| {
            active_runs
                .iter()
                .any(|r| r.cwd.as_deref() == Some(cwd.as_str()))
        });

        // Prune reaped_sessions and attach_attempted for runs that are no longer active
        self.reaped_sessions
            .retain(|s| active_runs.iter().any(|r| &r.session == s));
        self.attach_attempted
            .retain(|s| active_runs.iter().any(|r| &r.session == s));
        self.cpu_accounts
            .retain(|s, _| active_runs.iter().any(|r| &r.session == s));

        let live_state = appender.live_state();
        let mut polled_cwds = HashSet::new();
        for run in active_runs.iter() {
            if let Some(ref cwd) = run.cwd {
                if polled_cwds.insert(cwd.as_str()) {
                    // Prefer the Worker run with the most recent edit/command activity if multiple agents share cwd
                    let recent_ts = |agent_id: &str| -> u64 {
                        live_state
                            .agents
                            .get(agent_id)
                            .map(|a| {
                                let edit_ts = a.last_edit_ts_ns.unwrap_or(0);
                                let cmd_ts = a.recent_commands.last().map(|c| c.ts_ns).unwrap_or(0);
                                edit_ts.max(cmd_ts)
                            })
                            .unwrap_or(0)
                    };
                    let owner_agent = active_runs
                        .iter()
                        .filter(|r| {
                            r.cwd.as_deref() == Some(cwd.as_str()) && r.role == AgentRole::Worker
                        })
                        .max_by_key(|r| recent_ts(&r.agent_id))
                        .or_else(|| {
                            active_runs
                                .iter()
                                .filter(|r| {
                                    r.cwd.as_deref() == Some(cwd.as_str())
                                        && r.role != AgentRole::Orchestrator
                                })
                                .max_by_key(|r| recent_ts(&r.agent_id))
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
                    if let Some(mut slots) = self.proc_slots.remove(&run.session) {
                        Self::close_all_proc_slices(&mut slots, &run.run_track, appender, now_ns);
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

                // Cgroup scope discovery / attachment (attach-once semantics). Only this run's
                // own scope counts: a sub-agent started inside another agent's scope must not
                // inherit that agent's totals.
                let run_n = run
                    .run_track
                    .rsplit("run:")
                    .next()
                    .and_then(|s| s.split('/').next())
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(1);
                let unit = crate::cgroup::unit_name(&run.agent_id, run_n, run.pid);
                let mut cgroup = AgentCgroup::of_pid(run.pid, &unit);
                if cgroup.is_none()
                    && self.cgroup_enabled
                    && !self.attach_attempted.contains(&run.session)
                {
                    self.attach_attempted.insert(run.session.clone());
                    if self.populate_tree_pids(run.pid) {
                        if let Err(e) = AgentCgroup::attach(&unit, &self.tree_pids_scratch) {
                            if !CGROUP_ATTACH_ERR_LOGGED.swap(true, Ordering::Relaxed) {
                                eprintln!(
                                    "[tacho-daemon] cgroup attach failed ({}), falling back to /proc sampling",
                                    e
                                );
                            }
                        } else {
                            cgroup = AgentCgroup::of_pid(run.pid, &unit);
                        }
                    }
                }

                if let Some(sample) = self.sample_tree_detailed(run.pid, cgroup.as_ref()) {
                    let cpu_us = self
                        .cpu_accounts
                        .entry(run.session.clone())
                        .or_default()
                        .update(&sample);
                    let rss_bytes = sample.rss_bytes;
                    let live_procs = sample.procs;
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

                    if let Some(ref cg) = cgroup {
                        if let Some(mem_curr) = cg.memory_current() {
                            run_track_buf.truncate(run.run_track.len());
                            run_track_buf.push_str("/cgroup_mem");
                            send_sample_if_changed(
                                appender,
                                &mut self.last_samples,
                                run_track_buf,
                                mem_curr as i64,
                                Some("bytes"),
                                now_ns,
                            );
                        }
                        if let Some(mem_peak) = cg.memory_peak() {
                            run_track_buf.truncate(run.run_track.len());
                            run_track_buf.push_str("/cgroup_mem_peak");
                            send_sample_if_changed(
                                appender,
                                &mut self.last_samples,
                                run_track_buf,
                                mem_peak as i64,
                                Some("bytes"),
                                now_ns,
                            );
                        }
                    }

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

fn make_slice_name(pid: u32, root_pid: u32, comm: &str, cmdline: &str) -> String {
    if pid == root_pid {
        format!("{} [pid {}]", comm, pid)
    } else {
        let short_cmd: String = cmdline.chars().take(48).collect();
        if short_cmd.is_empty() || short_cmd == comm {
            format!("{} [pid {}]", comm, pid)
        } else {
            format!("{} [pid {}]: {}", comm, pid, short_cmd)
        }
    }
}

fn emit_proc_begin(
    appender: &AppenderHandle,
    slot_track: &str,
    tp: &TrackedProc,
    slice_name: &str,
    now_ns: u64,
) {
    let mut args = BTreeMap::new();
    args.insert("pid".to_string(), tp.pid.to_string());
    args.insert("ppid".to_string(), tp.ppid.to_string());
    args.insert("comm".to_string(), tp.comm.clone());
    args.insert("cmdline".to_string(), tp.cmdline.clone());
    args.insert("rss_bytes".to_string(), tp.last_rss_bytes.to_string());

    let _ = appender.send_input(
        Input::Custom {
            source_ts_ns: Some(now_ns),
            track: slot_track.to_string(),
            op: CustomOp::Begin,
            name: Some(slice_name.to_string()),
            id: Some(tp.pid.to_string()),
            args,
            value: None,
            flow_out: None,
            flow_in: None,
        },
        now_ns,
    );
}

fn emit_proc_end(appender: &AppenderHandle, slot_track: &str, tp: &TrackedProc, now_ns: u64) {
    let mut args = BTreeMap::new();
    args.insert("pid".to_string(), tp.pid.to_string());
    args.insert("cpu_us".to_string(), tp.last_cpu_us.to_string());
    args.insert("rss_bytes".to_string(), tp.last_rss_bytes.to_string());
    args.insert("peak_rss_bytes".to_string(), tp.peak_rss_bytes.to_string());

    let _ = appender.send_input(
        Input::Custom {
            source_ts_ns: Some(now_ns),
            track: slot_track.to_string(),
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
    proc_sampler.set_cgroup_enabled(config.cgroup);
    proc_sampler.set_worktree_scan_secs(config.worktree_scan_secs);
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

#[inline]
fn format_proc_pid_path(buf: &mut [u8; 64], pid: u32) -> Option<&str> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    write!(cursor, "/proc/{}", pid).ok()?;
    let len = cursor.position() as usize;
    std::str::from_utf8(&buf[..len]).ok()
}

#[inline]
fn format_proc_task_children(buf: &mut [u8; 64], pid: u32) -> Option<&str> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    write!(cursor, "/proc/{}/task/{}/children", pid, pid).ok()?;
    let len = cursor.position() as usize;
    std::str::from_utf8(&buf[..len]).ok()
}

#[inline]
fn format_proc_stat_path(buf: &mut [u8; 64], pid: u32) -> Option<&str> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    write!(cursor, "/proc/{}/stat", pid).ok()?;
    let len = cursor.position() as usize;
    std::str::from_utf8(&buf[..len]).ok()
}

#[inline]
fn format_proc_cmdline_path(buf: &mut [u8; 64], pid: u32) -> Option<&str> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    write!(cursor, "/proc/{}/cmdline", pid).ok()?;
    let len = cursor.position() as usize;
    std::str::from_utf8(&buf[..len]).ok()
}

/// Checks if a process PID is in zombie state ('Z') in `/proc/<pid>/stat`.
#[inline]
pub fn is_process_zombie(pid: u32) -> bool {
    let mut path_buf = [0u8; 64];
    let path = match format_proc_stat_path(&mut path_buf, pid) {
        Some(p) => p,
        None => return false,
    };
    if let Ok(file) = File::open(path) {
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
pub fn read_process_stat_fields(pid: u32, page_size: u64) -> Option<(u64, u64, u32)> {
    let mut path_buf = [0u8; 64];
    let path = format_proc_stat_path(&mut path_buf, pid)?;
    let file = File::open(path).ok()?;
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

/// Reads utime + stime, RSS, PPID, and the exact byte range of `comm` in the provided buffer.
pub fn read_process_stat_and_comm_slice(
    pid: u32,
    page_size: u64,
    buf: &mut [u8; 1024],
) -> Option<ProcStat> {
    let mut path_buf = [0u8; 64];
    let path = format_proc_stat_path(&mut path_buf, pid)?;
    let file = File::open(path).ok()?;
    let n = file.read_at(buf, 0).ok()?;
    let slice = &buf[..n];

    let lparen = slice.iter().position(|&b| b == b'(')?;
    let rparen = slice.iter().rposition(|&b| b == b')')?;
    if rparen <= lparen {
        return None;
    }
    let comm_start = lparen + 1;
    let comm_end = rparen;

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
    let cutime = parse_u64_ascii(tokens.next()?)?;
    let cstime = parse_u64_ascii(tokens.next()?)?;
    let reaped_child_ticks = cutime.saturating_add(cstime);

    // priority, nice, num_threads, itrealvalue, starttime, vsize
    for _ in 0..6 {
        tokens.next()?;
    }

    let rss_pages = parse_u64_ascii(tokens.next()?)?;
    let rss_bytes = rss_pages.saturating_mul(page_size);

    Some(ProcStat {
        cpu_ticks,
        reaped_child_ticks,
        rss_bytes,
        ppid,
        comm_start,
        comm_end,
    })
}

/// Fields parsed from `/proc/<pid>/stat` by [`read_process_stat_and_comm_slice`].
#[derive(Debug, Clone, Copy)]
pub struct ProcStat {
    /// Own CPU time (`utime + stime`) in clock ticks.
    pub cpu_ticks: u64,
    /// CPU time of exited children this process has waited for (`cutime + cstime`).
    pub reaped_child_ticks: u64,
    pub rss_bytes: u64,
    pub ppid: u32,
    /// Byte range of `comm` inside the caller's buffer.
    pub comm_start: usize,
    pub comm_end: usize,
}

/// Reads utime + stime (in clock ticks), RSS (in bytes), PPID, and process name (`comm`) from `/proc/<pid>/stat`.
pub fn read_process_stat_and_comm(pid: u32, page_size: u64) -> Option<(u64, u64, u32, String)> {
    let mut buf = [0u8; 1024];
    let st = read_process_stat_and_comm_slice(pid, page_size, &mut buf)?;
    let comm = String::from_utf8_lossy(&buf[st.comm_start..st.comm_end]).into_owned();
    Some((st.cpu_ticks, st.rss_bytes, st.ppid, comm))
}

/// Reads the full command line for `pid` from `/proc/<pid>/cmdline` into a 4 KiB buffer,
/// replacing NUL bytes with spaces.
pub fn read_process_cmdline(pid: u32, fallback_comm: &str) -> String {
    let mut path_buf = [0u8; 64];
    let path = match format_proc_cmdline_path(&mut path_buf, pid) {
        Some(p) => p,
        None => return fallback_comm.to_string(),
    };
    if let Ok(file) = File::open(path) {
        let mut buf = [0u8; 4096];
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

/// Executes a git command in `cwd` with a strict timeout, killing the process on expiration.
pub fn run_git(cwd: &Path, args: &[&str], timeout: Duration) -> io::Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd).args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn()?;
    let start = Instant::now();

    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                let mut out = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_string(&mut out);
                }
                return Ok(out);
            } else {
                return Err(io::Error::other(format!(
                    "git command failed: status {:?}",
                    status.code()
                )));
            }
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "git command timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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
                if let Ok(out) = run_git(cwd, &["rev-parse", "HEAD"], Duration::from_secs(2)) {
                    let sha = out.trim().to_string();
                    if !sha.is_empty() {
                        return (Some(sha), Some(marker));
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

/// Scans the worktree for modified and untracked files respecting `.gitignore`,
/// returning `(max_mtime_ns, newest_rel_path, 64-bit content hash)`.
pub fn scan_worktree_files(
    cwd: &Path,
    timeout: Duration,
) -> io::Result<(u64, Option<String>, u64)> {
    let out = run_git(
        cwd,
        &["ls-files", "-m", "-o", "--exclude-standard", "-z"],
        timeout,
    )?;

    let mut files = Vec::new();
    let mut max_mtime_ns = 0u64;
    let mut newest_rel = None;

    for rel_path in out.split('\0') {
        if rel_path.is_empty() {
            continue;
        }
        let full_path = cwd.join(rel_path);
        if let Ok(meta) = fs::metadata(&full_path) {
            let mtime = (meta.mtime() as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(meta.mtime_nsec() as u64);
            let size = meta.len();
            files.push((rel_path.to_string(), mtime, size));
            if mtime > max_mtime_ns {
                max_mtime_ns = mtime;
                newest_rel = Some(rel_path.to_string());
            }
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    for (path, mtime, size) in &files {
        hasher.update(path.as_bytes());
        hasher.update(&mtime.to_le_bytes());
        hasher.update(&size.to_le_bytes());
    }
    let hash = hasher.digest();

    Ok((max_mtime_ns, newest_rel, hash))
}

/// Backward-compatible wrapper for scanning latest mtime.
pub fn scan_worktree_latest_mtime_ns(cwd: &Path) -> (u64, Option<String>) {
    scan_worktree_files(cwd, Duration::from_secs(5))
        .map(|(m, p, _)| (m, p))
        .unwrap_or((0, None))
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
        assert!(rows
            .iter()
            .any(|e| e.track_key == format!("{}/procs/slot:1", run_track)
                && e.kind == Kind::CustomEnded));
        // Slot 2 must only have ONE Begin event across tick 1 and 2 (survivor not re-begun)
        let slot2_begins = rows
            .iter()
            .filter(|e| {
                e.track_key == format!("{}/procs/slot:2", run_track) && e.kind == Kind::Custom
            })
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
            .filter(|e| {
                e.track_key == format!("{}/procs/slot:2", run_track) && e.kind == Kind::Custom
            })
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

        // Spawn a daemonized burner that escapes the process tree via setsid
        let mut child = match Command::new("sh")
            .args(["-c", "setsid sh -c \"yes > /dev/null & sleep 3\" & sleep 3"])
            .spawn()
        {
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
                eprintln!(
                    "skipping cgroup integration test: cgroup attach failed or scope not found"
                );
                return;
            }
        };
        assert!(cg.dir.exists(), "scope cgroup dir must exist: {:?}", cg.dir);

        // Sleep ~600ms so `yes` burns CPU
        std::thread::sleep(Duration::from_millis(600));

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

        // 2. Assert daemonized `yes` appears in a procs slot slice name
        let has_yes_slice = rows.iter().any(|e| {
            e.track_key
                .starts_with("agent:cgroup-worker/run:1/procs/slot:")
                && e.custom_name
                    .as_deref()
                    .is_some_and(|name| name.contains("yes"))
        });
        assert!(
            has_yes_slice,
            "daemonized yes process must appear in a procs slot slice"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_on_stall_effect_hook_fires() {
        use crate::appender::spawn_appender_with_hook;
        use crate::log::EventLog;
        use std::time::{Instant, SystemTime, UNIX_EPOCH};
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

        let poll_start = Instant::now();
        let poll_timeout = Duration::from_secs(5);
        while !marker_path.exists() && poll_start.elapsed() < poll_timeout {
            std::thread::sleep(Duration::from_millis(10));
        }

        let content = std::fs::read_to_string(&marker_path).expect("on_stall hook file created");
        assert!(
            content.starts_with("hook-worker:LOOP:"),
            "unexpected hook payload: {}",
            content
        );
    }
}
