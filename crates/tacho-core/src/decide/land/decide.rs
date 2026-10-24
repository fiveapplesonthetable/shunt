//! Pure, replayable event-driven decision engine for the `land` concern (`decide_land`).
//!
//! Every [`LandAction`] carries a human-readable `because` reason explaining why the decision
//! was made. No I/O, no system clocks, no sleeps.

use super::assemble::{
    body, dropped_line, left_out_reason, red_fix_tasks, title, train_tasks, BodyParts, LandTask,
    MainCi, CI_NOTICE,
};
use super::config::LandConfig;
use super::landed::{
    abandoned, landed_comment, main_moved_news, newly_landed, retire_rebases, train_abandoned_news,
    train_landed_news, RetiredRebase,
};
use super::ready::{is_conflicting, Conflicted, Passed};
use super::rebase::Hands;
use super::release::{letter, waiting_for, ProvedBatch};
use super::train::{
    after_outside_merge, did_not_land_subject, finished, landed_line, mismatch, refusal,
    stopped_loud_line, stopped_why, MailState, Member, Sha, Train, TrainId, TrainStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A red batch failure recorded against a combined train PR (`R-ML-9`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainFailure {
    pub sha: Sha,
    pub areas: Vec<String>,
    pub why: String,
    pub log: String,
}

/// Complete pure state snapshot for the `land` concern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LandView {
    pub main_sha: Sha,
    pub main_seen: Option<Sha>,
    pub main_ci: MainCi,
    pub last_ci_change: Option<Sha>,
    pub open_prs: BTreeMap<u64, Sha>,
    pub open_drafts: BTreeSet<u64>,
    pub conflicting: BTreeMap<u64, bool>,
    pub trains: Vec<Train>,
    pub merged_prs: BTreeSet<u64>,
    pub merged_at: BTreeMap<u64, i64>,
    pub passed: BTreeMap<u64, Passed>,
    pub proved_batches: BTreeMap<String, ProvedBatch>,
    pub failed_trains: BTreeMap<u64, TrainFailure>,
    pub todo: Vec<LandTask>,
    pub retired_tasks: BTreeMap<String, RetiredRebase>,
    pub hands: BTreeMap<u64, Hands>,
    pub sibling_notified: BTreeMap<u64, Sha>,
    pub notified_ready: Vec<u64>,
    pub notified_trains: BTreeSet<TrainId>,
    pub train_refused: BTreeMap<String, String>,
}

/// Events routed to [`decide_land`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LandEvent {
    /// Owner ran `<tool> land <id>`.
    LandRequested { id: TrainId },
    /// Code host reported the result of merging member `pr` of sequential train `id`.
    MergeResult {
        id: TrainId,
        pr: u64,
        ok: bool,
        err: String,
        now: i64,
    },
    /// Hands fetched `main` after merging all members of sequential train `id`.
    MainTreeVerified {
        id: TrainId,
        main_sha: Sha,
        main_tree: Sha,
        now: i64,
    },
    /// Code host updated mergeability for `pr`.
    MergeabilityChanged { pr: u64, verdict: Conflicted },
    /// Main moved to `new_main` with `merged_since` first-parent PR merges since `main_seen`.
    MainMoved {
        new_main: Sha,
        merged_since: Option<Vec<u64>>,
        now: i64,
    },
    /// Reviewer ran `<tool> release-train <pr>`.
    ReleaseRequested {
        pr: u64,
        head: Sha,
        tree_now: Sha,
        required: BTreeSet<String>,
    },
    /// Periodic or event-driven check to mail any released train whose gate now passes.
    MailCheck {
        pr: u64,
        head: Sha,
        tree_now: Sha,
        required: BTreeSet<String>,
    },
    /// Mail transport reported whether the owner letter for `pr` at `head` was sent.
    MailResult {
        pr: u64,
        head: Sha,
        ok: bool,
        err: String,
    },
    /// Reconcile newly landed and abandoned combined train PRs.
    SettleTrains {
        open_prs: BTreeSet<u64>,
        merged_prs: BTreeSet<u64>,
        now: i64,
    },
    /// Queue one `train-fix-<pr>-<area>` task per failing area of any red combined train.
    QueueRedFixes,
    /// Record and open an assembled combined train PR (`R-OP-1`..`R-OP-6`).
    RecordCombinedTrain {
        pr: u64,
        branch: String,
        head: Sha,
        members: Vec<Member>,
        resolutions: Vec<(u64, String)>,
        left_out: Vec<(u64, Vec<String>)>,
        dropped: Vec<(u64, String)>,
        screens: Vec<String>,
        ci: bool,
    },
}

