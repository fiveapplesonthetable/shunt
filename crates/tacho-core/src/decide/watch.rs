//! Agent & chain progress verdicts (`MOVING`, `STALLED`, `HUNG`), phase notes,
//! supervision actions, CPU starvation planning, machine janitor (`tidy`),
//! watchdog alerts, cost accounting, and human attention items.

use crate::decide::dispatch::{
    attempt, base_key, pr_problem, recorded_pr, stuck_runs, AgentRun, Hold, PullFacts, TodoItem,
    STALE_HOLD_SECS,
};
use crate::decide::verify::{BatchRun, FailedRecord, PassedRecord};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

/// Seconds of zero work + zero CPU ticks before an agent is judged `HUNG` (30 minutes).
pub const HUNG_SECS: i64 = 1_800;

/// Seconds of zero work + idle CPU before an agent is judged `STALLED` (45 minutes).
pub const STALLED_SECS: i64 = 2_700;

/// Seconds in a single phase before a long-phase warning is raised (2 hours).
pub const LONG_PHASE_SECS: i64 = 7_200;

/// Allowed phase tags for `tacho note <key> "<phase>: <line>"`.
pub const VALID_PHASES: &[&str] = &["read", "write", "test", "fix", "push"];

/// Minimum age in seconds before an unguarded `yes`/`stress`/`stress-ng` process is killed (10 minutes).
pub const BURNER_MIN_AGE_SECS: i64 = 600;

/// Default maximum fraction of system CPUs allocated to `tacho starve` (`1 / 8`).
pub const STARVE_CPU_DIVISOR: usize = 8;

/// Burners spawned per starved CPU core during `tacho starve`.
pub const BURNERS_PER_CPU: usize = 4;

/// A periodic progress sample for a live agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProgressSample {
    pub ts: i64,
    pub log_bytes: u64,
    pub head_sha: String,
    pub dirty_files: u32,
    pub cpu_ticks: u64,
}

/// Progress verdict for a live agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Verdict {
    Moving,
    Stalled,
    Hung,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Moving => "MOVING",
            Verdict::Stalled => "STALLED",
            Verdict::Hung => "HUNG",
        }
    }
}

/// A structured phase note recorded via `tacho note <key> "<phase>: <line>"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseNote {
    pub ts: i64,
    pub phase: String,
    pub text: String,
}

/// Rendered row for a single agent in `tacho progress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProgressRow {
    pub key: String,
    pub area: String,
    pub verdict: String,
    pub phase: String,
    pub phase_age_secs: i64,
    pub phase_long: bool,
    pub note: String,
    pub idle_secs: i64,
}

/// Rendered throughput and latency row for an area chain in `tacho progress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainProgressRow {
    pub area: String,
    pub steps_last_hour: usize,
    pub steps_last_day: usize,
    pub median_run_secs: i64,
    pub median_batch_secs: i64,
    pub owner_wait_secs: i64,
    pub machine_wait_secs: i64,
    pub head_key: Option<String>,
    pub head_pr: Option<u64>,
}

/// Supervision action chosen for a live agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum SuperviseAction {
    /// Agent is making progress; no intervention needed.
    Ok,
    /// Agent is `HUNG` (0 CPU + 0 work for >= 30m): stop and schedule immediate resume.
    Resume { key: String, because: String },
    /// Agent is `STALLED` (no work for >= 45m, first stall): send a nudge prompt.
    Nudge { key: String, because: String },
    /// Agent remained `STALLED` after already being nudged: escalate to human attention/email.
    Escalate { key: String, because: String },
}

/// Snapshot of a host process for supervision and tidy checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub state: String,
    pub cpu_ticks: u64,
    pub elapsed_secs: i64,
    pub comm: String,
    pub args: String,
    pub cwd: String,
    #[serde(default)]
    pub env_starve: bool,
}

/// CPU starvation plan returned by [`plan_starve`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarvePlan {
    pub cpus: Vec<usize>,
    pub cpu_list: String,
    pub burners: usize,
    pub timeout_secs: u64,
}

/// Snapshot of machine artifacts inspected by the pure janitor (`tidy::decide`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TidySnapshot {
    pub now: i64,
    pub worktrees: Vec<WorktreeCandidate>,
    pub checkouts: Vec<CheckoutCandidate>,
    pub containers: Vec<ContainerCandidate>,
    pub networks: Vec<String>,
    pub live_agent_worktrees: BTreeSet<String>,
    pub live_batch_worktrees: BTreeSet<String>,
    pub held_paths: BTreeSet<String>,
    pub held_ports: BTreeSet<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorktreeCandidate {
    pub path: String,
    pub branch: String,
    pub age_secs: i64,
    pub dirty: bool,
    pub unpushed_commits: u32,
    pub pr_merged_or_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckoutCandidate {
    pub path: String,
    pub age_secs: i64,
    pub dirty: bool,
    pub unpushed_commits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContainerCandidate {
    pub id: String,
    pub name: String,
    pub age_secs: i64,
    pub ports: Vec<u16>,
}

/// Pure cleanup decision produced by [`decide_tidy`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TidyDecision {
    RemoveTree {
        path: String,
        rescue: bool,
        because: String,
    },
    RemoveCheckout {
        path: String,
        rescue: bool,
        because: String,
    },
    RemoveContainer {
        id: String,
        because: String,
    },
    RemoveNetwork {
        name: String,
        because: String,
    },
}

/// Token and cost usage for a single agent run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CostRecord {
    pub key: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub usd: f64,
}

