//! Reaping, worktree resolution, resuming unfinished runs, stopping, abandoning, stuck runs,
//! digest lines, and owner escalations (R7–R8, R63–R74).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::brief::has_sessions;
use super::config::DispatchConfig;
use super::hands::{in_open_train, targets_barred};
use super::model::{
    Action, ActionKind, BlockedRecord, Holds, PullFacts, RefusedRecord, Run, RunKey, RunStatus,
    Runs, Session, StartKind, Stopped, Stuck, WorktreeRec,
};
use super::report::{best_pr, unfinished};
use super::runs::{attempt, base_key, bases, latest, lineage, run_key};
use super::start::{decide_start, StartRequest};

/// Observed liveness of a process (PID-reuse safe via `process_start` field 22 of `/proc/<pid>/stat`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProcObs {
    pub alive: bool,
    pub zombie: bool,
    pub process_start: Option<String>,
}

/// Whether a recorded run's process is still alive (R7).
pub fn is_run_alive(run: &Run, obs: Option<&ProcObs>) -> bool {
    let Some(obs) = obs else { return false };
    if !obs.alive || obs.zombie {
        return false;
    }
    !matches!((&run.process_start, &obs.process_start), (Some(e), Some(a)) if e != a)
}

/// The worktree a run works in: the record whose `agent == base_key(key)` or, with `guess`, the
/// only worktree created under `work_root` while it ran (R8).
pub fn worktree_of(
    key: &str,
    worktrees: &BTreeMap<String, WorktreeRec>,
    made_between: &[String],
    work_root: &str,
    guess: bool,
) -> String {
    let base = base_key(key);
    for known in worktrees.values() {
        if known.agent.as_deref() == Some(base.as_str()) {
            return known.path.clone();
        }
    }
    if !guess {
        return String::new();
    }
    let root = work_root.trim_end_matches('/');
    let matched: Vec<&String> = made_between
        .iter()
        .filter(|p| root.is_empty() || p.rsplit_once('/').map(|(d, _)| d).unwrap_or("") == root)
        .collect();
    if matched.len() == 1 {
        matched[0].clone()
    } else {
        String::new()
    }
}

fn tail_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(max_chars)).collect()
}

/// Reap one running run that has exited, updating its status, summary, finished timestamp and
/// worktree, and emitting a `StopUnit` action if it had a scope unit (R7).
pub fn reap_run(
    key: &str,
    run: &mut Run,
    log: &str,
    worktrees: &BTreeMap<String, WorktreeRec>,
    made_between: &[String],
    now: i64,
    cfg: &DispatchConfig,
) -> Option<Action> {
    run.finished = Some(now);
    run.summary = tail_chars(log, cfg.summary_chars);
    run.status = if unfinished(log, cfg) {
        RunStatus::Unfinished
    } else {
        RunStatus::Done
    };
    if run.worktree.is_empty() {
        run.worktree = worktree_of(key, worktrees, made_between, &cfg.work_root, true);
    }
    if !run.unit.is_empty() {
        Some(Action::new(
            ActionKind::StopUnit {
                key: key.to_string(),
                unit: Some(run.unit.clone()),
                pid: run.pid,
            },
            format!("stop unit {} for finished run {key}", run.unit),
        ))
    } else {
        None
    }
}

/// Reap all `Running` runs that are no longer alive in `procs`, returning `(finished_keys, actions)` (R7).
pub fn reap_runs(
    runs: &mut Runs,
    procs: &BTreeMap<String, ProcObs>,
    logs: &BTreeMap<String, String>,
    worktrees: &BTreeMap<String, WorktreeRec>,
    made_between: &BTreeMap<String, Vec<String>>,
    now: i64,
    cfg: &DispatchConfig,
) -> (Vec<String>, Vec<Action>) {
    let mut finished = Vec::new();
    let mut actions = Vec::new();
    let keys: Vec<String> = runs.keys().cloned().collect();
    for key in keys {
        let alive = is_run_alive(&runs[&key], procs.get(&key));
        if runs[&key].status != RunStatus::Running || alive {
            continue;
        }
        let log = logs.get(&key).map(String::as_str).unwrap_or("");
        let made = made_between.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        if let Some(act) = reap_run(
            &key,
            runs.get_mut(&key).unwrap(),
            log,
            worktrees,
            made,
            now,
            cfg,
        ) {
            actions.push(act);
        }
        finished.push(key);
    }
    (finished, actions)
}

