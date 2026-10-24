//! Process identity, process trees, CPU burners and duplicate batches (all pure selections).
//!
//! Every signal the hands send is addressed by [`ProcId`] (pid + start time), so a reused pid is
//! refused. Selections only ever return processes that are *ours*.

use super::config::WatchConfig;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Identity of a process: its pid and its start time in clock ticks since boot (`/proc/<pid>/stat` field 22).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub struct ProcId {
    pub pid: u32,
    pub start_ticks: u64,
}

/// One process as observed: parentage, age, CPU seconds (self + reaped children), cwd and cgroup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Proc {
    pub id: ProcId,
    pub ppid: u32,
    pub pgid: u32,
    /// `comm`, the program name.
    pub name: String,
    /// Seconds since it started.
    pub age: i64,
    /// utime+stime+cutime+cstime in seconds.
    pub cpu: f64,
    pub cwd: String,
    /// Started under starve (its environment carries `<TOOL>_STARVE=1`).
    pub starved: bool,
    pub argv: Vec<String>,
    /// The v2 cgroup path (`0::` line), empty when unknown.
    pub cgroup: String,
}

impl Proc {
    pub fn pid(&self) -> u32 {
        self.id.pid
    }
}

/// `root` and every process descended from it (by ppid). Empty when `root` is not in the table.
pub fn below(procs: &[Proc], root: u32) -> Vec<&Proc> {
    let mut kids: HashMap<u32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        kids.entry(p.ppid).or_default().push(p);
    }
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue: Vec<&Proc> = procs.iter().filter(|p| p.pid() == root).collect();
    while let Some(p) = queue.pop() {
        if !seen.insert(p.pid()) {
            continue;
        }
        out.push(p);
        if let Some(k) = kids.get(&p.pid()) {
            queue.extend(k.iter().copied());
        }
    }
    out
}

/// CPU seconds of the whole tree under `root`, reaped children included.
pub fn tree_cpu(procs: &[Proc], root: u32) -> f64 {
    below(procs, root).iter().map(|p| p.cpu).sum()
}

/// `path` is `root` or lies under `root/` (never `root-other`).
pub fn inside(path: &str, root: &str) -> bool {
    if path.is_empty() || root.is_empty() {
        return false;
    }
    let r = root.trim_end_matches('/');
    if r.is_empty() {
        return path.starts_with('/');
    }
    path == r || path.starts_with(&format!("{r}/"))
}

/// Whether a process's working directory `cwd` is owned by the worktree `root`
/// (equals `root` or lies strictly inside `root/`, never `root-other`).
pub fn owns(cwd: &str, root: &str) -> bool {
    inside(cwd, root)
}

/// Parse one `/proc/<pid>/stat` line plus companion `/proc/<pid>/{cwd,environ,cmdline,cgroup}`
/// contents into a [`Proc`].
///
/// Handles process names containing spaces and parentheses by splitting at the last `')'`.
/// CPU seconds include `utime + stime + cutime + cstime` so finished, reaped children count.
#[allow(clippy::too_many_arguments)]
pub fn parse_proc_stat(
    pid: u32,
    stat_raw: &str,
    cwd: &str,
    environ: &[u8],
    cmdline: &[u8],
    cgroup_raw: &str,
    ticks_per_sec: u64,
    uptime_secs: f64,
    c: &WatchConfig,
) -> Option<Proc> {
    let ticks = ticks_per_sec.max(1) as f64;
    let (head, rest) = stat_raw.rsplit_once(')')?;
    let (_, name) = head.split_once('(')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 20 {
        return None;
    }
    let ppid: u32 = fields[1].parse().ok()?;
    let pgid: u32 = fields[2].parse().ok()?;
    let utime: u64 = fields[11].parse().ok()?;
    let stime: u64 = fields[12].parse().ok()?;
    let cutime: i64 = fields[13].parse().ok()?;
    let cstime: i64 = fields[14].parse().ok()?;
    let start_ticks: u64 = fields[19].parse().ok()?;

    let total_ticks = (utime as i64)
        .saturating_add(stime as i64)
        .saturating_add(cutime.max(0))
        .saturating_add(cstime.max(0)) as f64;
    let cpu = total_ticks / ticks;
    let age = (uptime_secs - (start_ticks as f64) / ticks).max(0.0) as i64;

    let marker = format!("{}=1", c.starve_env());
    let starved = environ
        .split(|&b| b == 0)
        .any(|entry| entry == marker.as_bytes() || entry == b"STARVE=1");

    let argv: Vec<String> = cmdline
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();

    let cgroup = cgroup_raw
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .or_else(|| cgroup_raw.trim().rsplit_once("::").map(|(_, cg)| cg))
        .unwrap_or("")
        .trim()
        .to_string();

    Some(Proc {
        id: ProcId { pid, start_ticks },
        ppid,
        pgid,
        name: name.to_string(),
        age,
        cpu,
        cwd: cwd.to_string(),
        starved,
        argv,
        cgroup,
    })
}

/// Whether a `/proc/<pid>/stat` line represents a live, non-zombie (`state != "Z"`) process.
pub fn stat_is_alive(stat_raw: &str) -> bool {
    let Some((_, rest)) = stat_raw.rsplit_once(')') else {
        return false;
    };
    let Some(state) = rest.split_whitespace().next() else {
        return false;
    };
    state != "Z"
}

/// Whether any non-zombie `/proc/<pid>/stat` line belongs to process group `group`.
pub fn group_is_alive(group: u32, stat_lines: &[&str]) -> bool {
    for raw in stat_lines {
        let Some((_, rest)) = raw.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() >= 3 && fields[0] != "Z" && fields[2].parse::<u32>().ok() == Some(group) {
            return true;
        }
    }
    false
}

