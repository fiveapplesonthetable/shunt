//! Queue hygiene (`compact`, `withdraw`, `decision_tasks`, `retire_closed_afters`, `in_flight_members`)
//! and fix-agent dispatch decisions (R18, R39, R47, R75–R78).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::config::DispatchConfig;
use super::holds::closed_after;
use super::model::{
    Action, ActionKind, BlockedRecord, FailureRec, FixAttemptRec, FixStep, HandsRec, HistoryItem,
    Holds, PrStanding, PullFacts, RefusedRecord, RunState, RunStatus, Runs, Session, StartKind,
    StartOutcome, TodoItem,
};
use super::runs::{run_state, running_count};
use super::start::{decide_start, StartRequest};

/// Open train record used by `in_flight_members` (R39).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OpenTrainRec {
    pub status: String,
    /// `(pr_number, sha)` pairs.
    #[serde(default)]
    pub members: Vec<(u64, String)>,
}

/// Train PR record used by `in_flight_members` (R39).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrainPrRec {
    #[serde(default)]
    pub members: Vec<u64>,
    #[serde(default)]
    pub landed: bool,
    #[serde(default)]
    pub abandoned: bool,
}

/// The PR numbers carried by an open train or by a train PR that is neither landed nor abandoned (R39).
pub fn in_flight_members(
    trains: &BTreeMap<String, OpenTrainRec>,
    train_prs: &BTreeMap<u64, TrainPrRec>,
) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    for tr in trains.values() {
        if tr.status == "open" {
            for &(n, _) in &tr.members {
                out.insert(n);
            }
        }
    }
    for tpr in train_prs.values() {
        if !tpr.landed && !tpr.abandoned {
            for &n in &tpr.members {
                out.insert(n);
            }
        }
    }
    out
}

/// Split the queue into `(kept, withdrawn)` where `withdrawn` holds `(item, "<kind> for #<pr>: <why>")`
/// for each item whose `pr` is in `gone` (R78).
pub fn withdraw(
    todo: &[TodoItem],
    gone: &BTreeMap<u64, String>,
) -> (Vec<TodoItem>, Vec<(TodoItem, String)>) {
    let mut kept = Vec::new();
    let mut withdrawn = Vec::new();
    for item in todo {
        if let Some(pr) = item.pr {
            if let Some(why) = gone.get(&pr) {
                let msg = format!("{} for #{pr}: {why}", item.kind.as_str());
                withdrawn.push((item.clone(), msg));
                continue;
            }
        }
        kept.push(item.clone());
    }
    (kept, withdrawn)
}

/// Withdraw items whose `pr` is in `gone` and record them in `history` as `Retired` (R47, R78).
pub fn retire_items(
    todo: &mut Vec<TodoItem>,
    history: &mut BTreeMap<String, HistoryItem>,
    gone: &BTreeMap<u64, String>,
) -> Vec<Action> {
    let (kept, withdrawn) = withdraw(todo, gone);
    *todo = kept;
    let mut actions = Vec::new();
    for (item, why) in withdrawn {
        let key = item.key.clone();
        let pr = item.pr.unwrap_or(0);
        history.insert(
            key.clone(),
            HistoryItem {
                item,
                status: RunStatus::Retired,
                finished: None,
                why: why.clone(),
            },
        );
        actions.push(Action::new(ActionKind::Withdraw { key, pr }, why));
    }
    actions
}

/// Move finished (`Done` or `Retired`) queue items into `history` and update `area_heads` for
/// non-`now` area items (R76).
pub fn compact(
    todo: &[TodoItem],
    runs: &Runs,
    history: &BTreeMap<String, HistoryItem>,
    area_heads: &BTreeMap<String, String>,
) -> (
    Vec<TodoItem>,
    BTreeMap<String, HistoryItem>,
    BTreeMap<String, String>,
) {
    let mut waiting = Vec::new();
    let mut archived = history.clone();
    let mut heads = area_heads.clone();
    for item in todo {
        let Some(run) = runs.get(&item.key) else {
            waiting.append(&mut vec![item.clone()]);
            continue;
        };
        if !matches!(run.status, RunStatus::Done | RunStatus::Retired) {
            waiting.push(item.clone());
            continue;
        }
        archived.insert(
            item.key.clone(),
            HistoryItem {
                item: item.clone(),
                status: run.status,
                finished: run.finished,
                why: String::new(),
            },
        );
        if !item.area.is_empty() && !item.now {
            heads.insert(item.area.clone(), item.key.clone());
        }
    }
    (waiting, archived, heads)
}

