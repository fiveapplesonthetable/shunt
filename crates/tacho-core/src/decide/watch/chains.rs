//! Agent progress rows, per-area chain velocity/wait metrics, health line, and table rendering.
//!
//! Requirements R31–R38.

use super::activity::Activity;
use super::config::WatchConfig;
use super::notes::{current_phase, phase_is_long, NoteAt};
use super::sample::Sample;
use super::verdict::verdict;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Lifecycle status of an agent run.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    #[default]
    Running,
    Done,
    Unfinished,
    Abandoned,
}

impl RunStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Done => "done",
            RunStatus::Unfinished => "unfinished",
            RunStatus::Abandoned => "abandoned",
        }
    }
}

/// How a PR was attributed to an agent run.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum PrHow {
    Guessed,
    #[default]
    Stated,
    Attached,
}

impl PrHow {
    pub const fn rank(&self) -> u8 {
        match self {
            PrHow::Attached => 3,
            PrHow::Stated => 2,
            PrHow::Guessed => 1,
        }
    }
}

/// A PR recorded on an agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    pub number: u64,
    pub branch: String,
    pub how: PrHow,
}

impl PrRef {
    pub fn stated(number: u64, branch: impl Into<String>) -> Self {
        Self {
            number,
            branch: branch.into(),
            how: PrHow::Stated,
        }
    }

    pub fn attached(number: u64, branch: impl Into<String>) -> Self {
        Self {
            number,
            branch: branch.into(),
            how: PrHow::Attached,
        }
    }
}

/// Standing of a PR in the verification and merge pipeline.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum PrStanding {
    #[default]
    Waiting,
    Passed,
    Merged,
    Closed,
}

/// Record of a single agent run in `WatchState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunRecord {
    pub status: RunStatus,
    pub started: i64,
    pub finished: Option<i64>,
    pub pid: Option<u32>,
    pub worktree: String,
    pub base_sha: String,
    pub task: String,
    pub model: String,
    pub pr: Option<PrRef>,
    /// True when the run's PR attribution was already checked (even if `pr` is `None`).
    pub pr_checked: bool,
    pub stated_no_pr: bool,
    pub no_change: bool,
    pub nudge: bool,
    pub stop_reason: String,
    pub summary: String,
}

/// A queued or active task item in the todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChainTodoItem {
    pub key: String,
    pub area: String,
    pub task: String,
    pub model: String,
    pub now: bool,
}

/// A verification batch record as `chain_rows` sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChainBatch {
    pub started: i64,
    pub finished: Option<i64>,
    pub status: String,
}

/// Rendered progress row for one task's latest run (R31–R32).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRow {
    pub key: String,
    pub state: String,
    pub since_commit: Option<i64>,
    pub since_change: Option<i64>,
    pub since_test: Option<i64>,
    pub commits: u32,
    pub files: u32,
    pub cpu_minutes: f64,
    pub phase: String,
    pub phase_for: Option<i64>,
    pub note: String,
    pub phase_long: bool,
}

/// Rendered throughput and wait-breakdown row for an area chain (R33–R35).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChainRow {
    pub area: String,
    pub steps_hour: usize,
    pub steps_day: usize,
    pub run_median: Option<f64>,
    pub batch_median: Option<f64>,
    pub ready_median: Option<f64>,
    pub owner_wait: i64,
    pub machine_wait: i64,
    pub head: Option<String>,
    pub head_for: Option<i64>,
}

/// Strip a `~<N>` attempt suffix from a run key (`"app-data~2"` -> `"app-data"`).
pub fn base_key(key: &str) -> &str {
    if let Some((base, suffix)) = key.rsplit_once('~') {
        if !base.is_empty() && !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) {
            return base;
        }
    }
    key
}

/// Return the 1-based attempt number of `key` (`"app-data"` -> `1`, `"app-data~2"` -> `2`).
pub fn attempt(key: &str) -> u32 {
    if let Some((base, suffix)) = key.rsplit_once('~') {
        if !base.is_empty() {
            if let Ok(n) = suffix.parse::<u32>() {
                if n >= 1 {
                    return n;
                }
            }
        }
    }
    1
}

/// Format the run key for attempt `n` (1-based) of `base`.
pub fn run_key(base: &str, n: u32) -> String {
    if n <= 1 {
        base.to_string()
    } else {
        format!("{base}~{n}")
    }
}

/// Contiguous run series `(run_key(base, 1), run_key(base, 2), ...)` from attempt 1 to the latest.
/// If attempt 1 is missing (a gap at the start of the series), returns an empty Vec.
pub fn lineage<'a>(
    base: &str,
    agents: &'a BTreeMap<String, RunRecord>,
) -> Vec<(String, &'a RunRecord)> {
    let root = base_key(base);
    let mut out = Vec::new();
    let mut n = 1;
    while let Some(rec) = agents.get(&run_key(root, n)) {
        out.push((run_key(root, n), rec));
        n += 1;
    }
    out
}