/// Aggregated cost summary across runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CostSummary {
    pub total_usd: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub by_provider: BTreeMap<String, f64>,
    pub by_area: BTreeMap<String, f64>,
    pub runs: usize,
}

/// Human-actionable item surfaced in `NEEDS YOU` (`tacho status` / `tacho top`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionItem {
    pub kind: String,
    pub target: String,
    pub summary: String,
    pub command_hint: String,
}

/// Append a new [`ProgressSample`] to `samples`, keeping at most `max_samples` (default 60).
pub fn take_sample(
    samples: &[ProgressSample],
    sample: ProgressSample,
    max_samples: usize,
) -> Vec<ProgressSample> {
    let cap = max_samples.max(2);
    let mut out: Vec<ProgressSample> = samples.to_vec();
    out.push(sample);
    if out.len() > cap {
        let drop_n = out.len() - cap;
        out.drain(0..drop_n);
    }
    out
}

/// Minimum process-tree CPU tick rate (`CLK_TCK = 100` -> 5 ticks/s = `0.05` CPU-s/s)
/// above which a worker's process tree (including child builds/tests) is considered busy.
pub const IDLE_CPU_TICKS_PER_SEC: u64 = 5;

/// Staleness threshold (30 minutes) before lagging self-update commits raise an attention item.
pub const SELF_UPDATE_STALE_SECS: i64 = 30 * 60;

/// Compute the progress [`Verdict`] (`MOVING`, `STALLED`, or `HUNG`) from an agent's samples.
///
/// Rules:
/// - Work occurs whenever `log_bytes` grows by `>= 64` bytes, `head_sha` changes, or `dirty_files` changes.
/// - Process-tree CPU liveness (`last_alive_ts`) advances whenever `cpu_ticks` increases by `>= 1` tick.
/// - Process-tree CPU busy activity (`last_busy_ts`) advances whenever `cpu_ticks` (which includes all
///   child processes in the agent's process tree/cgroup) grows at `>= IDLE_CPU_TICKS_PER_SEC` (`0.05` CPU-s/s).
/// - If no work occurred for `>= HUNG_SECS` (30m) AND `now - last_alive_ts >= HUNG_SECS` (zero CPU ticks), verdict is `HUNG`.
/// - If no work occurred for `>= STALLED_SECS` (45m) AND `now - last_busy_ts >= STALLED_SECS` (process tree CPU idle), verdict is `STALLED`.
/// - Otherwise (including when child processes are actively burning CPU on a long check), verdict is `MOVING`.
pub fn verdict(samples: &[ProgressSample], now: i64) -> (Verdict, i64) {
    let Some(_last) = samples.last() else {
        return (Verdict::Moving, 0);
    };
    let start_ts = samples.first().map(|s| s.ts).unwrap_or(now);
    let mut last_work_ts = start_ts;
    let mut last_alive_ts = start_ts;
    let mut last_busy_ts = start_ts;

    for pair in samples.windows(2) {
        let a = &pair[0];
        let b = &pair[1];
        if b.log_bytes.saturating_sub(a.log_bytes) >= 64
            || b.head_sha != a.head_sha
            || b.dirty_files != a.dirty_files
        {
            last_work_ts = b.ts;
        }
        let cpu_grew = b.cpu_ticks.saturating_sub(a.cpu_ticks);
        if cpu_grew > 0 {
            last_alive_ts = b.ts;
        }
        let elapsed = (b.ts - a.ts).max(1) as u64;
        if cpu_grew >= elapsed.saturating_mul(IDLE_CPU_TICKS_PER_SEC) {
            last_busy_ts = b.ts;
        }
    }

    let idle_secs = (now - last_work_ts).max(0);
    let cpu_silent_secs = (now - last_alive_ts).max(0);
    let cpu_idle_secs = (now - last_busy_ts).max(0);

    if idle_secs >= HUNG_SECS && cpu_silent_secs >= HUNG_SECS {
        return (Verdict::Hung, idle_secs);
    }
    if idle_secs >= STALLED_SECS && cpu_idle_secs >= STALLED_SECS {
        return (Verdict::Stalled, idle_secs);
    }
    (Verdict::Moving, idle_secs.min(cpu_idle_secs))
}

/// Parse a `"<phase>: <summary>"` note string, validating that `<phase>` is one of [`VALID_PHASES`].
pub fn parse_note(raw: &str, now: i64) -> Result<PhaseNote, String> {
    let Some((phase_raw, rest)) = raw.split_once(':') else {
        return Err(format!(
            "note must start with '<phase>: <summary>' where <phase> is one of {VALID_PHASES:?}"
        ));
    };
    let phase = phase_raw.trim().to_ascii_lowercase();
    if !VALID_PHASES.contains(&phase.as_str()) {
        return Err(format!(
            "invalid phase '{phase}' (expected one of {VALID_PHASES:?})"
        ));
    }
    let text = rest.trim();
    if text.is_empty() {
        return Err("note summary after '<phase>:' cannot be empty".to_string());
    }
    Ok(PhaseNote {
        ts: now,
        phase,
        text: text.to_string(),
    })
}

