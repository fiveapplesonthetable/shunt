//! Event-driven top-level dispatch engine: orchestrates `work_step`, `reap`, `record_prs`,
//! `resume`, `owner_step`, and `decide_dispatch` as pure, replayable state transitions.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::config::DispatchConfig;
use super::hands::{barred_rebases, next_hands};
use super::model::{
    Action, ActionKind, BlockedRecord, FailureRec, FixAttemptRec, ForgePr, HandsRec, HistoryItem,
    Holds, OpenPr, PrStanding, PullFacts, RefusedRecord, RetiredRecord, Runs, Session, StartKind,
    StartOutcome, Stopped, TodoItem, TrainView, WorktreeRec,
};
use super::queue::{decision_tasks, retire_closed_afters, retire_items};
use super::report::record_prs;
use super::resume::{
    decide_resume, digest_due, digest_mail, escalation, reap_runs, retire_closed_pr_stuck,
    stuck_digest_lines, stuck_runs, stuck_sentence, ProcObs,
};
use super::runs::{latest, running_count};
use super::select::{next_work, person_decisions, skipped_news, stacked_task, PickedWork};
use super::start::{decide_start, StartRequest};

/// Complete pure dispatch state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DispatchState {
    pub todo: Vec<TodoItem>,
    pub runs: Runs,
    pub holds: Holds,
    pub retired: BTreeMap<String, RetiredRecord>,
    pub task_history: BTreeMap<String, HistoryItem>,
    pub area_heads: BTreeMap<String, String>,
    pub hands: BTreeMap<u64, HandsRec>,
    pub refused: BTreeMap<String, RefusedRecord>,
    pub blocked: Option<BlockedRecord>,
    pub session: Option<Session>,
    pub worktrees: BTreeMap<String, WorktreeRec>,
    pub trains: TrainView,
    pub failed: BTreeMap<u64, FailureRec>,
    pub fixes: BTreeMap<u64, FixAttemptRec>,
    pub person_told: BTreeMap<String, String>,
    pub escalated: BTreeMap<String, i64>,
    pub digest_events: Vec<(String, String)>,
    pub digest_sent: Option<i64>,
}

/// External world observation passed into the dispatch engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DispatchObs {
    pub now: i64,
    pub busy: bool,
    pub env_live: bool,
    pub live_env_text: Option<String>,
    pub standing: BTreeMap<u64, PrStanding>,
    /// `None` when the forge is unreachable (e.g. GitHub offline).
    pub open_prs: Option<Vec<OpenPr>>,
    pub forge_prs: Vec<ForgePr>,
    pub branches_by_pr: BTreeMap<u64, String>,
    pub pull_facts: BTreeMap<u64, Result<PullFacts, String>>,
    pub procs: BTreeMap<String, ProcObs>,
    pub alive_pids: BTreeSet<u32>,
    pub logs: BTreeMap<String, String>,
    pub worktrees_made: BTreeMap<String, Vec<String>>,
    pub stopped_states: BTreeMap<String, Stopped>,
    pub progress_text: String,
}

/// Events that trigger a dispatch decision pass.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchEvent {
    /// Full tick pass: tidy (reap, record_prs, resume), work (now/hands/queue), and owner loop.
    Tick { dry_run: bool },
    /// Run `tick.work` only (choose and start urgent, hands, or queued item).
    Work { dry_run: bool },
    /// Direct start request (`agents.start`).
    DirectStart(StartRequest),
    /// Reap finished processes, attribute PRs, and resume due runs.
    Tidy { dry_run: bool },
    /// Run the owner notification/escalation/digest loop.
    OwnerLoop,
}

