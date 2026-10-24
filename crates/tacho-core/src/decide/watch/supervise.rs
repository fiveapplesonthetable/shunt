//! Pure supervision types and rules (`Step`, `step`, `nudged_before`, `stalled_sentence`,
//! `burner_notice`, `needs_resume`, `resume_brief`, `AreaGate`, `area_gate`, `Idle`, `idle_chains`,
//! `due`, `WatchAction`, `WatchState`, `WatchObs`, `WatchPlan`).
//!
//! Requirements R18–R26, R39–R48. Every [`WatchAction`] carries a human-readable `because` reason.

use super::activity::Activity;
use super::chains::{
    attempt, base_key, lineage, pr_problem, recorded_pr, run_key, run_state, ChainBatch,
    ChainTodoItem, PrStanding, RunRecord, RunStatus,
};
use super::config::WatchConfig;
use super::notes::NoteAt;
use super::procs::{BatchObs, Proc, ProcId};
use super::sample::Sample;
use super::verdict::Verdict;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub use super::tick::{decide_owner_mail, decide_watch, next_deadline, resume_ready_agents};

/// Supervision intervention chosen for a running agent (R18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    None,
    Resume,
    Nudge,
    Email,
}

impl Step {
    /// Alias matching `Step::Email` for callers using the `Escalate` terminology.
    pub const ESCALATE: Self = Self::Email;

    pub const fn as_str(&self) -> &'static str {
        match self {
            Step::None => "none",
            Step::Resume => "resume",
            Step::Nudge => "nudge",
            Step::Email => "email",
        }
    }
}

/// Pure supervision rule (R18):
/// - `HUNG` -> `RESUME`
/// - `STALLED` -> `EMAIL` if `nudged_before`, else `NUDGE`
/// - `MOVING` -> `NONE`
pub fn step(v: Verdict, nudged_before: bool) -> Step {
    match v {
        Verdict::Hung => Step::Resume,
        Verdict::Stalled => {
            if nudged_before {
                Step::Email
            } else {
                Step::Nudge
            }
        }
        Verdict::Moving => Step::None,
    }
}

/// True when any earlier run in `key`'s lineage (excluding the current run) has `nudge == true` (R19).
pub fn nudged_before(key: &str, agents: &BTreeMap<String, RunRecord>) -> bool {
    let lin = lineage(base_key(key), agents);
    let n = lin.len().saturating_sub(1);
    lin.into_iter().take(n).any(|(_, r)| r.nudge)
}

/// Format the `needs_person["stalled-<key>"]` escalation message (R24).
pub fn stalled_sentence(key: &str, worktree: &str, notes: &[NoteAt], c: &WatchConfig) -> String {
    let said = if let Some(last) = notes.last() {
        format!("Its last note: {}: {}", last.phase, last.line)
    } else {
        "It has left no note.".to_string()
    };
    let wt = if worktree.is_empty() {
        "unknown"
    } else {
        worktree
    };
    format!(
        "{key} was nudged and has again written nothing for over 45 minutes while its CPU is idle.\n\
         Worktree: {wt}. {said}\n\
         It needs: someone to look at what it is waiting for, and to stop it with `{} kill` or answer what blocks it.",
        c.tool
    )
}

/// Format the burner notice written into `<wt>/<run_dir>/<notice_file>` (R42).
pub fn burner_notice(count: usize, c: &WatchConfig) -> String {
    format!(
        "{} stopped {count} CPU burner process(es) you left running for over 10 minutes. A starvation test runs \
         under `{} starve -- <command>`, which gives it a CPU slice of its own and cleans up after itself.\n",
        c.tool, c.tool
    )
}

/// Whether an unfinished run is eligible to be resumed at `now` (R22, R23).
pub fn needs_resume(
    agent: &RunRecord,
    agents: &BTreeMap<String, RunRecord>,
    key: &str,
    now: i64,
    c: &WatchConfig,
) -> bool {
    let n = attempt(key);
    let resume_limit = c.max_runs.saturating_sub(1).max(1);
    if agent.status != RunStatus::Unfinished
        || n > resume_limit
        || agents.contains_key(&run_key(base_key(key), n + 1))
    {
        return false;
    }
    agent.nudge || (now - agent.finished.unwrap_or(0) >= c.resume_backoff)
}