/// Append a validated [`PhaseNote`] to an agent's note history.
pub fn with_note(notes: &[PhaseNote], note: PhaseNote) -> Vec<PhaseNote> {
    let mut out = notes.to_vec();
    out.push(note);
    out
}

/// Return the current phase (`(phase, since_ts, latest_text)`) for an agent, tracking when the current consecutive phase started.
pub fn current_phase(notes: &[PhaseNote], started_at: i64) -> (String, i64, String) {
    let Some(last) = notes.last() else {
        return ("start".to_string(), started_at, String::new());
    };
    let mut since = last.ts;
    for n in notes.iter().rev() {
        if n.phase == last.phase {
            since = n.ts;
        } else {
            break;
        }
    }
    (last.phase.clone(), since, last.text.clone())
}

/// Check whether an agent has spent `>= LONG_PHASE_SECS` (2h) in the same phase.
pub fn phase_is_long(notes: &[PhaseNote], started_at: i64, now: i64) -> bool {
    let (_, since, _) = current_phase(notes, started_at);
    (now - since) >= LONG_PHASE_SECS
}

/// Build an [`AgentProgressRow`] for a live agent.
pub fn agent_row(
    agent: &AgentRun,
    samples: &[ProgressSample],
    notes: &[PhaseNote],
    now: i64,
) -> AgentProgressRow {
    let (v, idle_secs) = verdict(samples, now);
    let (phase, since, text) = current_phase(notes, agent.started_at);
    let phase_age_secs = (now - since).max(0);
    let note = if !text.is_empty() {
        text
    } else {
        agent.status_note.clone()
    };
    AgentProgressRow {
        key: agent.key.clone(),
        area: agent.area.clone(),
        verdict: v.as_str().to_string(),
        phase,
        phase_age_secs,
        phase_long: phase_age_secs >= LONG_PHASE_SECS,
        note,
        idle_secs,
    }
}

fn median_i64(mut vals: Vec<i64>) -> i64 {
    if vals.is_empty() {
        return 0;
    }
    vals.sort_unstable();
    vals[vals.len() / 2]
}

/// Compute per-area chain velocity and wait-time breakdown (`tacho progress`).
pub fn chain_rows(
    agents: &BTreeMap<String, AgentRun>,
    batches: &[BatchRun],
    retired: &BTreeSet<String>,
    now: i64,
) -> Vec<ChainProgressRow> {
    let mut areas: BTreeSet<String> = BTreeSet::new();
    for a in agents.values() {
        if !retired.contains(&a.key) && !a.area.is_empty() {
            areas.insert(a.area.clone());
        }
    }

    let mut out = Vec::new();
    for area in areas {
        let mut runs: Vec<&AgentRun> = agents
            .values()
            .filter(|a| !retired.contains(&a.key) && a.area == area)
            .collect();
        runs.sort_by_key(|a| (a.started_at, attempt(&a.key)));

        let hour_ago = now - 3_600;
        let day_ago = now - 86_400;
        let steps_last_hour = runs
            .iter()
            .filter(|a| a.finished_at.unwrap_or(0) >= hour_ago && recorded_pr(a).is_some())
            .count();
        let steps_last_day = runs
            .iter()
            .filter(|a| a.finished_at.unwrap_or(0) >= day_ago && recorded_pr(a).is_some())
            .count();

        let run_durations: Vec<i64> = runs
            .iter()
            .filter_map(|a| a.finished_at.map(|f| (f - a.started_at).max(0)))
            .collect();
        let batch_durations: Vec<i64> = batches
            .iter()
            .filter(|b| b.areas.contains(&area))
            .filter_map(|b| b.finished_at.map(|f| (f - b.started_at).max(0)))
            .collect();

        let last = runs.last().copied();
        let (owner_wait, machine_wait) = match last {
            Some(l) if l.finished_at.is_some() => {
                let since_done = (now - l.finished_at.unwrap_or(now)).max(0);
                if pr_problem(l, None).is_some() || (l.pr.is_none() && !l.pr_none) {
                    (since_done, 0)
                } else {
                    (0, since_done)
                }
            }
            _ => (0, 0),
        };

        out.push(ChainProgressRow {
            area,
            steps_last_hour,
            steps_last_day,
            median_run_secs: median_i64(run_durations),
            median_batch_secs: median_i64(batch_durations),
            owner_wait_secs: owner_wait,
            machine_wait_secs: machine_wait,
            head_key: last.map(|l| l.key.clone()),
            head_pr: last.and_then(recorded_pr),
        });
    }
    out
}

/// Decide the supervision action for a live agent based on its progress [`Verdict`] and whether it was already nudged.
pub fn supervise_step(
    key: &str,
    verdict: Verdict,
    idle_secs: i64,
    already_nudged: bool,
) -> SuperviseAction {
    match verdict {
        Verdict::Moving => SuperviseAction::Ok,
        Verdict::Hung => SuperviseAction::Resume {
            key: key.to_string(),
            because: format!("agent {key} HUNG (0 CPU + no work for {idle_secs}s >= {HUNG_SECS}s)"),
        },
        Verdict::Stalled => {
            if !already_nudged {
                SuperviseAction::Nudge {
                    key: key.to_string(),
                    because: format!(
                        "agent {key} STALLED (no work for {idle_secs}s >= {STALLED_SECS}s)"
                    ),
                }
            } else {
                SuperviseAction::Escalate {
                    key: key.to_string(),
                    because: format!("agent {key} still STALLED ({idle_secs}s idle) after nudge"),
                }
            }
        }
    }
}