/// Specific effect the hands must execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LandActionKind {
    RefuseLand {
        id: TrainId,
        reason: String,
    },
    RefuseRelease {
        pr: u64,
        reason: String,
    },
    MarkReady {
        pr: u64,
    },
    MergePr {
        train_id: TrainId,
        pr: u64,
        match_head: Sha,
    },
    WaitForMergeRetry {
        train_id: TrainId,
        pr: u64,
        attempt: u32,
    },
    VerifyMainTree {
        train_id: TrainId,
    },
    OpenPr {
        branch: String,
        title: String,
        body: String,
        draft: bool,
    },
    StartBatch {
        prs: Vec<u64>,
    },
    QueueTask {
        task: LandTask,
    },
    ClosePrWithComment {
        pr: u64,
        comment: String,
    },
    SendMail {
        pr: u64,
        head: Sha,
        subject: String,
        body: String,
    },
    LoudPost {
        subject: String,
        issue_title: String,
        line: String,
    },
    Notify {
        once_key: String,
        line: String,
    },
    News {
        line: String,
    },
}

/// An action with its mandatory human-readable `because` reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandAction {
    pub kind: LandActionKind,
    pub because: String,
}

impl LandAction {
    pub fn new(kind: LandActionKind, because: impl Into<String>) -> Self {
        Self {
            kind,
            because: because.into(),
        }
    }
}

/// Output of [`decide_land`]: the updated state and the ordered plan of actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandDecision {
    pub next: LandView,
    pub actions: Vec<LandAction>,
}