/// Build the resume brief for the next attempt of a stopped task (R22, R23).
///
/// - When `nudge` is `true`, leads with the question ("Say where you are and what is blocking...").
/// - When `nudge` is `false` (hung/unfinished), leads with the continue instruction without the question.
pub fn resume_brief(
    task: &str,
    worktree: &str,
    commits: &[String],
    changes: &[String],
    nudge: bool,
    c: &WatchConfig,
) -> String {
    let lead = if nudge {
        format!(
            "The previous run of this task was stopped because nothing in its worktree changed for 45 minutes. \
             Say where you are and what is blocking with `{} note`, then carry on in the same worktree: do not start over.",
            c.tool
        )
    } else {
        format!(
            "A previous run of this task ended without its final report. Continue in the worktree it left, do not start \
             over and do not make a new one. Push what is done, then `{} prove`, `{} pr`, and write the \
             summary. Wait in the foreground, never leave anything running.",
            c.tool, c.tool
        )
    };
    if worktree.is_empty() {
        return format!("{lead}\n\n{task}").trim().to_string();
    }
    let commit_lines: Vec<String> = if commits.is_empty() {
        vec!["  (no commits)".to_string()]
    } else {
        let mut v: Vec<String> = commits.iter().take(10).map(|l| format!("  {l}")).collect();
        if commits.len() > 10 {
            v.push(format!("  and {} more", commits.len() - 10));
        }
        v
    };
    let change_lines: Vec<String> = if changes.is_empty() {
        vec!["  (clean)".to_string()]
    } else {
        let mut v: Vec<String> = changes.iter().take(30).map(|l| format!("  {l}")).collect();
        if changes.len() > 30 {
            v.push(format!("  and {} more", changes.len() - 30));
        }
        v
    };
    let report = format!(
        "Its worktree is {worktree}.\ngit log against its base:\n{}\ngit status:\n{}",
        commit_lines.join("\n"),
        change_lines.join("\n")
    );
    format!("{lead}\n\n{report}\n\n{task}").trim().to_string()
}

/// Area serialization gate result for a queued todo item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AreaGate {
    GoMain { skipped: Option<u64> },
    GoPr { pr: u64, branch: String },
    Blocked { why: String, person: bool },
}

impl AreaGate {
    pub const fn is_go(&self) -> bool {
        matches!(self, AreaGate::GoMain { .. } | AreaGate::GoPr { .. })
    }

    pub fn why(&self) -> String {
        match self {
            AreaGate::Blocked { why, .. } => why.clone(),
            _ => String::new(),
        }
    }
}

/// Evaluate whether `item` may start in its area chain.
pub fn area_gate(
    item: &ChainTodoItem,
    todo: &[ChainTodoItem],
    agents: &BTreeMap<String, RunRecord>,
    standing: &BTreeMap<u64, PrStanding>,
    area_heads: &BTreeMap<String, String>,
    c: &WatchConfig,
) -> AreaGate {
    let idx = todo.iter().position(|t| t.key == item.key).unwrap_or(0);
    let earlier: Vec<&ChainTodoItem> = todo[..idx]
        .iter()
        .filter(|t| t.area == item.area && !t.now)
        .collect();
    let prev = earlier
        .last()
        .map(|t| t.key.as_str())
        .or_else(|| area_heads.get(&item.area).map(String::as_str));
    let Some(prev_key) = prev else {
        return AreaGate::GoMain { skipped: None };
    };
    match run_state(prev_key, agents, c) {
        "queued" => {
            return AreaGate::Blocked {
                why: format!("behind {prev_key}, which has not started"),
                person: false,
            }
        }
        "running" => {
            return AreaGate::Blocked {
                why: format!("waiting for {prev_key} to finish"),
                person: false,
            }
        }
        "resuming" => {
            return AreaGate::Blocked {
                why: format!("waiting for {prev_key} to be resumed"),
                person: false,
            }
        }
        "stuck" => {
            return AreaGate::Blocked {
                why: format!("{prev_key} did not finish; a person decides"),
                person: true,
            }
        }
        _ => {}
    }
    if let Some((why, person)) = pr_problem(prev_key, agents, c) {
        return AreaGate::Blocked { why, person };
    }
    let Some(pr) = recorded_pr(prev_key, agents) else {
        return AreaGate::GoMain { skipped: None };
    };
    match standing
        .get(&pr.number)
        .copied()
        .unwrap_or(PrStanding::Waiting)
    {
        PrStanding::Merged => AreaGate::GoMain { skipped: None },
        PrStanding::Passed => AreaGate::GoPr {
            pr: pr.number,
            branch: pr.branch.clone(),
        },
        PrStanding::Closed => AreaGate::GoMain {
            skipped: Some(pr.number),
        },
        PrStanding::Waiting => AreaGate::Blocked {
            why: format!("waiting on #{} to pass a batch", pr.number),
            person: false,
        },
    }
}