/// Return all descendant PIDs rooted at `root_pid` (including `root_pid`).
pub fn below(root_pid: u32, procs: &[ProcEntry]) -> BTreeSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p.pid);
    }
    let mut visited = BTreeSet::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if visited.insert(pid) {
            if let Some(kids) = children.get(&pid) {
                for &k in kids {
                    stack.push(k);
                }
            }
        }
    }
    visited
}

/// Sum CPU ticks across a process tree rooted at `root_pid`.
pub fn tree_cpu(root_pid: u32, procs: &[ProcEntry]) -> u64 {
    let pids = below(root_pid, procs);
    procs
        .iter()
        .filter(|p| pids.contains(&p.pid))
        .map(|p| p.cpu_ticks)
        .sum()
}

/// Identify which agent owns `pid` (either by process tree ancestry or worktree `cwd`).
pub fn owner<'a>(
    pid: u32,
    procs: &[ProcEntry],
    agents: &'a BTreeMap<String, AgentRun>,
) -> Option<&'a str> {
    for a in agents.values() {
        if let Some(root) = a.pid {
            if below(root, procs).contains(&pid) {
                return Some(a.key.as_str());
            }
        }
    }
    if let Some(proc_entry) = procs.iter().find(|p| p.pid == pid) {
        if !proc_entry.cwd.is_empty() {
            let p_cwd = Path::new(&proc_entry.cwd);
            for a in agents.values() {
                if !a.worktree.is_empty() && p_cwd.starts_with(Path::new(&a.worktree)) {
                    return Some(a.key.as_str());
                }
            }
        }
    }
    None
}

fn basename(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

/// Extract a CPU burner binary name (`yes`, `stress`, `stress-ng`) from `comm` or `args`,
/// looking through `timeout` wrappers (e.g. `timeout -k 5 30 yes`).
pub fn burner_command(comm: &str, args: &str) -> Option<String> {
    let base_comm = basename(comm);
    if matches!(base_comm, "yes" | "stress" | "stress-ng") {
        return Some(base_comm.to_string());
    }
    let parts: Vec<&str> = args.split_whitespace().collect();
    let first = parts.first().copied().map(basename).unwrap_or(base_comm);
    if matches!(first, "yes" | "stress" | "stress-ng") {
        return Some(first.to_string());
    }
    if first != "timeout" {
        return None;
    }
    let mut i = 1;
    while i < parts.len() {
        let arg = parts[i];
        if matches!(arg, "-k" | "-s" | "--kill-after" | "--signal") {
            i += 2;
        } else if arg.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let cmd = basename(parts.get(i + 1).copied()?);
    if matches!(cmd, "yes" | "stress" | "stress-ng") {
        Some(cmd.to_string())
    } else {
        None
    }
}

/// Find runaway CPU burner processes (`yes`, `stress`, `stress-ng`, including under `timeout`)
/// running `>= BURNER_MIN_AGE_SECS` that were NOT launched by `tacho starve` (`env_starve == false`).
pub fn burners(procs: &[ProcEntry]) -> Vec<ProcEntry> {
    procs
        .iter()
        .filter(|p| {
            !p.env_starve
                && p.elapsed_secs >= BURNER_MIN_AGE_SECS
                && burner_command(&p.comm, &p.args).is_some()
        })
        .cloned()
        .collect()
}

/// Find duplicate running batches covering the same `group` or identical PR set so all but the oldest can be terminated.
pub fn duplicate_batches(batches: &[BatchRun]) -> Vec<String> {
    let mut by_key: BTreeMap<String, Vec<&BatchRun>> = BTreeMap::new();
    for b in batches {
        if b.status == "running" || b.status == "bisecting" {
            let mut sorted_prs = b.prs.clone();
            sorted_prs.sort_unstable();
            let key = if !b.group.is_empty() {
                format!("g:{}:prs:{:?}", b.group, sorted_prs)
            } else {
                format!("prs:{:?}", sorted_prs)
            };
            by_key.entry(key).or_default().push(b);
        }
    }
    let mut kill_ids = Vec::new();
    for (_, mut list) in by_key {
        if list.len() > 1 {
            list.sort_by_key(|b| (b.started_at, b.id.as_str()));
            for b in &list[1..] {
                kill_ids.push(b.id.clone());
            }
        }
    }
    kill_ids
}

/// Identify runs blocked only because they exited 0 without stating `PR: none` when their worktree
/// actually has 0 commits ahead and 0 dirty files (`idle_chains`), so supervision can auto-mark `pr_none = true`.
pub fn idle_chains(
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    live_keys: &BTreeSet<String>,
    worktree_clean_zero_commits: &HashMap<String, bool>,
) -> Vec<String> {
    let mut out = Vec::new();
    for a in agents.values() {
        if retired.contains(&a.key) || live_keys.contains(&a.key) {
            continue;
        }
        if a.finished_at.is_some()
            && a.exit_code == Some(0)
            && a.pr.is_none()
            && !a.pr_none
            && worktree_clean_zero_commits.get(&a.key) == Some(&true)
        {
            out.push(a.key.clone());
        }
    }
    out
}

/// Format a sorted slice of CPU indices into compact `taskset -c` range notation
/// (e.g. `&[0, 1, 2, 3, 8]` -> `"0-3,8"`).
pub fn format_cpu_set(cpus: &[usize]) -> String {
    if cpus.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<usize> = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut groups = Vec::new();
    let mut start = sorted[0];
    let mut end = sorted[0];
    for &cpu in &sorted[1..] {
        if cpu == end + 1 {
            end = cpu;
        } else {
            if start == end {
                groups.push(format!("{start}"));
            } else {
                groups.push(format!("{start}-{end}"));
            }
            start = cpu;
            end = cpu;
        }
    }
    if start == end {
        groups.push(format!("{start}"));
    } else {
        groups.push(format!("{start}-{end}"));
    }
    groups.join(",")
}

/// Plan a bounded CPU starvation slice for `tacho starve`.
///
/// Rules:
/// - Refuses if any verification batch is currently running (`batch_running == true`).
/// - Allocates at most `max(1, total_cpus / 8)` CPUs by default (highest-numbered CPUs first).
/// - Spawns `BURNERS_PER_CPU` (4) `yes` burners per slice CPU.
pub fn plan_starve(
    total_cpus: usize,
    requested_slice: Option<usize>,
    requested_cpu_set: Option<&str>,
    batch_running: bool,
    timeout_secs: u64,
) -> Result<StarvePlan, String> {
    if batch_running {
        return Err("cannot run `tacho starve` while a verification batch is running".to_string());
    }
    if total_cpus == 0 {
        return Err("total_cpus must be >= 1".to_string());
    }
    let max_slice = (total_cpus / STARVE_CPU_DIVISOR).max(1);

    let cpus: Vec<usize> = if let Some(spec) = requested_cpu_set {
        let mut parsed = BTreeSet::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(format!("empty element in CPU set '{spec}'"));
            }
            if let Some((a, b)) = part.split_once('-') {
                let start: usize = a
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid CPU range '{part}'"))?;
                let end: usize = b
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid CPU range '{part}'"))?;
                if start > end || end >= total_cpus {
                    return Err(format!(
                        "CPU range '{part}' out of bounds for {total_cpus} CPUs"
                    ));
                }
                for c in start..=end {
                    parsed.insert(c);
                }
            } else {
                let c: usize = part
                    .parse()
                    .map_err(|_| format!("invalid CPU id '{part}'"))?;
                if c >= total_cpus {
                    return Err(format!("CPU {c} out of bounds for {total_cpus} CPUs"));
                }
                parsed.insert(c);
            }
        }
        if parsed.is_empty() {
            return Err("empty CPU set".to_string());
        }
        if parsed.len() > max_slice {
            return Err(format!(
                "requested {} CPUs exceeds max starve slice of {} (1/{STARVE_CPU_DIVISOR} of {total_cpus} CPUs)",
                parsed.len(),
                max_slice
            ));
        }
        parsed.into_iter().collect()
    } else {
        let count = requested_slice.unwrap_or(max_slice).clamp(1, max_slice);
        if let Some(req) = requested_slice {
            if req > max_slice {
                return Err(format!(
                    "requested slice {req} exceeds max starve slice of {max_slice} (1/{STARVE_CPU_DIVISOR} of {total_cpus} CPUs)"
                ));
            }
        }
        ((total_cpus - count)..total_cpus).collect()
    };

    let cpu_list = format_cpu_set(&cpus);
    let burners = cpus.len() * BURNERS_PER_CPU;
    Ok(StarvePlan {
        cpus,
        cpu_list,
        burners,
        timeout_secs: timeout_secs.clamp(1, 300),
    })
}