/// Newest run in `base`'s contiguous lineage, or `None` if it has not started (or has a gap at 1).
pub fn latest<'a>(
    base: &str,
    agents: &'a BTreeMap<String, RunRecord>,
) -> Option<(String, &'a RunRecord)> {
    lineage(base, agents).pop()
}

/// Task state across its lineage: `"queued"`, `"running"`, `"done"`, `"resuming"`, or `"stuck"`.
pub fn run_state(key: &str, agents: &BTreeMap<String, RunRecord>, c: &WatchConfig) -> &'static str {
    let Some((run_id, agent)) = latest(base_key(key), agents) else {
        return "queued";
    };
    let resume_limit = c.max_runs.saturating_sub(1).max(1);
    if agent.status == RunStatus::Unfinished && attempt(&run_id) <= resume_limit {
        return "resuming";
    }
    match agent.status {
        RunStatus::Running => "running",
        RunStatus::Done => "done",
        RunStatus::Unfinished | RunStatus::Abandoned => "stuck",
    }
}

/// Most trustworthy PR recorded on any run in `key`'s lineage (`Attached > Stated > Guessed`, newest on ties).
pub fn recorded_pr<'a>(key: &str, agents: &'a BTreeMap<String, RunRecord>) -> Option<&'a PrRef> {
    let lin = lineage(base_key(key), agents);
    let mut best: Option<&'a PrRef> = None;
    for (_, rec) in lin.into_iter().rev() {
        if let Some(ref pr) = rec.pr {
            if best.is_none_or(|b| pr.how.rank() > b.how.rank()) {
                best = Some(pr);
            }
        }
    }
    best
}

/// Why the PR recorded for a finished task cannot be built on yet, as `(sentence, needs_person)`.
pub fn pr_problem(
    key: &str,
    agents: &BTreeMap<String, RunRecord>,
    c: &WatchConfig,
) -> Option<(String, bool)> {
    let base = base_key(key);
    let runs: Vec<&RunRecord> = lineage(base, agents).into_iter().map(|(_, r)| r).collect();
    if let Some(pr) = recorded_pr(base, agents) {
        if pr.how == PrHow::Guessed {
            return Some((
                format!(
                    "#{} for {base} was guessed from logs, not stated; check it, then `{} attach {base} {}`",
                    pr.number, c.tool, pr.number
                ),
                true,
            ));
        }
        return None;
    }
    if runs.iter().any(|r| r.stated_no_pr) && !runs.iter().any(|r| r.no_change) {
        return Some((
            format!(
                "{base} stated `PR: none`; a person decides ({} attach {base} <pr>)",
                c.tool
            ),
            true,
        ));
    }
    if runs.iter().any(|r| r.pr_checked || r.no_change) {
        return None;
    }
    Some((format!("looking for the PR of {base}"), false))
}

fn ago(then: Option<i64>, now: i64) -> Option<i64> {
    then.map(|t| (now - t).max(0))
}

/// Build an [`AgentRow`] for a single run (R31).
#[allow(clippy::too_many_arguments)]
pub fn agent_row(
    key: &str,
    status: &str,
    act: &Activity,
    sample: &Sample,
    started: i64,
    notes: &[NoteAt],
    now: i64,
    c: &WatchConfig,
) -> AgentRow {
    let cur = current_phase(notes);
    let state = if status == "running" {
        verdict(act, sample, started, now, c).as_str().to_string()
    } else {
        status.to_string()
    };
    AgentRow {
        key: key.to_string(),
        state,
        since_commit: ago(act.last_commit, now),
        since_change: ago(act.last_change, now),
        since_test: ago(act.last_test, now),
        commits: act.commits,
        files: act.files,
        cpu_minutes: sample.cpu / 60.0,
        phase: cur.as_ref().map(|x| x.phase.clone()).unwrap_or_default(),
        phase_for: cur.as_ref().and_then(|x| ago(Some(x.since), now)),
        note: cur.as_ref().map(|x| x.line.clone()).unwrap_or_default(),
        phase_long: phase_is_long(cur.as_ref(), now, c),
    }
}