/// Pure decision function `(LandView, LandEvent, LandConfig) -> LandDecision`.
pub fn decide_land(view: &LandView, ev: &LandEvent, cfg: &LandConfig) -> LandDecision {
    let mut next = view.clone();
    let mut actions = Vec::new();

    match ev {
        LandEvent::LandRequested { id } => {
            let Some(idx) = next.trains.iter().position(|t| &t.id == id) else {
                actions.push(LandAction::new(
                    LandActionKind::RefuseLand {
                        id: id.clone(),
                        reason: format!("no train {id}"),
                    },
                    format!("train {id} does not exist in the recorded trains"),
                ));
                return LandDecision { next, actions };
            };
            if let Some(r) = refusal(&next.trains[idx], &next.main_sha, &next.open_prs) {
                actions.push(LandAction::new(
                    LandActionKind::RefuseLand {
                        id: id.clone(),
                        reason: format!("refused: {r}"),
                    },
                    format!("train {id} failed pre-merge verification check: {r}"),
                ));
                return LandDecision { next, actions };
            }
            next.trains[idx].status = TrainStatus::Landing {
                next: 0,
                attempts: 0,
            };
            if let Some(first) = next.trains[idx].members.first().cloned() {
                actions.push(LandAction::new(
                    LandActionKind::MarkReady { pr: first.pr },
                    format!(
                        "member #{} of train {id} must be ready before merge",
                        first.pr
                    ),
                ));
                actions.push(LandAction::new(
                    LandActionKind::MergePr {
                        train_id: id.clone(),
                        pr: first.pr,
                        match_head: first.head,
                    },
                    format!("merge first member #{} of verified train {id}", first.pr),
                ));
            } else {
                next.trains[idx].status = TrainStatus::VerifyingTree;
                actions.push(LandAction::new(
                    LandActionKind::VerifyMainTree {
                        train_id: id.clone(),
                    },
                    format!("verify main tree for empty train {id}"),
                ));
            }
        }

        LandEvent::MergeResult {
            id,
            pr,
            ok,
            err,
            now,
        } => {
            let Some(idx) = next.trains.iter().position(|t| &t.id == id) else {
                return LandDecision { next, actions };
            };
            let TrainStatus::Landing {
                next: step_idx,
                attempts,
            } = next.trains[idx].status
            else {
                return LandDecision { next, actions };
            };
            if next.trains[idx].members.get(step_idx).map(|m| m.pr) != Some(*pr) {
                return LandDecision { next, actions };
            }
            if *ok {
                next.trains[idx].merged.push(*pr);
                next.merged_at.entry(*pr).or_insert(*now);
                next.merged_prs.insert(*pr);
                let next_idx = step_idx + 1;
                if let Some(member) = next.trains[idx].members.get(next_idx).cloned() {
                    next.trains[idx].status = TrainStatus::Landing {
                        next: next_idx,
                        attempts: 0,
                    };
                    actions.push(LandAction::new(
                        LandActionKind::MarkReady { pr: member.pr },
                        format!(
                            "member #{} of train {id} must be ready before merge",
                            member.pr
                        ),
                    ));
                    actions.push(LandAction::new(
                        LandActionKind::MergePr {
                            train_id: id.clone(),
                            pr: member.pr,
                            match_head: member.head,
                        },
                        format!(
                            "merge member #{} ({}/{}) of verified train {id}",
                            member.pr,
                            next_idx + 1,
                            next.trains[idx].members.len()
                        ),
                    ));
                } else {
                    next.trains[idx].status = TrainStatus::VerifyingTree;
                    actions.push(LandAction::new(
                        LandActionKind::VerifyMainTree {
                            train_id: id.clone(),
                        },
                        format!("all members of train {id} merged; verify main's tree matches"),
                    ));
                }
            } else {
                let tried = attempts + 1;
                let max_attempts = cfg.merge_attempts.max(1);
                if tried < max_attempts {
                    next.trains[idx].status = TrainStatus::Landing {
                        next: step_idx,
                        attempts: tried,
                    };
                    actions.push(LandAction::new(
                        LandActionKind::WaitForMergeRetry {
                            train_id: id.clone(),
                            pr: *pr,
                            attempt: tried,
                        },
                        format!(
                            "merge of #{pr} in train {id} failed (attempt {tried}/{max_attempts}: {err}); waiting for mergeability update"
                        ),
                    ));
                } else {
                    let why = stopped_why(*pr, &next.trains[idx].merged, err);
                    let loud = stopped_loud_line(id, &why);
                    next.trains = finished(&next.trains, id, TrainStatus::Stopped { why });
                    next.main_seen = Some(next.main_sha.clone());
                    actions.push(LandAction::new(
                        LandActionKind::LoudPost {
                            subject: did_not_land_subject(&cfg.project),
                            issue_title: cfg.queue_issue_title.clone(),
                            line: loud,
                        },
                        format!("train {id} exhausted {max_attempts} merge attempts at #{pr}"),
                    ));
                }
            }
        }

        LandEvent::MainTreeVerified {
            id,
            main_sha,
            main_tree,
            now,
        } => {
            next.main_sha = main_sha.clone();
            next.main_seen = Some(main_sha.clone());
            let Some(train) = next.trains.iter().find(|t| &t.id == id).cloned() else {
                return LandDecision { next, actions };
            };
            for &m in &train.merged {
                next.merged_at.entry(m).or_insert(*now);
            }
            if let Some(wrong) = mismatch(&train, main_tree) {
                next.trains = finished(
                    &next.trains,
                    id,
                    TrainStatus::Mismatch { why: wrong.clone() },
                );
                actions.push(LandAction::new(
                    LandActionKind::LoudPost {
                        subject: did_not_land_subject(&cfg.project),
                        issue_title: cfg.queue_issue_title.clone(),
                        line: wrong.clone(),
                    },
                    wrong,
                ));
            } else {
                let msg = landed_line(id, &train.merged);
                next.trains = finished(&next.trains, id, TrainStatus::Landed);
                actions.push(LandAction::new(
                    LandActionKind::Notify {
                        once_key: format!("landed:{id}"),
                        line: msg.clone(),
                    },
                    msg,
                ));
            }
        }

        LandEvent::MergeabilityChanged { pr, verdict } => {
            let was = next.conflicting.get(pr).copied().unwrap_or(false);
            let now_conflicting = is_conflicting(*verdict, was);
            next.conflicting.insert(*pr, now_conflicting);
            if !now_conflicting {
                for t in &next.trains {
                    if let TrainStatus::Landing {
                        next: idx,
                        attempts,
                    } = t.status
                    {
                        if attempts > 0 && t.members.get(idx).map(|m| m.pr) == Some(*pr) {
                            let m = &t.members[idx];
                            actions.push(LandAction::new(
                                LandActionKind::MergePr {
                                    train_id: t.id.clone(),
                                    pr: m.pr,
                                    match_head: m.head.clone(),
                                },
                                format!(
                                    "#{pr} became mergeable after attempt {attempts}; retry merge for train {}",
                                    t.id
                                ),
                            ));
                        }
                    }
                }
            }
        }

        LandEvent::MainMoved {
            new_main,
            merged_since,
            now,
        } => {
            let had_seen = next.main_seen.as_deref().is_some_and(|s| !s.is_empty());
            let changed = next.main_seen.as_deref() != Some(new_main.as_str());
            next.main_sha = new_main.clone();
            next.main_seen = Some(new_main.clone());
            if had_seen && changed {
                if let Some(ms) = merged_since {
                    for &n in ms {
                        next.merged_at.entry(n).or_insert(*now);
                        next.merged_prs.insert(n);
                    }
                }
                next.trains = after_outside_merge(&next.trains);
                let line = main_moved_news(new_main, merged_since.as_deref());
                actions.push(LandAction::new(
                    LandActionKind::News { line: line.clone() },
                    line,
                ));
            }
        }

        LandEvent::ReleaseRequested {
            pr,
            head,
            tree_now,
            required,
        } => {
            let Some(idx) = next
                .trains
                .iter()
                .position(|t| t.combined_pr() == Some(*pr))
            else {
                let reason = format!("#{pr} is not a train pull request");
                actions.push(LandAction::new(
                    LandActionKind::RefuseRelease {
                        pr: *pr,
                        reason: reason.clone(),
                    },
                    reason,
                ));
                return LandDecision { next, actions };
            };
            next.trains[idx].reviewed = Some(head.clone());
            try_mail_train(
                &mut next,
                &mut actions,
                idx,
                *pr,
                head,
                tree_now,
                required,
                cfg,
            );
        }

        LandEvent::MailCheck {
            pr,
            head,
            tree_now,
            required,
        } => {
            if let Some(idx) = next
                .trains
                .iter()
                .position(|t| t.combined_pr() == Some(*pr))
            {
                try_mail_train(
                    &mut next,
                    &mut actions,
                    idx,
                    *pr,
                    head,
                    tree_now,
                    required,
                    cfg,
                );
            }
        }

        LandEvent::MailResult { pr, head, ok, err } => {
            if let Some(t) = next
                .trains
                .iter_mut()
                .find(|t| t.combined_pr() == Some(*pr))
            {
                if *ok {
                    t.mail = MailState::Sent { head: head.clone() };
                } else {
                    t.mail = MailState::Failed {
                        head: head.clone(),
                        err: err.clone(),
                    };
                    let line = format!("train #{pr} mail did not send");
                    actions.push(LandAction::new(
                        LandActionKind::Notify {
                            once_key: String::new(),
                            line: line.clone(),
                        },
                        line,
                    ));
                }
            }
        }

        LandEvent::SettleTrains {
            open_prs,
            merged_prs,
            now,
        } => {
            next.merged_prs.extend(merged_prs.iter().copied());
            for train_pr in newly_landed(&next.trains, &next.merged_prs) {
                let Some(idx) = next
                    .trains
                    .iter()
                    .position(|t| t.combined_pr() == Some(train_pr))
                else {
                    continue;
                };
                let members = next.trains[idx].numbers();
                let member_set: BTreeSet<u64> = members.iter().copied().collect();
                let (kept_todo, kept_hands, retired) =
                    retire_rebases(&next.todo, &next.hands, &member_set);
                next.todo = kept_todo;
                next.hands = kept_hands;
                for r in retired {
                    next.retired_tasks.insert(r.key.clone(), r);
                }
                for &m in &members {
                    if open_prs.contains(&m) {
                        actions.push(LandAction::new(
                            LandActionKind::ClosePrWithComment {
                                pr: m,
                                comment: landed_comment(train_pr),
                            },
                            format!("member #{m} landed inside combined train #{train_pr}"),
                        ));
                    }
                    next.merged_at.entry(m).or_insert(*now);
                    next.merged_prs.insert(m);
                }
                next.trains[idx].status = TrainStatus::Landed;
                let line = train_landed_news(train_pr, &members);
                actions.push(LandAction::new(
                    LandActionKind::News { line: line.clone() },
                    line,
                ));
            }
            for train_pr in abandoned(&next.trains, open_prs, &next.merged_prs, cfg.open_limit) {
                if let Some(t) = next
                    .trains
                    .iter_mut()
                    .find(|t| t.combined_pr() == Some(train_pr))
                {
                    t.status = TrainStatus::Abandoned;
                    let line = train_abandoned_news(train_pr);
                    actions.push(LandAction::new(
                        LandActionKind::News { line: line.clone() },
                        line,
                    ));
                }
            }
        }

        LandEvent::QueueRedFixes => {
            let train_prs: Vec<u64> = next.trains.iter().filter_map(|t| t.combined_pr()).collect();
            for pr in train_prs {
                let Some(failure) = next.failed_trains.get(&pr).cloned() else {
                    continue;
                };
                let Some(idx) = next.trains.iter().position(|t| t.combined_pr() == Some(pr)) else {
                    continue;
                };
                let tasks = red_fix_tasks(
                    pr,
                    &failure.sha,
                    &failure.areas,
                    &failure.why,
                    &failure.log,
                    next.trains[idx].fixed_for.as_deref(),
                    cfg,
                );
                if !tasks.is_empty() {
                    next.trains[idx].fixed_for = Some(failure.sha.clone());
                    replace_and_queue_tasks(&mut next.todo, &mut actions, tasks);
                }
            }
        }

        LandEvent::RecordCombinedTrain {
            pr,
            branch,
            head,
            members,
            resolutions,
            left_out,
            dropped,
            screens,
            ci,
        } => {
            next.train_refused.clear();
            let member_nums: Vec<u64> = members.iter().map(|m| m.pr).collect();
            let left_reasons: Vec<(u64, String)> = left_out
                .iter()
                .map(|(n, files)| (*n, left_out_reason(files)))
                .collect();
            let drop_reasons: Vec<(u64, String)> = dropped
                .iter()
                .map(|(n, err)| (*n, dropped_line(err)))
                .collect();
            let mut unverified = vec!["screenshots were drawn on this machine's fonts".to_string()];
            if *ci {
                unverified.push(CI_NOTICE.to_string());
            }
            let pr_title = title(member_nums.len());
            let pr_body = body(&BodyParts {
                members: &member_nums,
                resolutions,
                left_out: &left_reasons,
                dropped: &drop_reasons,
                screens,
                unverified: &unverified,
            });
            actions.push(LandAction::new(
                LandActionKind::OpenPr {
                    branch: branch.clone(),
                    title: pr_title.clone(),
                    body: pr_body,
                    draft: false,
                },
                format!("open combined train PR #{pr}: {pr_title}"),
            ));
            let tasks = train_tasks(*pr, left_out, dropped, cfg);
            replace_and_queue_tasks(&mut next.todo, &mut actions, tasks);
            let record = Train::make_combined(
                *pr,
                branch.clone(),
                head.clone(),
                members.clone(),
                left_out.clone(),
                dropped.clone(),
                *ci,
            );
            next.trains.retain(|t| t.combined_pr() != Some(*pr));
            next.trains.push(record);
            actions.push(LandAction::new(
                LandActionKind::StartBatch { prs: vec![*pr] },
                format!("start verification batch for newly opened train PR #{pr}"),
            ));
        }
    }

    LandDecision { next, actions }
}