/// Decide which stale worktrees, checkouts, containers, and networks to clean up (`tacho tidy`).
///
/// Safety invariants:
/// - Never touches any path in `live_agent_worktrees`, `live_batch_worktrees`, or `held_paths`.
/// - Never removes a container that binds any port in `held_ports`.
/// - Sets `rescue: true` whenever a worktree or checkout has uncommitted changes (`dirty`)
///   or `unpushed_commits > 0` so work is archived to a rescue ref before removal.
pub fn decide_tidy(snap: &TidySnapshot, min_age_secs: i64) -> Vec<TidyDecision> {
    let mut out = Vec::new();

    for wt in &snap.worktrees {
        if snap.live_agent_worktrees.contains(&wt.path)
            || snap.live_batch_worktrees.contains(&wt.path)
            || snap.held_paths.contains(&wt.path)
        {
            continue;
        }
        if wt.age_secs < min_age_secs && !wt.pr_merged_or_closed {
            continue;
        }
        let rescue = wt.dirty || wt.unpushed_commits > 0;
        let because = if wt.pr_merged_or_closed {
            format!("PR for branch '{}' is merged or closed", wt.branch)
        } else {
            format!("worktree idle for {}s >= {}s", wt.age_secs, min_age_secs)
        };
        out.push(TidyDecision::RemoveTree {
            path: wt.path.clone(),
            rescue,
            because,
        });
    }

    for co in &snap.checkouts {
        if snap.live_batch_worktrees.contains(&co.path) || snap.held_paths.contains(&co.path) {
            continue;
        }
        if co.age_secs < min_age_secs {
            continue;
        }
        let rescue = co.dirty || co.unpushed_commits > 0;
        out.push(TidyDecision::RemoveCheckout {
            path: co.path.clone(),
            rescue,
            because: format!(
                "batch checkout idle for {}s >= {}s",
                co.age_secs, min_age_secs
            ),
        });
    }

    for c in &snap.containers {
        if c.age_secs < min_age_secs {
            continue;
        }
        if c.ports.iter().any(|p| snap.held_ports.contains(p)) {
            continue;
        }
        out.push(TidyDecision::RemoveContainer {
            id: c.id.clone(),
            because: format!(
                "container '{}' age {}s >= {}s with no held ports",
                c.name, c.age_secs, min_age_secs
            ),
        });
    }

    for net in &snap.networks {
        out.push(TidyDecision::RemoveNetwork {
            name: net.clone(),
            because: "unused ephemeral network".to_string(),
        });
    }

    out
}