fn start_queued_item(
    picked: &PickedWork,
    dry_run: bool,
    state: &mut DispatchState,
    obs: &DispatchObs,
    cfg: &DispatchConfig,
) -> Vec<Action> {
    let item = &picked.item;
    let task_text = stacked_task(item, picked.base.as_ref(), cfg);
    let model = item
        .model
        .clone()
        .unwrap_or_else(|| cfg.providers.claude_default.clone());
    let worktree = state
        .worktrees
        .get(&item.key)
        .map(|w| w.path.clone())
        .unwrap_or_default();
    let req = StartRequest {
        key: item.key.clone(),
        task: task_text,
        model,
        dry_run,
        area: item.area.clone(),
        worktree,
        base: picked.base.clone(),
        pr: item.pr,
        resume_session: String::new(),
        provider_override: item.provider.clone(),
        pinned: item.provider.is_some(),
        kind: if item.now {
            StartKind::Urgent
        } else {
            StartKind::Queued
        },
        new_session_id: None,
    };
    let facts = item.pr.and_then(|pr| obs.pull_facts.get(&pr));
    let (outcome, mut actions) = decide_start(
        &req,
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        state.session.as_ref(),
        facts,
        obs.env_live,
        obs.now,
        cfg,
    );
    if !dry_run {
        if let Some(ref_rec) = state.refused.get(&item.key).cloned() {
            if let Some(pr) = item.pr {
                let mut gone = BTreeMap::new();
                gone.insert(pr, ref_rec.reason);
                let withdraw_actions =
                    retire_items(&mut state.todo, &mut state.task_history, &gone);
                actions.extend(withdraw_actions);
            }
            state.refused.remove(&item.key);
        }
    }
    let said = outcome.message(&item.key);
    let event_line = if said.is_empty() {
        format!("{} already started", item.key)
    } else {
        said.clone()
    };
    actions.push(Action::new(
        ActionKind::Announce { news: false },
        event_line,
    ));
    if let (Some(skipped_pr), StartOutcome::Started(_)) = (picked.skipped, &outcome) {
        let news = skipped_news(&item.key, skipped_pr);
        state
            .digest_events
            .push((obs.now.to_string(), news.clone()));
        actions.push(Action::new(ActionKind::Announce { news: true }, news));
    }
    actions
}

/// Pure equivalent of `tick.work`:
/// 1. Urgent (`now`) item if ready (ignores slots and area lane).
/// 2. Lowest-numbered unbarred `hands` rebase if `running < slots` and not busy; if refused,
///    falls through in the same tick!
/// 3. Retire `--after pr:<n>` items whose PR closed without landing, then start the oldest ready
///    ordinary queue item (R28).
pub fn work_step(
    state: &mut DispatchState,
    obs: &DispatchObs,
    cfg: &DispatchConfig,
    dry_run: bool,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let running = running_count(&state.runs);
    let picked = next_work(
        &state.todo,
        &state.runs,
        running,
        cfg.slots,
        obs.busy,
        &obs.standing,
        &state.holds,
        &state.area_heads,
        cfg,
    );
    if let Some(ref p) = picked {
        if p.item.now {
            return start_queued_item(p, dry_run, state, obs, cfg);
        }
    }
    let barred = barred_rebases(
        obs.open_prs.as_deref(),
        &state.trains,
        &state.holds,
        &state.hands,
        cfg,
    );
    if let Some((n, task)) = next_hands(
        &state.hands,
        &state.runs,
        running,
        cfg.slots,
        obs.busy,
        &barred,
        cfg,
    ) {
        let key = cfg.hands_key(n);
        let worktree = state
            .worktrees
            .get(&key)
            .map(|w| w.path.clone())
            .unwrap_or_default();
        let req = StartRequest {
            key: key.clone(),
            task,
            model: cfg.providers.claude_default.clone(),
            dry_run,
            area: String::new(),
            worktree,
            base: None,
            pr: Some(n),
            resume_session: String::new(),
            provider_override: None,
            pinned: false,
            kind: StartKind::Hands,
            new_session_id: None,
        };
        let facts = obs.pull_facts.get(&n);
        let (outcome, start_actions) = decide_start(
            &req,
            &mut state.runs,
            &state.holds,
            &mut state.refused,
            &mut state.blocked,
            state.session.as_ref(),
            facts,
            obs.env_live,
            obs.now,
            cfg,
        );
        let said = outcome.message(&key);
        actions.extend(start_actions);
        if !state.refused.contains_key(&key) {
            let line = if said.is_empty() {
                format!("{key} already started")
            } else {
                said
            };
            actions.push(Action::new(ActionKind::Announce { news: false }, line));
            return actions;
        }
        if !said.is_empty() {
            actions.push(Action::new(ActionKind::Announce { news: false }, said));
        }
        // Hands start was refused -> fall through to ordinary queue work in the same tick!
    }
    if !dry_run {
        actions.extend(retire_closed_afters(
            &mut state.todo,
            &obs.standing,
            &mut state.task_history,
        ));
    }
    let running = running_count(&state.runs);
    if let Some(p) = next_work(
        &state.todo,
        &state.runs,
        running,
        cfg.slots,
        obs.busy,
        &obs.standing,
        &state.holds,
        &state.area_heads,
        cfg,
    ) {
        actions.extend(start_queued_item(&p, dry_run, state, obs, cfg));
    }
    actions
}