/// Build [`AgentRow`]s across all tasks running, unfinished, or finished within `c.row_window` (24 h) (R32).
pub fn rows(
    agents: &BTreeMap<String, RunRecord>,
    samples: &BTreeMap<String, Sample>,
    notes: &BTreeMap<String, Vec<NoteAt>>,
    activities: &BTreeMap<String, Activity>,
    now: i64,
    c: &WatchConfig,
) -> Vec<AgentRow> {
    let mut seen_bases = BTreeSet::new();
    let mut out = Vec::new();
    for k in agents.keys() {
        let base = base_key(k);
        if !seen_bases.insert(base.to_string()) {
            continue;
        }
        let Some((latest_key, agent)) = latest(base, agents) else {
            continue;
        };
        if agent.status != RunStatus::Running && now - agent.finished.unwrap_or(now) > c.row_window
        {
            continue;
        }
        let seen = samples
            .get(&latest_key)
            .copied()
            .unwrap_or_else(|| Sample::first(0.0, now));
        let total_cpu: f64 = lineage(base, agents)
            .iter()
            .map(|(rk, _)| samples.get(rk).map(|s| s.cpu).unwrap_or(0.0))
            .sum();
        let state_name = if run_state(base, agents, c) == "stuck" {
            "stuck"
        } else {
            agent.status.as_str()
        };
        let act = activities
            .get(&agent.worktree)
            .or_else(|| activities.get(&latest_key))
            .or_else(|| activities.get(base))
            .copied()
            .unwrap_or_else(Activity::quiet);
        let task_notes = notes.get(base).map(Vec::as_slice).unwrap_or(&[]);
        let combined_sample = Sample {
            cpu: total_cpu,
            at: seen.at,
            alive_at: seen.alive_at,
            busy_at: seen.busy_at,
        };
        out.push(agent_row(
            &latest_key,
            state_name,
            &act,
            &combined_sample,
            agent.started,
            task_notes,
            now,
            c,
        ));
    }
    out
}

fn median_f64(mut values: Vec<i64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let n = values.len();
    if n % 2 == 1 {
        Some(values[n / 2] as f64)
    } else {
        Some((values[n / 2 - 1] as f64 + values[n / 2] as f64) / 2.0)
    }
}