/// Find finished runs whose PID is no longer alive but whose `finished_at` was never stamped.
pub fn stale_runs(agents: &BTreeMap<String, AgentRun>, live_pids: &BTreeSet<u32>) -> Vec<String> {
    let mut out = Vec::new();
    for a in agents.values() {
        if a.finished_at.is_none() {
            match a.pid {
                Some(pid) if !live_pids.contains(&pid) => out.push(a.key.clone()),
                None => out.push(a.key.clone()),
                _ => {}
            }
        }
    }
    out
}

/// Format a summary of zombie (`Z`) processes grouped by parent PID.
pub fn zombie_lines(procs: &[ProcEntry]) -> Vec<String> {
    let mut by_ppid: BTreeMap<u32, usize> = BTreeMap::new();
    for p in procs {
        if p.state.starts_with('Z') {
            *by_ppid.entry(p.ppid).or_default() += 1;
        }
    }
    by_ppid
        .into_iter()
        .filter(|&(_, count)| count >= 5)
        .map(|(ppid, count)| format!("parent PID {ppid} has {count} zombie children"))
        .collect()
}

/// Check whether the orchestrator tick loop appears down (`last_tick_ts` older than `max_stale_secs`
/// while work is active or queued).
pub fn down_reason(
    last_tick_ts: Option<i64>,
    now: i64,
    max_stale_secs: i64,
    has_active_work: bool,
) -> Option<String> {
    if !has_active_work {
        return None;
    }
    let Some(ts) = last_tick_ts else {
        return Some("orchestrator has never recorded a tick while work is active".to_string());
    };
    let age = (now - ts).max(0);
    if age > max_stale_secs {
        Some(format!(
            "orchestrator tick is stale by {age}s (threshold {max_stale_secs}s)"
        ))
    } else {
        None
    }
}

/// Decide whether the watchdog should send an alert (rate-limited by `cooldown_secs`).
pub fn down_alert(
    reason: Option<&str>,
    last_alert_ts: Option<i64>,
    now: i64,
    cooldown_secs: i64,
) -> Option<String> {
    let r = reason?;
    if let Some(prev) = last_alert_ts {
        if (now - prev) < cooldown_secs {
            return None;
        }
    }
    Some(r.to_string())
}

/// Estimate USD cost for token usage of a model (per 1M tokens).
/// Unknown models return `0.0` rather than guessing.
pub fn price(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> f64 {
    let m = model.to_ascii_lowercase();
    let (in_rate, out_rate) = if m.contains("opus") {
        (15.0, 75.0)
    } else if m.contains("sonnet") || m.contains("gemini-3.1-pro") || m.contains("gemini-pro") {
        (3.0, 15.0)
    } else if m.contains("haiku") || m.contains("mini") || m.contains("flash") {
        (0.8, 4.0)
    } else if m.contains("gpt-5") || m.contains("codex") || m.contains("o1") || m.contains("o3") {
        (2.5, 10.0)
    } else {
        return 0.0;
    };
    let cache_read_rate = in_rate * 0.1;
    let cache_write_rate = in_rate * 2.0;
    (input_tokens as f64 * in_rate
        + output_tokens as f64 * out_rate
        + cache_read_tokens as f64 * cache_read_rate
        + cache_write_tokens as f64 * cache_write_rate)
        / 1_000_000.0
}

/// Summarize token and USD costs across a set of [`CostRecord`]s.
pub fn summarize_costs(records: &[CostRecord], agents: &BTreeMap<String, AgentRun>) -> CostSummary {
    let mut out = CostSummary::default();
    let mut seen_keys = HashSet::new();
    for r in records {
        if !seen_keys.insert(r.key.clone()) {
            continue;
        }
        out.runs += 1;
        out.total_usd += r.usd;
        out.total_input_tokens += r.input_tokens;
        out.total_output_tokens += r.output_tokens;
        *out.by_provider.entry(r.provider.clone()).or_default() += r.usd;
        let area = agents
            .get(&r.key)
            .or_else(|| agents.get(base_key(&r.key)))
            .map(|a| {
                if a.area.is_empty() {
                    "unscoped".to_string()
                } else {
                    a.area.clone()
                }
            })
            .unwrap_or_else(|| "unscoped".to_string());
        *out.by_area.entry(area).or_default() += r.usd;
    }
    out
}

/// Compute the list of human-actionable `NEEDS YOU` items (`tacho status` / `tacho top`).
#[allow(clippy::too_many_arguments)]
pub fn needs_you(
    agents: &BTreeMap<String, AgentRun>,
    todo: &[TodoItem],
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    open_prs: &[PullFacts],
    passed: &BTreeMap<u64, PassedRecord>,
    failed: &BTreeMap<u64, FailedRecord>,
    now: i64,
) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    let _ = passed;

    // 1. Untrusted or contradictory PR attribution on finished runs
    for a in agents.values() {
        if retired.contains(&a.key) || live_keys.contains(&a.key) {
            continue;
        }
        if let Some(prob) = pr_problem(a, None) {
            items.push(AttentionItem {
                kind: "attach".to_string(),
                target: a.key.clone(),
                summary: prob,
                command_hint: format!("tacho attach {} <pr>|none", a.key),
            });
        }
    }

    // 2. Stuck runs that exhausted retries
    for stuck in stuck_runs(agents, todo, retired, holds, live_keys) {
        items.push(AttentionItem {
            kind: "stuck".to_string(),
            target: stuck.key.clone(),
            summary: format!(
                "{} stopped after {} attempt(s) without a PR",
                stuck.key,
                attempt(&stuck.key) + 1
            ),
            command_hint: format!(
                "tacho session {} --resume  # or: tacho retire {}",
                stuck.key,
                base_key(&stuck.key)
            ),
        });
    }

    // 3. Red PRs that remain red after automatic fix attempt
    let open_by_num: HashMap<u64, &PullFacts> = open_prs.iter().map(|p| (p.number, p)).collect();
    for (&pr_num, rec) in failed {
        if holds.contains_key(&format!("pr:{pr_num}")) {
            continue;
        }
        if let Some(pr) = open_by_num.get(&pr_num) {
            if pr.head == rec.sha
                && crate::decide::dispatch::already_ran("fix", pr_num, &rec.sha, agents, live_keys)
            {
                items.push(AttentionItem {
                    kind: "red-pr".to_string(),
                    target: format!("pr:{pr_num}"),
                    summary: format!("PR #{pr_num} still red after auto-fix: {}", rec.summary),
                    command_hint: format!("tacho replay {}", rec.batch_id),
                });
            }
        }
    }

    // 4. Stale holds (>= 24h)
    for (target, reason, age) in crate::decide::dispatch::stale_holds(holds, now, STALE_HOLD_SECS) {
        items.push(AttentionItem {
            kind: "stale-hold".to_string(),
            target: target.clone(),
            summary: format!("held for {}h: {}", age / 3600, reason),
            command_hint: format!("tacho release {target}"),
        });
    }

    items
}

