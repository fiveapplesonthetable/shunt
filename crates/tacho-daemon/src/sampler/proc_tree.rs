//! Process tree discovery, `/proc/<pid>` sampling, and Perfetto slot slice projection.
//!
//! Discovers all PIDs in each active run's process tree via `/proc/<pid>/task/<tid>/children`
//! (unioning with `cgroup.procs` when attached to catch reparented daemons), samples CPU and
//! RSS from `/proc/<pid>/stat`, and projects live processes onto stable `procs/slot:<k>` tracks.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use tacho_core::{CustomOp, Input, SessionKey};

use crate::appender::{ActiveRun, AppenderHandle};
use crate::cgroup::AgentCgroup;

use super::{get_page_size, parse_u64_ascii, ProcSampler};

#[derive(Debug, Clone)]
pub(super) struct TrackedProc {
    pub(super) slot: usize,
    pub(super) pid: u32,
    pub(super) start_ticks: u64,
    pub(super) ppid: u32,
    pub(super) comm: String,
    pub(super) cmdline: String,
    pub(super) last_cpu_us: u64,
    pub(super) last_rss_bytes: u64,
    pub(super) peak_rss_bytes: u64,
}

/// Stack-buffered process sample details for zero-allocation tick comparisons.
#[derive(Debug, Clone, Copy)]
pub(super) struct ProcSampleInfo {
    pub(super) pid: u32,
    pub(super) start_ticks: u64,
    pub(super) ppid: u32,
    pub(super) proc_cpu_us: u64,
    pub(super) rss_bytes: u64,
    pub(super) comm: [u8; 16],
    pub(super) comm_len: u8,
}

impl ProcSampleInfo {
    #[inline]
    pub(super) fn comm_bytes(&self) -> &[u8] {
        &self.comm[..self.comm_len as usize]
    }
}

/// One sample of an agent's process tree.
pub(super) struct TreeSample {
    /// CPU of live tree members plus children they reaped (from `/proc`).
    pub(super) tree_cpu_us: u64,
    /// Cumulative CPU of the agent's cgroup scope, when attached.
    pub(super) cgroup_cpu_us: Option<u64>,
    /// Sum of member RSS.
    pub(super) rss_bytes: u64,
    pub(super) procs: Vec<ProcSampleInfo>,
}

/// Per-session CPU bookkeeping that keeps `<run>/cpu` cumulative and monotonic
/// across the switch from `/proc` accounting to cgroup accounting.
#[derive(Default)]
pub(super) struct CpuAccount {
    /// CPU attributed before the cgroup started counting (tree CPU at attach time).
    cgroup_base_us: Option<u64>,
    last_us: u64,
}