/// An idle chain with nothing running: its head, why it is not starting, and any predecessor whose
/// `PR: none` claim may be a false block (R47).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Idle {
    pub area: String,
    pub key: String,
    pub why: String,
    pub no_pr_of: String,
}

/// Identify chains whose head could be starting or is blocked while the machine has free capacity (R47).
#[allow(clippy::too_many_arguments)]
pub fn idle_chains(
    todo: &[ChainTodoItem],
    agents: &BTreeMap<String, RunRecord>,
    standing: &BTreeMap<u64, PrStanding>,
    holds: &BTreeSet<String>,
    running: usize,
    slots: usize,
    busy: bool,
    area_heads: &BTreeMap<String, String>,
    c: &WatchConfig,
) -> Vec<Idle> {
    if busy || running >= slots {
        return Vec::new();
    }
    let mut areas = Vec::new();
    let mut seen = BTreeSet::new();
    for t in todo {
        if !t.area.is_empty() && seen.insert(t.area.clone()) {
            areas.push(t.area.clone());
        }
    }
    let mut out = Vec::new();
    for area in areas {
        let items: Vec<&ChainTodoItem> = todo.iter().filter(|t| t.area == area).collect();
        if items
            .iter()
            .any(|t| matches!(run_state(&t.key, agents, c), "running" | "resuming"))
        {
            continue;
        }
        let Some((head_idx, head)) = items
            .iter()
            .enumerate()
            .find(|(_, t)| run_state(&t.key, agents, c) == "queued")
        else {
            continue;
        };
        if holds.contains(&head.key) {
            continue;
        }
        let gate = area_gate(head, todo, agents, standing, area_heads, c);
        if gate.is_go() {
            out.push(Idle {
                area,
                key: head.key.clone(),
                why: format!(
                    "{} could start and has not; this is a fault in {}",
                    head.key, c.tool
                ),
                no_pr_of: String::new(),
            });
            continue;
        }
        let why = gate.why();
        let no_pr_of = if why.contains("stated `PR: none`") {
            head_idx
                .checked_sub(1)
                .map(|i| items[i].key.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        out.push(Idle {
            area,
            key: head.key.clone(),
            why,
            no_pr_of,
        });
    }
    out
}

/// Whether the hourly supervision pass is due (`now - last >= c.report_interval`) (R44).
pub fn due(last: i64, now: i64, c: &WatchConfig) -> bool {
    now - last >= c.report_interval
}

/// Every action produced by [`decide_watch`], each carrying a human-readable `because` reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchAction {
    /// Send a provider `tell` nudge to a live session, or fall back to stop+resume.
    Tell {
        key: String,
        text: String,
        because: String,
    },
    /// Stop an agent's process group (verified by `ProcId` and `pgid == pid`) and schedule resume.
    StopAndResume {
        key: String,
        id: ProcId,
        backoff: bool,
        question: bool,
        stop_reason: String,
        tell_line: String,
        because: String,
    },
    /// Escalate a repeatedly-stalled agent to `needs_person["stalled-<key>"]` without killing it.
    Escalate {
        key: String,
        need_key: String,
        need: String,
        because: String,
    },
    /// Clear `needs_person["stalled-<key>"]` when an agent is judged.
    ClearNeed {
        key: String,
        need_key: String,
        because: String,
    },
    /// Terminate a rogue CPU burner process (`SIGTERM`), write a worktree notice, tell, and optionally comment on a PR.
    KillBurner {
        id: ProcId,
        name: String,
        owner: Option<String>,
        worktree_notice: Option<(String, String)>,
        tell_line: String,
        pr_comment: Option<(u64, String)>,
        because: String,
    },
    /// Mark a newer duplicate batch `"superseded"` and stop its process group if it leads one.
    SupersedeBatch {
        batch: String,
        id: ProcId,
        stop_group: bool,
        tell_line: Option<String>,
        because: String,
    },
    /// Lift a false `PR: none` block when the predecessor's known worktree has 0 commits and 0 files.
    LiftPrNone {
        key: String,
        area: String,
        tell_line: String,
        because: String,
    },
    /// Emit an event/report line from the hourly pass (idle chain reason, health line, or guarded error).
    Report { line: String, because: String },
    /// Send a tell-once owner email (stalled escalation, unmailed green train set, or daily digest).
    MailOwner {
        dedup_key: String,
        subject: String,
        body: String,
        because: String,
    },
}

impl WatchAction {
    pub fn because(&self) -> &str {
        match self {
            WatchAction::Tell { because, .. }
            | WatchAction::StopAndResume { because, .. }
            | WatchAction::Escalate { because, .. }
            | WatchAction::ClearNeed { because, .. }
            | WatchAction::KillBurner { because, .. }
            | WatchAction::SupersedeBatch { because, .. }
            | WatchAction::LiftPrNone { because, .. }
            | WatchAction::Report { because, .. }
            | WatchAction::MailOwner { because, .. } => because,
        }
    }
}

/// Persistent state owned or updated by the watch concern.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WatchState {
    pub todo: Vec<ChainTodoItem>,
    pub agents: BTreeMap<String, RunRecord>,
    pub samples: BTreeMap<String, Sample>,
    pub notes: BTreeMap<String, Vec<NoteAt>>,
    pub needs_person: BTreeMap<String, String>,
    pub worktrees: BTreeMap<String, String>,
    pub batches: BTreeMap<String, BatchObs>,
    pub chain_batches: BTreeMap<String, ChainBatch>,
    pub passed_batch_of_pr: BTreeMap<u64, String>,
    pub merged_at: BTreeMap<u64, i64>,
    pub holds: BTreeSet<String>,
    pub area_heads: BTreeMap<String, String>,
    pub supervised: Option<i64>,
    pub told_keys: BTreeSet<String>,
    pub unmailed_green_prs: Vec<(u64, String)>,
    pub digest_sent: Option<i64>,
    pub digest_entries: Vec<(String, String)>,
}

/// External world observation passed into [`decide_watch`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WatchObs {
    pub now: i64,
    pub procs: Vec<Proc>,
    pub live_pids: BTreeSet<u32>,
    /// Map from worktree path (or run key) to observed git/file [`Activity`].
    pub activities: BTreeMap<String, Activity>,
    pub work_root: String,
    pub standing: BTreeMap<u64, PrStanding>,
    pub busy: bool,
    pub cpu_percent: Option<f64>,
    pub load: f64,
    pub free_gb: u64,
    pub run_hourly: bool,
    /// Optional injected fault per hourly step name (`"duplicate batches"`, `"idle chains"`, `"green batches"`, `"health"`).
    pub step_faults: BTreeMap<String, String>,
}

/// Result of a pure [`decide_watch`] evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WatchPlan {
    pub actions: Vec<WatchAction>,
    pub next_state: WatchState,
    pub hourly_lines: Vec<String>,
}

pub(crate) fn resolve_worktree(st: &WatchState, key: &str, agent: &RunRecord) -> String {
    if !agent.worktree.is_empty() {
        return agent.worktree.clone();
    }
    st.worktrees
        .get(key)
        .or_else(|| st.worktrees.get(base_key(key)))
        .cloned()
        .unwrap_or_default()
}