/// Pure equivalent of `tick.tidy`: reap finished runs, attribute PRs, and resume unfinished runs.
pub fn tidy_step(
    state: &mut DispatchState,
    obs: &DispatchObs,
    cfg: &DispatchConfig,
    dry_run: bool,
) -> Vec<Action> {
    let mut actions = Vec::new();
    if !dry_run {
        let (_, reap_actions) = reap_runs(
            &mut state.runs,
            &obs.procs,
            &obs.logs,
            &state.worktrees,
            &obs.worktrees_made,
            obs.now,
            cfg,
        );
        actions.extend(reap_actions);
        for line in record_prs(&mut state.runs, &obs.forge_prs, &obs.branches_by_pr, cfg) {
            actions.push(Action::new(
                ActionKind::Announce { news: false },
                format!("agent {line}"),
            ));
        }
    }
    let barred = barred_rebases(
        obs.open_prs.as_deref(),
        &state.trains,
        &state.holds,
        &state.hands,
        cfg,
    );
    let (_, resume_actions) = decide_resume(
        &mut state.runs,
        &state.holds,
        &mut state.refused,
        &mut state.blocked,
        state.session.as_ref(),
        &state.trains.in_flight,
        &barred,
        &state.worktrees,
        &obs.worktrees_made,
        &obs.stopped_states,
        &obs.pull_facts,
        &obs.alive_pids,
        dry_run,
        obs.env_live,
        obs.now,
        cfg,
    );
    actions.extend(resume_actions);
    actions
}