/// The cgroup is under an agent slice (`<prefix>*.slice`) or a `run-u<N>.scope`.
pub fn in_agent_cgroup(cgroup: &str, c: &WatchConfig) -> bool {
    cgroup.split('/').any(|part| {
        (part.starts_with(&c.agent_slice_prefix) && part.ends_with(".slice"))
            || part
                .strip_prefix(&c.run_scope_prefix)
                .and_then(|r| r.strip_suffix(".scope"))
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// The program a `timeout` invocation runs: the second non-option word after argv[0]
/// (the first is the duration).
pub fn timeout_command(argv: &[String]) -> &str {
    let rest: Vec<&String> = argv
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    rest.get(1).map(|s| basename(s)).unwrap_or("")
}

/// A burner by name, or `timeout` wrapping a burner.
pub fn is_burner(p: &Proc, c: &WatchConfig) -> bool {
    c.is_burner_name(&p.name) || (p.name == "timeout" && c.is_burner_name(timeout_command(&p.argv)))
}

/// Why a burner is ours, or None when it is a stranger's.
pub fn ours_because(
    p: &Proc,
    ours: &BTreeSet<u32>,
    work_root: &str,
    c: &WatchConfig,
) -> Option<&'static str> {
    if ours.contains(&p.pid()) {
        Some("it descends from an agent we started")
    } else if inside(&p.cwd, work_root) {
        Some("it runs inside the work root")
    } else if in_agent_cgroup(&p.cgroup, c) {
        Some("it runs in an agent cgroup")
    } else {
        None
    }
}

fn ours_set(procs: &[Proc], roots: &BTreeSet<u32>) -> BTreeSet<u32> {
    roots
        .iter()
        .flat_map(|r| below(procs, *r))
        .map(Proc::pid)
        .collect()
}

/// Burners past the time box that are ours (descent from a running agent root, cwd in the work
/// root, or an agent cgroup) and not started under starve.
pub fn burners<'a>(
    procs: &'a [Proc],
    agent_roots: &BTreeSet<u32>,
    work_root: &str,
    c: &WatchConfig,
) -> Vec<&'a Proc> {
    let ours = ours_set(procs, agent_roots);
    procs
        .iter()
        .filter(|p| {
            is_burner(p, c)
                && p.age >= c.burner_limit
                && !p.starved
                && ours_because(p, &ours, work_root, c).is_some()
        })
        .collect()
}

/// The earliest time a burner of ours that is still inside its time box will be due, if any.
pub fn next_burner_due(
    procs: &[Proc],
    agent_roots: &BTreeSet<u32>,
    work_root: &str,
    now: i64,
    c: &WatchConfig,
) -> Option<i64> {
    let ours = ours_set(procs, agent_roots);
    procs
        .iter()
        .filter(|p| {
            is_burner(p, c)
                && p.age < c.burner_limit
                && !p.starved
                && ours_because(p, &ours, work_root, c).is_some()
        })
        .map(|p| now + (c.burner_limit - p.age))
        .min()
}

/// The running agent whose root shares the burner's cgroup; else the one it descends from.
/// `running` is `(key, root pid)`.
pub fn owner(burner: &Proc, procs: &[Proc], running: &[(String, u32)]) -> Option<String> {
    if !burner.cgroup.is_empty() {
        for (key, pid) in running {
            if procs
                .iter()
                .any(|p| p.pid() == *pid && p.cgroup == burner.cgroup)
            {
                return Some(key.clone());
            }
        }
    }
    running
        .iter()
        .find(|(_, pid)| below(procs, *pid).iter().any(|p| p.pid() == burner.pid()))
        .map(|(k, _)| k.clone())
}

/// The process with this exact identity leads its own process group, as the ones we start do.
/// False when it is gone, its start time differs (pid reuse), or it is not a group leader.
pub fn leads_group(procs: &[Proc], id: ProcId) -> bool {
    procs.iter().any(|p| p.id == id && p.pgid == id.pid)
}

/// The identity a pid has in the table, if present.
pub fn identity(procs: &[Proc], pid: u32) -> Option<ProcId> {
    procs.iter().find(|p| p.pid() == pid).map(|p| p.id)
}

/// A batch as the duplicate check sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchObs {
    pub id: String,
    pub prs: Vec<u64>,
    pub started: i64,
    pub pid: Option<u32>,
    pub running: bool,
}

impl BatchObs {
    /// Adapter from the verify record (only `running` counts; `bisecting` is the same batch at work).
    pub fn from_run(b: &crate::decide::verify::BatchRun) -> Self {
        Self {
            id: b.id.clone(),
            prs: b.prs.clone(),
            started: b.started_at,
            pid: b.pid,
            running: b.status == "running",
        }
    }
}

/// Among running batches whose pid is alive, group by member-PR set; every one but the oldest
/// (by start, then id) is returned as `(id, pid)`.
pub fn duplicate_batches(batches: &[BatchObs], alive: &BTreeSet<u32>) -> Vec<(String, u32)> {
    let mut groups: BTreeMap<BTreeSet<u64>, Vec<(i64, &str, u32)>> = BTreeMap::new();
    for b in batches {
        let Some(pid) = b.pid else { continue };
        if b.running && alive.contains(&pid) {
            groups
                .entry(b.prs.iter().copied().collect())
                .or_default()
                .push((b.started, b.id.as_str(), pid));
        }
    }
    let mut out = Vec::new();
    for mut g in groups.into_values() {
        g.sort();
        out.extend(
            g.into_iter()
                .skip(1)
                .map(|(_, id, pid)| (id.to_string(), pid)),
        );
    }
    out
}