/// An unfinished run is retried up to `resume_limit` times after its backoff; a nudged run resumes
/// immediately (R63).
pub fn needs_resume(key: &str, run: &Run, runs: &Runs, now: i64, cfg: &DispatchConfig) -> bool {
    let rk = RunKey::parse(key);
    if run.status != RunStatus::Unfinished
        || rk.attempt > cfg.resume_limit
        || runs.contains_key(&rk.next().to_string())
    {
        return false;
    }
    run.nudge || (now - run.finished.unwrap_or(0) >= cfg.backoff_for(rk.attempt))
}

fn trunc_list(items: &[String], limit: usize, empty: &str) -> Vec<String> {
    if items.is_empty() {
        return vec![empty.to_string()];
    }
    let mut out: Vec<String> = items.iter().take(limit).cloned().collect();
    if items.len() > limit {
        out.push(format!("and {} more", items.len() - limit));
    }
    out
}

/// The brief for a resumed run: lead (`NUDGE` or `CONTINUE`), where the last one stopped (with
/// commit/file truncation), then the original task (empty when a session carries it) (R65).
pub fn resume_brief(
    task: &str,
    where_stopped: Option<&Stopped>,
    nudge: bool,
    cfg: &DispatchConfig,
) -> String {
    let tool = &cfg.tool;
    let lead = if nudge {
        format!(
            "The previous run of this task was stopped because nothing in its worktree changed \
             for {} minutes. Say where you are and what is blocking with `{tool} note`, then \
             carry on in the same worktree: do not start over.",
            cfg.nudge_quiet_minutes
        )
    } else {
        format!(
            "A previous run of this task ended without its final report. Continue in the worktree \
             it left, do not start over and do not make a new one. Push what is done, then \
             `{tool} prove`, `{tool} pr`, and write the summary. Wait in the foreground, never \
             leave anything running."
        )
    };
    let Some(where_) = where_stopped else {
        return format!("{lead}\n\n{task}").trim().to_string();
    };
    let commits = trunc_list(&where_.commits, cfg.shown_commits, "(no commits)");
    let changes = trunc_list(&where_.changes, cfg.shown_files, "(clean)");
    let mut report_lines = vec![
        format!("Its worktree is {}.", where_.path),
        "git log against its base:".to_string(),
    ];
    for c in commits {
        report_lines.push(format!("  {c}"));
    }
    report_lines.push("git status:".to_string());
    for ch in changes {
        report_lines.push(format!("  {ch}"));
    }
    let report = report_lines.join("\n");
    format!("{lead}\n\n{report}\n\n{task}").trim().to_string()
}

/// Ends the task's running run on a person's say-so and records it `stopped` (never resumed) (R67).
pub fn stop_run(key: &str, runs: &mut Runs, now: i64) -> Result<Action, String> {
    let base = base_key(key);
    let Some((run_id, _)) = latest(&base, runs) else {
        return Err(format!("no run of {key} has started"));
    };
    let run = runs.get_mut(&run_id).unwrap();
    if run.status != RunStatus::Running {
        return Err(format!("{run_id} is not running ({})", run.status.as_str()));
    }
    run.status = RunStatus::Stopped;
    run.finished = Some(now);
    let unit = (!run.unit.is_empty()).then(|| run.unit.clone());
    let pid = run.pid;
    Ok(Action::new(
        ActionKind::StopUnit {
            key: run_id.clone(),
            unit,
            pid,
        },
        format!("stopped {run_id} on user request"),
    ))
}