/// Compute per-area chain velocity, medians, machine/owner wait, and head (R33–R35).
pub fn chain_rows(
    todo: &[ChainTodoItem],
    agents: &BTreeMap<String, RunRecord>,
    batches: &BTreeMap<String, ChainBatch>,
    passed_batch_of_pr: &BTreeMap<u64, String>,
    merged_at: &BTreeMap<u64, i64>,
    standing: &BTreeMap<u64, PrStanding>,
    now: i64,
) -> Vec<ChainRow> {
    let mut areas = Vec::new();
    let mut seen_areas = BTreeSet::new();
    for t in todo {
        if !t.area.is_empty() && seen_areas.insert(t.area.clone()) {
            areas.push(t.area.clone());
        }
    }

    let mut out = Vec::new();
    for area in areas {
        let items: Vec<&str> = todo
            .iter()
            .filter(|t| t.area == area)
            .map(|t| t.key.as_str())
            .collect();
        let mut steps: Vec<(i64, &'static str, i64)> = Vec::new();
        let mut owner_wait: i64 = 0;
        let mut machine_wait: i64 = 0;
        let mut head: Option<String> = None;
        let mut head_for: Option<i64> = None;
        let mut merged_before: Option<i64> = None;

        for key in items {
            let lin = lineage(key, agents);
            let found = lin.last();
            let pr = recorded_pr(key, agents);
            let ran_from = lin.first().map(|(_, r)| r.started);
            let ran_to = found
                .filter(|(_, r)| r.status == RunStatus::Done)
                .and_then(|(_, r)| r.finished);
            if let (Some(from), Some(to)) = (ran_from, ran_to) {
                steps.push((to, "run", to - from));
            }
            let n = pr.map(|p| p.number);
            let batch_rec = n
                .and_then(|num| passed_batch_of_pr.get(&num))
                .and_then(|bid| batches.get(bid));
            if let Some(rec) = batch_rec {
                if let Some(bfin) = rec.finished {
                    steps.push((bfin, "batch", bfin - rec.started));
                    if let Some(rto) = ran_to {
                        machine_wait += (rec.started - rto).max(0);
                    }
                    if let Some(num) = n {
                        if let Some(&m_at) = merged_at.get(&num) {
                            steps.push((m_at, "ready", m_at - bfin));
                            owner_wait += (m_at - bfin).max(0);
                        } else if standing.get(&num) == Some(&PrStanding::Passed) {
                            owner_wait += (now - bfin).max(0);
                        }
                    }
                }
            }
            let complete = found.is_some_and(|(_, r)| r.status == RunStatus::Done)
                && n.is_none_or(|num| standing.get(&num) == Some(&PrStanding::Merged));
            if !complete && head.is_none() {
                head = Some(key.to_string());
                head_for = ago(merged_before.or(ran_from), now);
            }
            if let Some(num) = n {
                merged_before = merged_at.get(&num).copied();
            }
        }

        let by_kind = |kind: &str| -> Option<f64> {
            median_f64(
                steps
                    .iter()
                    .filter(|(_, k, _)| *k == kind)
                    .map(|(_, _, d)| *d)
                    .collect(),
            )
        };
        let steps_hour = steps.iter().filter(|(e, _, _)| now - *e < 3600).count();
        let steps_day = steps.iter().filter(|(e, _, _)| now - *e < 86400).count();
        out.push(ChainRow {
            area,
            steps_hour,
            steps_day,
            run_median: by_kind("run"),
            batch_median: by_kind("batch"),
            ready_median: by_kind("ready"),
            owner_wait,
            machine_wait,
            head,
            head_for,
        });
    }
    out
}

/// Format a duration in seconds as `"-"` (for `None`), `"<m> min"` (< 120 min), or `"<h> h"` (R37).
pub fn format_span(seconds: Option<f64>) -> String {
    let Some(s) = seconds else {
        return "-".to_string();
    };
    let minutes = (s / 60.0).floor() as i64;
    if minutes < 120 {
        format!("{minutes} min")
    } else {
        format!("{} h", minutes.div_euclid(60))
    }
}

fn span_i64(seconds: Option<i64>) -> String {
    format_span(seconds.map(|s| s as f64))
}

/// Single-line health summary of agent verdicts and chain steps (R36).
pub fn health_line(agent_rows: &[AgentRow], chains: &[ChainRow]) -> String {
    let agents = if agent_rows.is_empty() {
        "no agents running".to_string()
    } else {
        agent_rows
            .iter()
            .map(|r| {
                if r.phase.is_empty() {
                    format!("{} {}", r.key, r.state)
                } else {
                    format!("{} {} ({})", r.key, r.state, r.phase)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let stepped = if chains.is_empty() {
        "no chains".to_string()
    } else {
        chains
            .iter()
            .map(|c| {
                format!(
                    "{} {} steps in the last hour, {} today",
                    c.area, c.steps_hour, c.steps_day
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("progress: {agents}; {stepped}")
}

/// Full hourly health line combining CPU/load, free disk, running/stuck counts, and [`health_line`] (R38).
#[allow(clippy::too_many_arguments)]
pub fn hourly_health_line(
    cpu_percent: Option<f64>,
    load: f64,
    free_gb: u64,
    running: usize,
    stuck: usize,
    agent_rows: &[AgentRow],
    chains: &[ChainRow],
    c: &WatchConfig,
) -> String {
    let cpu = match cpu_percent {
        Some(used) => format!("cpu {:.0}% of {:.0}%", used, c.max_cpu_percent),
        None => format!("load {:.0}", load),
    };
    format!(
        "health: {cpu}, {free_gb} GB free, {running} agents running, {stuck} stuck; {}",
        health_line(agent_rows, chains)
    )
}

/// Render the multi-line progress table of agents and chains (R30, R37, parity cases 28–29).
pub fn table(agent_rows: &[AgentRow], chains: &[ChainRow]) -> String {
    let mut lines = vec![format!(
        "{:<28}{:<9}{:>9}{:>9}{:>9}{:>8}{:>6}{:>8}  PHASE",
        "AGENT", "STATE", "COMMIT", "CHANGE", "TEST", "COMMITS", "FILES", "CPU"
    )];
    for r in agent_rows {
        let phase = if !r.phase.is_empty() {
            format!("{} {}: {}", r.phase, span_i64(r.phase_for), r.note)
        } else {
            "no note yet".to_string()
        };
        lines.push(format!(
            "{:<28}{:<9}{:>9}{:>9}{:>9}{:>8}{:>6}{:>7.0}m  {}",
            r.key,
            r.state,
            span_i64(r.since_commit),
            span_i64(r.since_change),
            span_i64(r.since_test),
            r.commits,
            r.files,
            r.cpu_minutes,
            phase
        ));
        if r.phase_long {
            lines.push(format!("{:<28}over 2 h in {}: {}", "", r.phase, r.note));
        }
    }
    lines.push(format!(
        "\n{:<16}{:>5}{:>5}{:>9}{:>9}{:>9}{:>9}{:>9}  HEAD",
        "CHAIN", "HOUR", "DAY", "RUN", "BATCH", "READY", "OWNER", "MACHINE"
    ));
    for c in chains {
        let head = match &c.head {
            Some(h) => format!("{h} for {}", span_i64(c.head_for)),
            None => "none, all merged".to_string(),
        };
        lines.push(format!(
            "{:<16}{:>5}{:>5}{:>9}{:>9}{:>9}{:>9}{:>9}  {}",
            c.area,
            c.steps_hour,
            c.steps_day,
            format_span(c.run_median),
            format_span(c.batch_median),
            format_span(c.ready_median),
            span_i64(Some(c.owner_wait)),
            span_i64(Some(c.machine_wait)),
            head
        ));
    }
    lines.join("\n")
}