/// The active queue preceded by each area's latest completed entry from `history` that is not in
/// the active queue (R77).
pub fn decision_tasks(
    todo: &[TodoItem],
    history: &BTreeMap<String, HistoryItem>,
    area_heads: &BTreeMap<String, String>,
) -> Vec<TodoItem> {
    let active: BTreeSet<&str> = todo.iter().map(|t| t.key.as_str()).collect();
    let mut seen_heads = BTreeSet::new();
    let mut out = Vec::new();
    for key in area_heads.values() {
        if seen_heads.insert(key.as_str()) && !active.contains(key.as_str()) {
            if let Some(h) = history.get(key) {
                out.push(h.item.clone());
            }
        }
    }
    out.extend(todo.iter().cloned());
    out
}

/// Retire queued tasks waiting with `--after pr:<n>` on a PR closed without landing (R18).
pub fn retire_closed_afters(
    todo: &mut Vec<TodoItem>,
    standing: &BTreeMap<u64, PrStanding>,
    history: &mut BTreeMap<String, HistoryItem>,
) -> Vec<Action> {
    let mut kept = Vec::new();
    let mut actions = Vec::new();
    for item in todo.drain(..) {
        if let Some(pr) = closed_after(&item.after, standing) {
            let why = format!("#{pr} was closed without landing on main");
            let key = item.key.clone();
            history.insert(
                key.clone(),
                HistoryItem {
                    item,
                    status: RunStatus::Retired,
                    finished: None,
                    why: why.clone(),
                },
            );
            actions.push(Action::new(ActionKind::RetireClosedAfter { key, pr }, why));
        } else {
            kept.push(item);
        }
    }
    *todo = kept;
    actions
}

/// Remove and stop rebase work whose PR has landed (`tick.retire_rebase_work`).
pub fn retire_rebase_work(
    members: &BTreeSet<u64>,
    todo: &mut Vec<TodoItem>,
    history: &mut BTreeMap<String, HistoryItem>,
    hands: &mut BTreeMap<u64, HandsRec>,
    runs: &mut Runs,
    now: i64,
    cfg: &DispatchConfig,
) -> Vec<Action> {
    if members.is_empty() {
        return Vec::new();
    }
    let gone: BTreeMap<u64, String> = members
        .iter()
        .map(|&n| (n, "PR merged".to_string()))
        .collect();
    let mut actions = retire_items(todo, history, &gone);
    for &n in members {
        hands.remove(&n);
        let key = cfg.hands_key(n);
        if let Some(run) = runs.get_mut(&key) {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Retired;
                run.finished = Some(now);
                run.summary = "PR merged".to_string();
                actions.push(Action::new(
                    ActionKind::StopUnit {
                        key: key.clone(),
                        unit: (!run.unit.is_empty()).then(|| run.unit.clone()),
                        pid: run.pid,
                    },
                    format!("PR #{n} merged; retired running {key}"),
                ));
            }
        }
    }
    actions
}

/// Key of the fix agent for PR `n` (`"fix-n"`) (R75).
pub fn fix_key(n: u64, cfg: &DispatchConfig) -> String {
    format!("{}{n}", cfg.fix_prefix)
}

/// The brief for the agent that repairs PR `n` whose batch failed in its own code (R75).
pub fn fix_task(n: u64, branch: &str, failure: &FailureRec, cfg: &DispatchConfig) -> String {
    let tests = if failure.tests.is_empty() {
        "none named".to_string()
    } else {
        failure.tests.join(", ")
    };
    let areas = if failure.areas.is_empty() {
        "not recorded".to_string()
    } else {
        failure.areas.join(" ")
    };
    let log = if failure.log.is_empty() {
        "(none)"
    } else {
        &failure.log
    };
    let tool = &cfg.tool;
    format!(
        "The batch for PR #{n} (branch `{branch}`) is red for a reason in its own code; main \
         alone passes the area.\n\n\
         Area: {areas}\n\
         Failed: {}\n\
         Tests: {tests}\n\n\
         Log excerpt:\n\
         {log}\n\n\
         Fix the root cause on the same branch, not by loosening the check. Make a worktree of \
         the branch (`git fetch origin {branch}`, then `git worktree add <path> origin/{branch}`), \
         commit the fix there, push to `{branch}` without force, and run `{tool} tick` so the PR \
         is batched again. Do not open a second PR.",
        failure.why
    )
}

/// `Start`, `Wait`, `Tell`, or `Done` for a PR that has `failure` on its head (R75).
pub fn decide_fix(
    failure: &FailureRec,
    record: Option<&FixAttemptRec>,
    state: RunState,
) -> FixStep {
    let Some(rec) = record else {
        return FixStep::Start;
    };
    if rec.told {
        return FixStep::Done;
    }
    if failure.sha != rec.sha || state == RunState::Done {
        FixStep::Tell
    } else {
        FixStep::Wait
    }
}

