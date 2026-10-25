//! Legacy watch types and progress helpers preserved for backward compatibility
//! with `decide/mod.rs`, `decide/tests.rs`, `tacho-cli`, and `tacho-daemon`.

use crate::decide::dispatch::{attempt, pr_problem, recorded_pr, AgentRun};
use crate::decide::verify::BatchRun;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

pub use super::legacy_notify::*;
pub use super::legacy_ops::*;

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

/// Minimum process-tree CPU tick rate (`CLK_TCK = 100` -> 5 ticks/s = `0.05` CPU-s/s)
/// above which a worker's process tree (including child builds/tests) is considered busy.
pub const IDLE_CPU_TICKS_PER_SEC: u64 = 5;

/// Staleness threshold (30 minutes) before lagging self-update commits raise an attention item.
pub const SELF_UPDATE_STALE_SECS: i64 = 30 * 60;

/// A periodic progress sample for a live agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProgressSample {
    pub ts: i64,
    pub log_bytes: u64,
    pub head_sha: String,
    pub dirty_files: u32,
    pub cpu_ticks: u64,
    /// Newest file mtime (epoch seconds) in the agent's worktree; `None` when not observed.
    #[serde(default)]
    pub newest_mtime: Option<i64>,
}

/// Progress verdict for a live agent (legacy uppercase serde format).
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
    Ok,
    Resume { key: String, because: String },
    Nudge { key: String, because: String },
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

/// CPU starvation plan returned by legacy `plan_starve`.
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
///
/// Sub-minute identical samples are coalesced, and repeated quiet heartbeats update the
/// trailing quiet sample in place so the initial baseline timestamp at `samples[0]` is never
/// pushed out of the window during long idle periods.
pub fn take_sample(
    samples: &[ProgressSample],
    sample: ProgressSample,
    max_samples: usize,
) -> Vec<ProgressSample> {
    let cap = max_samples.max(2);
    let mut out: Vec<ProgressSample> = samples.to_vec();
    let same_metrics = |a: &ProgressSample, b: &ProgressSample| {
        a.log_bytes == b.log_bytes
            && a.head_sha == b.head_sha
            && a.dirty_files == b.dirty_files
            && a.cpu_ticks == b.cpu_ticks
            && a.newest_mtime == b.newest_mtime
    };
    if let Some(last) = out.last() {
        if same_metrics(last, &sample) {
            if sample.ts.saturating_sub(last.ts) < 60 {
                return out;
            }
            if out.len() >= 2 && same_metrics(&out[out.len() - 2], last) {
                if let Some(tail) = out.last_mut() {
                    tail.ts = sample.ts;
                }
                return out;
            }
        }
    }
    out.push(sample);
    if out.len() > cap {
        let drop_n = out.len() - cap;
        out.drain(1..=drop_n);
    }
    out
}

/// Compute the progress [`Verdict`] (`MOVING`, `STALLED`, or `HUNG`) from an agent's samples.
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
            || matches!((a.newest_mtime, b.newest_mtime), (Some(x), Some(y)) if y > x)
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

/// Return the current phase (`(phase, since_ts, latest_text)`) for an agent.
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
        if a.pid.is_some_and(|root| below(root, procs).contains(&pid)) {
            return Some(a.key.as_str());
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