/// A commit on `main` ahead of the currently running orchestrator binary/checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehindCommit {
    pub sha: String,
    pub ts: i64,
    pub ci_green: bool,
}

/// Pure decision for whether the running orchestrator should self-update to a newer green `main` commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfUpdateDecision {
    pub target_sha: Option<String>,
    pub stale: bool,
    pub distance_summary: String,
    pub attention: Option<AttentionItem>,
    pub because: String,
}

/// Observed CI / deploy run on `main`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployRunObs {
    pub run_id: u64,
    pub head_sha: String,
    pub status: String,
    pub gate_conclusion: Option<String>,
    pub on_main: bool,
}

/// Tell-once owner notification item (email / attention line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerNotification {
    pub key: String,
    pub kind: String,
    pub subject: String,
    pub body: String,
    pub command_hint: String,
    pub because: String,
}

/// Decide whether the running orchestrator should advance to a newer `main` commit.
///
/// Rules (`SHUNT.md:26`):
/// - Only moves when `detached` is true and a candidate commit in `behind` (newest first, up to 5) has `ci_green == true`.
/// - Never moves to `refused_sha` (a commit whose self-test previously failed).
/// - Raises an attention item when behind `main` for `> SELF_UPDATE_STALE_SECS` (30m).
pub fn decide_self_update(
    detached: bool,
    behind: &[BehindCommit],
    refused_sha: Option<&str>,
    now: i64,
) -> SelfUpdateDecision {
    if behind.is_empty() {
        return SelfUpdateDecision {
            target_sha: None,
            stale: false,
            distance_summary: "up to date with main".to_string(),
            attention: None,
            because: "up to date with main".to_string(),
        };
    }

    let oldest_ts = behind.iter().map(|c| c.ts).min().unwrap_or(now);
    let oldest_min = ((now - oldest_ts).max(0)) / 60;
    let plural = if behind.len() == 1 { "" } else { "s" };
    let distance_summary = format!(
        "behind main by {} commit{plural}, the oldest {oldest_min} min old",
        behind.len()
    );
    let stale = (now - oldest_ts) > SELF_UPDATE_STALE_SECS;
    let attention = if stale {
        Some(AttentionItem {
            kind: "self-update".to_string(),
            target: "tacho".to_string(),
            summary: format!(
                "orchestrator code is {distance_summary} (over {} min)",
                SELF_UPDATE_STALE_SECS / 60
            ),
            command_hint: "tacho tick".to_string(),
        })
    } else {
        None
    };

    if !detached {
        return SelfUpdateDecision {
            target_sha: None,
            stale,
            distance_summary,
            attention,
            because: "checkout is not detached; skipping self-update".to_string(),
        };
    }

    let candidate = behind
        .iter()
        .take(5)
        .find(|c| c.ci_green)
        .map(|c| c.sha.clone());

    match candidate {
        Some(ref sha) if refused_sha == Some(sha.as_str()) => SelfUpdateDecision {
            target_sha: None,
            stale,
            distance_summary,
            attention,
            because: format!(
                "refusing self-update to {}: self-test previously failed on that commit",
                &sha[..sha.len().min(8)]
            ),
        },
        Some(sha) => {
            let short = &sha[..sha.len().min(8)];
            SelfUpdateDecision {
                because: format!("moving orchestrator to green main commit {short}"),
                target_sha: Some(sha),
                stale,
                distance_summary,
                attention,
            }
        }
        None => SelfUpdateDecision {
            target_sha: None,
            stale,
            distance_summary,
            attention,
            because: "waiting for main CI to turn green before self-updating".to_string(),
        },
    }
}