/// Owner mail `(subject, body)` when the fix agent's attempt did not turn the batch green (R75).
pub fn fix_failure_mail(
    n: u64,
    failure: &FailureRec,
    pushed: bool,
    cfg: &DispatchConfig,
) -> (String, String) {
    let tests = if failure.tests.is_empty() {
        "no test named".to_string()
    } else {
        failure.tests.join(", ")
    };
    let areas = if failure.areas.is_empty() {
        "not recorded".to_string()
    } else {
        failure.areas.join(" ")
    };
    let how = if pushed {
        "pushed a fix that is still red"
    } else {
        "pushed nothing"
    };
    let tool = &cfg.tool;
    let subject = format!("{tool}: #{n} is still red after its fix agent");
    let body = format!(
        "The fix agent for #{n} {how}.\n\n\
         Area: {areas}\n\
         Failed: {}\n\
         Tests: {tests}\n\n\
         {}\n\n\
         It will not get another agent by itself. Push to the branch, or run `{tool} batch {n}` when the cause is gone.\n",
        failure.why, failure.log
    );
    (subject, body)
}

/// Run the fix-failures dispatch pass over all failed PRs (skipping `train_prs`) (R75).
#[allow(clippy::too_many_arguments)]
pub fn decide_fixes(
    failed: &BTreeMap<u64, FailureRec>,
    fixes: &mut BTreeMap<u64, FixAttemptRec>,
    train_prs: &BTreeSet<u64>,
    runs: &mut Runs,
    holds: &Holds,
    refused: &mut BTreeMap<String, RefusedRecord>,
    blocked: &mut Option<BlockedRecord>,
    profile: Option<&Session>,
    branches_by_pr: &BTreeMap<u64, String>,
    pull_facts: &BTreeMap<u64, Result<PullFacts, String>>,
    busy: bool,
    env_live: bool,
    now: i64,
    cfg: &DispatchConfig,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut running = running_count(runs);
    for (&n, failure) in failed {
        if train_prs.contains(&n) {
            continue;
        }
        let key = fix_key(n, cfg);
        let state = run_state(&key, runs, cfg);
        let rec = fixes.get(&n).cloned();
        match decide_fix(failure, rec.as_ref(), state) {
            FixStep::Start => {
                if cfg.slots > 0 && (running as i32) < cfg.slots && !busy {
                    let branch = branches_by_pr
                        .get(&n)
                        .map(String::as_str)
                        .unwrap_or("unknown");
                    let task = fix_task(n, branch, failure, cfg);
                    let req = StartRequest {
                        key: key.clone(),
                        task,
                        model: cfg.providers.claude_default.clone(),
                        dry_run: false,
                        area: "fix".to_string(),
                        worktree: String::new(),
                        base: None,
                        pr: Some(n),
                        resume_session: String::new(),
                        provider_override: None,
                        pinned: false,
                        kind: StartKind::Fix,
                        new_session_id: None,
                    };
                    let (outcome, start_actions) = decide_start(
                        &req,
                        runs,
                        holds,
                        refused,
                        blocked,
                        profile,
                        pull_facts.get(&n),
                        env_live,
                        now,
                        cfg,
                    );
                    actions.extend(start_actions);
                    if matches!(outcome, StartOutcome::Started(_)) {
                        running += 1;
                        fixes.insert(
                            n,
                            FixAttemptRec {
                                sha: failure.sha.clone(),
                                told: false,
                            },
                        );
                        actions.push(Action::new(
                            ActionKind::Announce { news: false },
                            format!("#{n} failed its batch in its own code; started a fix agent"),
                        ));
                    }
                }
            }
            FixStep::Tell => {
                if let Some(rec) = fixes.get_mut(&n) {
                    let pushed = failure.sha != rec.sha;
                    let (subject, body) = fix_failure_mail(n, failure, pushed, cfg);
                    rec.told = true;
                    actions.push(Action::new(
                        ActionKind::MailOwner {
                            key: format!("fix-{n}"),
                            subject,
                            body,
                        },
                        format!("#{n} did not pass after its fix agent; emailed the owner"),
                    ));
                    actions.push(Action::new(
                        ActionKind::Announce { news: true },
                        format!("#{n} did not pass after its fix agent; emailed the owner"),
                    ));
                }
            }
            FixStep::Wait | FixStep::Done => {}
        }
    }
    actions
}