#[allow(clippy::too_many_arguments)]
fn try_mail_train(
    next: &mut LandView,
    actions: &mut Vec<LandAction>,
    idx: usize,
    pr: u64,
    head: &str,
    tree_now: &str,
    required: &BTreeSet<String>,
    cfg: &LandConfig,
) {
    let train = &next.trains[idx];
    if matches!(
        train.mail,
        MailState::Intent { .. } | MailState::Sent { .. }
    ) {
        return;
    }
    if train.reviewed.as_deref().is_none_or(|r| r.is_empty()) {
        return;
    }
    let passed = next.passed.get(&pr);
    let batch = passed.and_then(|p| next.proved_batches.get(&p.batch));
    if let Some(reason) = waiting_for(train, passed, batch, head, tree_now, required, cfg) {
        let line = format!("train #{pr} not mailed: {reason}");
        actions.push(LandAction::new(
            LandActionKind::Notify {
                once_key: String::new(),
                line: line.clone(),
            },
            line,
        ));
        return;
    }
    let members = train.numbers();
    let left_nums: Vec<u64> = train.left_out.iter().map(|(n, _)| *n).collect();
    let drop_nums: Vec<u64> = train.dropped.iter().map(|(n, _)| *n).collect();
    let ci = train.ci;
    let reviewed = train.reviewed.clone().unwrap_or_else(|| head.to_string());
    next.trains[idx].mail = MailState::Intent {
        head: reviewed.clone(),
    };
    actions.push(LandAction::new(
        LandActionKind::MarkReady { pr },
        format!("train #{pr} passed review and tree gate; mark ready before mailing"),
    ));
    let (subject, body) = letter(
        pr,
        &members,
        &left_nums,
        &drop_nums,
        ci,
        &cfg.upstream,
        &cfg.project,
        &reviewed,
    );
    actions.push(LandAction::new(
        LandActionKind::SendMail {
            pr,
            head: reviewed,
            subject,
            body,
        },
        format!("mail pinned merge command for reviewed train #{pr}"),
    ));
}

fn replace_and_queue_tasks(
    todo: &mut Vec<LandTask>,
    actions: &mut Vec<LandAction>,
    tasks: Vec<LandTask>,
) {
    let keys: BTreeSet<String> = tasks.iter().map(|t| t.key.clone()).collect();
    todo.retain(|t| !keys.contains(&t.key));
    for task in tasks {
        let why = format!("queue {} task `{}` for #{}", task.kind, task.key, task.pr);
        todo.push(task.clone());
        actions.push(LandAction::new(LandActionKind::QueueTask { task }, why));
    }
}