/// Pure equivalent of the dispatch parts of `tick.owner_loop`:
/// 1. Retire stuck tasks whose recorded PR is closed.
/// 2. Mail person decisions (area chain waiting on a person + stuck tasks) once while unchanged.
/// 3. Post escalation comments for stuck tasks to the owner issue once.
/// 4. Send the daily digest if due and there is news or stuck tasks.
pub fn owner_step(
    state: &mut DispatchState,
    obs: &DispatchObs,
    cfg: &DispatchConfig,
) -> Vec<Action> {
    let mut actions = Vec::new();
    if let Some(open_list) = &obs.open_prs {
        let open_nums: BTreeSet<u64> = open_list.iter().map(|p| p.number).collect();
        actions.extend(retire_closed_pr_stuck(
            &mut state.runs,
            &open_nums,
            obs.now,
            cfg,
        ));
    }
    let tasks = decision_tasks(&state.todo, &state.task_history, &state.area_heads);
    let mut waiting = person_decisions(&tasks, &state.runs, &obs.standing, &state.area_heads, cfg);
    let stuck_list = stuck_runs(&state.runs, obs.now, cfg);
    for stuck in &stuck_list {
        let where_ = obs.stopped_states.get(&stuck.worktree);
        waiting.insert(
            format!("stuck-{}", stuck.key),
            stuck_sentence(stuck, where_, cfg),
        );
    }
    for (key, why) in &waiting {
        if state.person_told.get(key) != Some(why) {
            let line = format!("{key} needs a person: {why}");
            state
                .digest_events
                .push((obs.now.to_string(), line.clone()));
            actions.push(Action::new(ActionKind::Announce { news: true }, line));
            actions.push(Action::new(
                ActionKind::MailOwner {
                    key: key.clone(),
                    subject: format!("{}: {key} needs you", cfg.tool),
                    body: why.clone(),
                },
                format!("mail owner: {key} needs you"),
            ));
            state.person_told.insert(key.clone(), why.clone());
        }
    }
    state.person_told.retain(|k, _| waiting.contains_key(k));

    for stuck in &stuck_list {
        if state.escalated.contains_key(&stuck.key) {
            continue;
        }
        let log = latest(&stuck.key, &state.runs)
            .and_then(|(k, r)| {
                obs.logs
                    .get(&k)
                    .map(String::as_str)
                    .or_else(|| (!r.summary.is_empty()).then_some(r.summary.as_str()))
            })
            .unwrap_or("");
        let body = escalation(&stuck.key, stuck.runs, log, cfg);
        state.escalated.insert(stuck.key.clone(), obs.now);
        actions.push(Action::new(
            ActionKind::CommentIssue {
                issue: cfg.owner_issue,
                key: stuck.key.clone(),
                body,
            },
            format!(
                "escalate stuck task {} to issue #{}",
                stuck.key, cfg.owner_issue
            ),
        ));
    }

    match state.digest_sent {
        None => {
            state.digest_sent = Some(obs.now);
        }
        Some(sent_at) => {
            let stuck_lines = stuck_digest_lines(&stuck_list, obs.now);
            if digest_due(sent_at, obs.now, &state.digest_events, &stuck_lines) {
                let (subject, body) =
                    digest_mail(&state.digest_events, &stuck_lines, &obs.progress_text, cfg);
                state.digest_events.clear();
                state.digest_sent = Some(obs.now);
                actions.push(Action::new(
                    ActionKind::MailOwner {
                        key: "digest".to_string(),
                        subject,
                        body,
                    },
                    "daily digest due",
                ));
            }
        }
    }
    actions
}

/// Event-driven top-level dispatch decision function, deterministic and replayable from the log.
pub fn decide_dispatch(
    state: &DispatchState,
    event: &DispatchEvent,
    obs: &DispatchObs,
    cfg: &DispatchConfig,
) -> (DispatchState, Vec<Action>) {
    let mut next_state = state.clone();
    let effective_cfg = match &obs.live_env_text {
        Some(text) => cfg.refresh_live(text).0,
        None => cfg.clone(),
    };
    let mut actions = Vec::new();
    match event {
        DispatchEvent::Tick { dry_run } => {
            actions.extend(tidy_step(&mut next_state, obs, &effective_cfg, *dry_run));
            actions.extend(work_step(&mut next_state, obs, &effective_cfg, *dry_run));
            if !*dry_run {
                actions.extend(owner_step(&mut next_state, obs, &effective_cfg));
            }
        }
        DispatchEvent::Work { dry_run } => {
            actions.extend(work_step(&mut next_state, obs, &effective_cfg, *dry_run));
        }
        DispatchEvent::DirectStart(req) => {
            let facts = req.pr.and_then(|pr| obs.pull_facts.get(&pr));
            let (_, start_actions) = decide_start(
                req,
                &mut next_state.runs,
                &next_state.holds,
                &mut next_state.refused,
                &mut next_state.blocked,
                next_state.session.as_ref(),
                facts,
                obs.env_live,
                obs.now,
                &effective_cfg,
            );
            actions.extend(start_actions);
        }
        DispatchEvent::Tidy { dry_run } => {
            actions.extend(tidy_step(&mut next_state, obs, &effective_cfg, *dry_run));
        }
        DispatchEvent::OwnerLoop => {
            actions.extend(owner_step(&mut next_state, obs, &effective_cfg));
        }
    }
    (next_state, actions)
}