impl CpuAccount {
    pub(super) fn update(&mut self, s: &TreeSample) -> u64 {
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

/// Fields parsed from `/proc/<pid>/stat` by [`read_process_stat_and_comm_slice`].
#[derive(Debug, Clone, Copy)]
pub struct ProcStat {
    /// Own CPU time (`utime + stime`) in clock ticks.
    pub cpu_ticks: u64,
    /// CPU time of exited children this process has waited for (`cutime + cstime`).
    pub reaped_child_ticks: u64,
    /// Start time in clock ticks since boot; with the pid it identifies a process uniquely,
    /// so a recycled pid is never mistaken for the process we were tracking.
    pub start_ticks: u64,
    pub rss_bytes: u64,
    pub ppid: u32,
    /// Byte range of `comm` inside the caller's buffer.
    pub comm_start: usize,
    pub comm_end: usize,
}

impl ProcSampler {
    /// Discovers all PIDs in the process tree rooted at `root_pid` in parent-first BFS order.
    ///
    /// Children are listed per thread in `/proc/<pid>/task/<tid>/children`, so every thread
    /// is read: a multi-threaded agent (node, python) often forks from a non-main thread.
    /// Falls back to a full `/proc` scan when the kernel lacks `children` files.
    pub(super) fn populate_tree_pids(&mut self, root_pid: u32) -> bool {
        self.tree_pids_scratch.clear();
        self.visited_scratch.clear();
        if !Path::new(&format!("/proc/{}", root_pid)).exists() {
            return false;
        }
        self.tree_pids_scratch.push(root_pid);
        self.visited_scratch.insert(root_pid);

        let mut children = Vec::new();
        let mut idx = 0;
        while idx < self.tree_pids_scratch.len() {
            let pid = self.tree_pids_scratch[idx];
            idx += 1;
            children.clear();
            match read_children_of(pid, &mut children) {
                Ok(()) => {
                    for &cpid in &children {
                        if self.visited_scratch.insert(cpid) {
                            self.tree_pids_scratch.push(cpid);
                        }
                    }
                }
                // The process exited mid-walk: it simply has no children any more.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    let full_tree = find_process_tree(root_pid);
                    self.tree_pids_scratch.clear();
                    self.visited_scratch.clear();
                    for pid in full_tree {
                        if self.visited_scratch.insert(pid) {
                            self.tree_pids_scratch.push(pid);
                        }
                    }
                    break;
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
    pub(super) fn sample_tree_detailed(
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
                    start_ticks: st.start_ticks,
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

    pub(super) fn close_all_proc_slices(
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

    pub(super) fn update_proc_slices(
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
        let live_ids: HashSet<(u32, u64)> = live_procs
            .iter()
            .map(|lp| (lp.pid, lp.start_ticks))
            .collect();
        let mut exited_pids = Vec::new();
        for (&pid, tp) in session_procs.iter() {
            if !live_ids.contains(&(pid, tp.start_ticks)) {
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
                    start_ticks: lp.start_ticks,
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

    /// True once a run's root process has exited, become a zombie, or been replaced by an
    /// unrelated process that recycled its pid (detected by a changed start time).
    pub(super) fn run_process_gone(&mut self, run: &ActiveRun) -> bool {
        let mut buf = [0u8; 1024];
        let Some(st) = read_process_stat_and_comm_slice(run.pid, self.page_size, &mut buf) else {
            return true; // No /proc entry: the process is gone.
        };
        if is_process_zombie(run.pid) {
            return true;
        }
        let first_seen = *self
            .root_start_ticks
            .entry(run.session.clone())
            .or_insert(st.start_ticks);
        first_seen != st.start_ticks
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

/// Appends the children of every thread of `pid` to `out`.
///
/// Reads each `children` file to the end: a fixed buffer would cut the last pid in half
/// once a process has more than a few dozen children.
fn read_children_of(pid: u32, out: &mut Vec<u32>) -> io::Result<()> {
    use std::io::Read;
    let mut text = Vec::new();
    for task in fs::read_dir(format!("/proc/{}/task", pid))? {
        let task = task?;
        text.clear();
        match File::open(task.path().join("children")) {
            Ok(mut f) => {
                f.read_to_end(&mut text)?;
            }
            // The thread exited between readdir and open.
            Err(e) if e.kind() == io::ErrorKind::NotFound && !task.path().exists() => continue,
            // The thread exists but has no `children` file: kernel without CONFIG_PROC_CHILDREN.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, e))
            }
            Err(e) => return Err(e),
        }
        out.extend(parse_pid_list(&text));
    }
    Ok(())
}

/// Parses a whitespace-separated pid list, skipping malformed tokens.
pub(super) fn parse_pid_list(text: &[u8]) -> impl Iterator<Item = u32> + '_ {
    text.split(|b| b.is_ascii_whitespace())
        .filter(|t| !t.is_empty() && t.iter().all(u8::is_ascii_digit))
        .filter_map(|t| parse_u64_ascii(t).and_then(|v| u32::try_from(v).ok()))
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

    // priority, nice, num_threads, itrealvalue
    for _ in 0..4 {
        tokens.next()?;
    }
    let start_ticks = parse_u64_ascii(tokens.next()?)?;
    let _vsize = tokens.next()?;

    let rss_pages = parse_u64_ascii(tokens.next()?)?;
    let rss_bytes = rss_pages.saturating_mul(page_size);

    Some(ProcStat {
        cpu_ticks,
        reaped_child_ticks,
        start_ticks,
        rss_bytes,
        ppid,
        comm_start,
        comm_end,
    })
}

/// Reads utime + stime (in clock ticks), RSS (in bytes), PPID, and process name (`comm`) from `/proc/<pid>/stat`.
pub fn read_process_stat_and_comm(pid: u32, page_size: u64) -> Option<(u64, u64, u32, String)> {
    let mut buf = [0u8; 1024];
    let st = read_process_stat_and_comm_slice(pid, page_size, &mut buf)?;
    let comm = String::from_utf8_lossy(&buf[st.comm_start..st.comm_end]).into_owned();
    Some((st.cpu_ticks, st.rss_bytes, st.ppid, comm))
}

/// Reads the command line for `pid` (first 4 KiB) from `/proc/<pid>/cmdline`, joining
/// arguments with single spaces and dropping control characters. Non-ASCII text is kept
/// (invalid UTF-8 becomes U+FFFD) so non-English paths stay readable.
pub fn read_process_cmdline(pid: u32, fallback_comm: &str) -> String {
    let mut path_buf = [0u8; 64];
    let Some(path) = format_proc_cmdline_path(&mut path_buf, pid) else {
        return fallback_comm.to_string();
    };
    let mut buf = [0u8; 4096];
    let n = match File::open(path).and_then(|f| f.read_at(&mut buf, 0)) {
        Ok(n) => n,
        Err(_) => return fallback_comm.to_string(),
    };
    let text = sanitize_cmdline(&buf[..n]);
    if text.is_empty() {
        fallback_comm.to_string()
    } else {
        text
    }
}

/// Turns raw NUL-separated cmdline bytes into one readable line.
pub(super) fn sanitize_cmdline(raw: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(lossy.len());
    for ch in lossy.chars() {
        let ch = if ch == '\0' || ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if ch.is_control() || (ch == ' ' && (out.is_empty() || out.ends_with(' '))) {
            continue;
        }
        out.push(ch);
    }
    out.truncate(out.trim_end().len());
    out
}

/// Samples the CPU time (in microseconds) and RSS (in bytes) of a process tree rooted at `root_pid`.
pub fn sample_pid_tree(root_pid: u32) -> Option<(u64, u64)> {
    let mut sampler = ProcSampler::new(&[]);
    sampler.sample_tree(root_pid)
}

/// Discovers all descendant PIDs rooted at `root_pid` by scanning `/proc`.
pub fn find_process_tree(root_pid: u32) -> Vec<u32> {
    use std::collections::VecDeque;

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