/// Compute new tell-once owner notifications (`SHUNT.md:25`: "The owner is told once.
/// attention.py gives one line per item with the command that fixes it, said once rather than every tick.").
#[allow(clippy::too_many_arguments)]
pub fn decide_owner_notifications(
    attention_items: &[AttentionItem],
    batches: &[BatchRun],
    ready_trains: &[(String, Vec<u64>, BTreeMap<u64, String>)],
    failed_main_jobs: &BTreeMap<String, String>,
    deploy_runs: &[DeployRunObs],
    already_notified: &BTreeSet<String>,
) -> Vec<OwnerNotification> {
    let mut out = Vec::new();

    // 1. Attention / NEEDS YOU items (one line per item with the command that fixes it, said once)
    for item in attention_items {
        let key = format!("attention:{}:{}:{}", item.kind, item.target, item.summary);
        if already_notified.contains(&key) {
            continue;
        }
        out.push(OwnerNotification {
            key,
            kind: item.kind.clone(),
            subject: format!("[tacho] {} needs attention ({})", item.target, item.kind),
            body: format!("{}\nFix: {}", item.summary, item.command_hint),
            command_hint: item.command_hint.clone(),
            because: format!(
                "{} needs attention: {} (run `{}`)",
                item.target, item.summary, item.command_hint
            ),
        });
    }

    // 2. Ready merge trains (once per train + member head set)
    for (train_id, order, heads) in ready_trains {
        if train_id.is_empty() || order.is_empty() {
            continue;
        }
        let mut head_parts = Vec::new();
        for pr in order {
            let sha = heads.get(pr).map(String::as_str).unwrap_or("");
            head_parts.push(format!("{pr}:{}", &sha[..sha.len().min(8)]));
        }
        let key = format!("ready:{train_id}:{}", head_parts.join(","));
        if already_notified.contains(&key) {
            continue;
        }
        let pr_list = order
            .iter()
            .map(|n| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let cmd = format!("tacho land --train {train_id}");
        out.push(OwnerNotification {
            key,
            kind: "ready-train".to_string(),
            subject: format!("[tacho] train {train_id} ready to merge ({pr_list})"),
            body: format!("Verified train {train_id} ({pr_list}) is ready.\nMerge: {cmd}"),
            command_hint: cmd.clone(),
            because: format!("train {train_id} ({pr_list}) is ready to merge (run `{cmd}`)"),
        });
    }

    // 3. Red batches (reported once when finished red)
    for b in batches {
        if b.status != "failed" && b.status != "red" && b.status != "red-once" {
            continue;
        }
        let key = format!("batch-red:{}", b.id);
        if already_notified.contains(&key) {
            continue;
        }
        let cmd = format!("tacho replay {}", b.id);
        let culprit_str = b
            .culprit
            .map(|c| format!(" (blamed PR #{c})"))
            .unwrap_or_default();
        out.push(OwnerNotification {
            key,
            kind: "red-batch".to_string(),
            subject: format!("[tacho] batch {} failed{}", b.id, culprit_str),
            body: format!(
                "Batch {} on PRs {:?} ended red{}.\nInspect: {}",
                b.id, b.prs, culprit_str, cmd
            ),
            command_hint: cmd.clone(),
            because: format!("batch {} ended red{} (run `{cmd}`)", b.id, culprit_str),
        });
    }

    // 4. Failed main CI jobs (reported once per job failure key)
    for (job_key, line) in failed_main_jobs {
        let key = format!("main-job:{job_key}");
        if already_notified.contains(&key) {
            continue;
        }
        let cmd = "tacho status".to_string();
        out.push(OwnerNotification {
            key,
            kind: "main-red".to_string(),
            subject: format!("[tacho] main job failed: {job_key}"),
            body: format!("Main CI job {job_key} failed: {line}\nCheck: {cmd}"),
            command_hint: cmd,
            because: format!("main CI job {job_key} failed: {line}"),
        });
    }

    // 5. Deploys started on main (reported once per deploy run_id)
    for d in deploy_runs {
        if !d.on_main || !matches!(d.status.as_str(), "in_progress" | "queued" | "waiting") {
            continue;
        }
        if matches!(
            d.gate_conclusion.as_deref(),
            Some("skipped" | "failure" | "cancelled")
        ) {
            continue;
        }
        let key = format!("deploy:{}", d.run_id);
        if already_notified.contains(&key) {
            continue;
        }
        let short = &d.head_sha[..d.head_sha.len().min(8)];
        let cmd = "tacho top".to_string();
        out.push(OwnerNotification {
            key,
            kind: "deploy-started".to_string(),
            subject: format!("[tacho] deploy of {short} started (run #{})", d.run_id),
            body: format!("Deploy of main commit {short} started (run #{})", d.run_id),
            command_hint: cmd,
            because: format!("deploy of {short} started (run #{})", d.run_id),
        });
    }

    out
}