/// Records a run stopped by the supervisor as `unfinished` so it resumes like a death (R68).
pub fn mark_stopped(key: &str, why: &str, nudge: bool, now: i64, runs: &mut Runs) -> bool {
    let Some(run) = runs.get_mut(key) else {
        return false;
    };
    run.status = RunStatus::Unfinished;
    run.finished = Some(now);
    run.nudge = nudge;
    run.summary = format!("{}\n{why}", run.summary).trim().to_string();
    true
}

/// Ends an unfinished run's chance of resuming (`abandoned`) (R69).
pub fn abandon_run(key: &str, why: &str, runs: &mut Runs) -> Option<Action> {
    let run = runs.get_mut(key)?;
    if run.status != RunStatus::Unfinished {
        return None;
    }
    run.status = RunStatus::Abandoned;
    run.summary = format!("{}\nnot resumed: {why}", run.summary)
        .trim()
        .to_string();
    Some(Action::new(
        ActionKind::Abandon {
            key: key.to_string(),
        },
        format!("abandoned {key}: {why}"),
    ))
}

/// Resume pass across all runs (skipped when `cfg.slots <= 0`) (R64).
#[allow(clippy::too_many_arguments)]
pub fn decide_resume(
    runs: &mut Runs,
    holds: &Holds,
    refused: &mut BTreeMap<String, RefusedRecord>,
    blocked: &mut Option<BlockedRecord>,
    profile: Option<&Session>,
    in_flight: &BTreeSet<u64>,
    barred: &BTreeSet<u64>,
    worktrees: &BTreeMap<String, WorktreeRec>,
    made_between: &BTreeMap<String, Vec<String>>,
    stopped_states: &BTreeMap<String, Stopped>,
    pull_facts: &BTreeMap<u64, Result<PullFacts, String>>,
    alive_pids: &BTreeSet<u32>,
    dry_run: bool,
    env_live: bool,
    now: i64,
    cfg: &DispatchConfig,
) -> (Vec<String>, Vec<Action>) {
    if cfg.slots <= 0 {
        return (Vec::new(), Vec::new());
    }
    let mut out = Vec::new();
    let mut actions = Vec::new();
    let keys: Vec<String> = runs.keys().cloned().collect();
    for key in keys {
        if in_open_train(&key, in_flight, cfg) || targets_barred(&key, barred, cfg) {
            continue;
        }
        let a = runs[&key].clone();
        let pid_alive = a.pid.is_some_and(|p| alive_pids.contains(&p));
        if a.status == RunStatus::Unfinished && attempt(&key) > cfg.resume_limit && !pid_alive {
            if let Some(act) = abandon_run(&key, "no resumptions left", runs) {
                actions.push(act);
            }
            continue;
        }
        let base = base_key(&key);
        if !needs_resume(&key, &a, runs, now, cfg) || holds.contains_key(&base) {
            continue;
        }
        let first_task = lineage(&base, runs)
            .first()
            .map(|(_, r)| r.task.clone())
            .unwrap_or_else(|| a.task.clone());
        let path = if !a.worktree.is_empty() {
            a.worktree.clone()
        } else {
            let made = made_between.get(&key).map(Vec::as_slice).unwrap_or(&[]);
            worktree_of(&key, worktrees, made, &cfg.work_root, true)
        };
        let kept = a
            .provider_selection
            .clone()
            .or_else(|| (!a.provider.is_empty()).then(|| a.provider.clone()))
            .unwrap_or_else(|| "configured".to_string());
        let effective_provider = if a.pinned {
            kept.as_str()
        } else {
            profile
                .map(|p| p.provider.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(kept.as_str())
        };
        let command = match effective_provider {
            "codex" => &cfg.codex_command,
            "claude" => &cfg.claude_command,
            _ => &cfg.command,
        };
        let session = if has_sessions(command, cfg) {
            a.session.clone().unwrap_or_default()
        } else {
            String::new()
        };
        let where_ = stopped_states.get(&path);
        let task = resume_brief(
            if session.is_empty() { &first_task } else { "" },
            where_,
            a.nudge,
            cfg,
        );
        let next_key = run_key(&base, attempt(&key) + 1);
        let facts = a.for_pr.and_then(|pr| pull_facts.get(&pr));
        let req = StartRequest {
            key: next_key.clone(),
            task,
            model: a.model.clone(),
            dry_run,
            area: a.area.clone(),
            worktree: path,
            base: None,
            pr: a.for_pr,
            resume_session: session,
            provider_override: Some(kept),
            pinned: a.pinned,
            kind: StartKind::Resume,
            new_session_id: None,
        };
        let (outcome, start_actions) = decide_start(
            &req, runs, holds, refused, blocked, profile, facts, env_live, now, cfg,
        );
        out.push(outcome.message(&next_key));
        actions.extend(start_actions);
        if !dry_run {
            if let Some(ref_rec) = refused.get(&next_key).cloned() {
                if let Some(act) = abandon_run(&key, &ref_rec.reason, runs) {
                    actions.push(act);
                }
            }
        }
    }
    (out, actions)
}

/// Every task whose latest run was given up on, or ended unfinished with no resumptions left,
/// oldest first (R70).
pub fn stuck_runs(runs: &Runs, now: i64, cfg: &DispatchConfig) -> Vec<Stuck> {
    let mut out = Vec::new();
    for base in bases(runs) {
        let Some((key, agent)) = latest(&base, runs) else {
            continue;
        };
        let n = attempt(&key);
        if !(agent.status == RunStatus::Abandoned
            || (agent.status == RunStatus::Unfinished && n > cfg.resume_limit))
        {
            continue;
        }
        let non_blank: Vec<String> = agent
            .summary
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(String::from)
            .collect();
        let start = non_blank.len().saturating_sub(cfg.tail_lines);
        let tail = non_blank[start..].to_vec();
        let worktree = lineage(&base, runs)
            .into_iter()
            .rev()
            .find_map(|(_, r)| (!r.worktree.is_empty()).then(|| r.worktree.clone()))
            .unwrap_or_default();
        out.push(Stuck {
            key: base,
            runs: n,
            since: agent.finished.unwrap_or(now),
            tail,
            worktree,
        });
    }
    out.sort_by_key(|s| s.since);
    out
}

/// What the owner is told about a stuck task (stable while the task stays stuck) (R71).
pub fn stuck_sentence(
    stuck: &Stuck,
    where_stopped: Option<&Stopped>,
    cfg: &DispatchConfig,
) -> String {
    let tool = &cfg.tool;
    let work = match where_stopped {
        Some(w) => format!(
            "{} commits and {} changed files are in it",
            w.commits.len(),
            w.changes.len()
        ),
        None => "its worktree is unknown".to_string(),
    };
    let needs = if where_stopped.is_some_and(|w| !w.commits.is_empty() || !w.changes.is_empty()) {
        format!(
            "someone to finish or push what is there, then `{tool} attach {} <pr>`",
            stuck.key
        )
    } else {
        format!(
            "someone to queue the task again with `{tool} todo`, or drop it; nothing was written"
        )
    };
    let log = if stuck.tail.is_empty() {
        "  (its log is empty)".to_string()
    } else {
        stuck
            .tail
            .iter()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let wt = if stuck.worktree.is_empty() {
        "unknown"
    } else {
        &stuck.worktree
    };
    format!(
        "{} ended {} times without its final report and will not be resumed again.\n\
         Worktree: {wt} ({work}).\n\
         Last lines of its log:\n\
         {log}\n\
         It needs: {needs}",
        stuck.key, stuck.runs
    )
}

fn span(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    if minutes < 120 {
        format!("{minutes} min")
    } else {
        format!("{} h", minutes / 60)
    }
}

/// Digest lines for stuck tasks (R72).
pub fn stuck_digest_lines(stuck: &[Stuck], now: i64) -> Vec<String> {
    stuck
        .iter()
        .map(|s| {
            let wt = if s.worktree.is_empty() {
                "unknown"
            } else {
                &s.worktree
            };
            format!(
                "{} has been stuck for {} after {} runs (worktree {wt})",
                s.key,
                span(now - s.since),
                s.runs
            )
        })
        .collect()
}

/// Whether the daily digest is due (`now - last_sent >= 86400` and there is news or stuck tasks) (R72).
pub fn digest_due(
    last_sent: i64,
    now: i64,
    events: &[(String, String)],
    stuck_lines: &[String],
) -> bool {
    (!events.is_empty() || !stuck_lines.is_empty()) && now - last_sent >= 86_400
}

/// Format the daily digest email `(subject, body)` (R72).
pub fn digest_mail(
    events: &[(String, String)],
    stuck_lines: &[String],
    progress: &str,
    cfg: &DispatchConfig,
) -> (String, String) {
    let events_text = if events.is_empty() {
        "No events.".to_string()
    } else {
        events
            .iter()
            .map(|(at, line)| format!("{at}  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut parts = vec![events_text];
    if !stuck_lines.is_empty() {
        parts.push(format!("STUCK\n{}", stuck_lines.join("\n")));
    }
    if !progress.is_empty() {
        parts.push(format!("PROGRESS\n{progress}"));
    }
    (
        format!("{}: the day's events", cfg.tool),
        format!("{}\n", parts.join("\n\n")),
    )
}

/// The PR a task belongs to or produced, if known (`owner_tick._task_pr`) (R73).
pub fn task_pr(base: &str, runs: &Runs, cfg: &DispatchConfig) -> Option<u64> {
    for (_, run) in lineage(base, runs).into_iter().rev() {
        if let Some(pr) = run.for_pr {
            return Some(pr);
        }
    }
    if let Some(pr) = cfg.hands_pr(base) {
        return Some(pr);
    }
    best_pr(base, runs).map(|pr| pr.number)
}

/// Retire stuck tasks whose recorded PR is no longer open (one event, no mail) (R73).
pub fn retire_closed_pr_stuck(
    runs: &mut Runs,
    open_prs: &BTreeSet<u64>,
    now: i64,
    cfg: &DispatchConfig,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let stuck = stuck_runs(runs, now, cfg);
    for s in stuck {
        let Some(pr_num) = task_pr(&s.key, runs, cfg) else {
            continue;
        };
        if open_prs.contains(&pr_num) {
            continue;
        }
        if let Some((latest_key, _)) = latest(&s.key, runs) {
            if let Some(run) = runs.get_mut(&latest_key) {
                run.status = RunStatus::Retired;
            }
        }
        let msg = format!(
            "retired stuck task {}: PR #{pr_num} is no longer open",
            s.key
        );
        actions.push(Action::new(
            ActionKind::RetireStuck {
                key: s.key,
                pr: pr_num,
            },
            msg,
        ));
    }
    actions
}

/// The owner-issue comment for a task that used its retries: the last `escalation_lines` of its log (R74).
pub fn escalation(key: &str, runs_made: u32, log: &str, cfg: &DispatchConfig) -> String {
    let non_blank: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = non_blank.len().saturating_sub(cfg.escalation_lines);
    let tail = if non_blank.is_empty() {
        "(its log is empty)".to_string()
    } else {
        non_blank[start..].join("\n")
    };
    format!(
        "`{key}` ended {runs_made} times without its final report and will not be dispatched again.\n\n\
         Last {} lines of its log:\n\n```\n{tail}\n```",
        cfg.escalation_lines
    )
}
