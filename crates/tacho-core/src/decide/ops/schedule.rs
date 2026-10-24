//! Daemon poll decision, ordered isolated tick steps, pure step rules (PR filter, prune,
//! merges, conflicts, shape/replay trains, deploy watch, main health), and `events --since`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::tell::Fact;

/// Policy and limits for the daemon poll loop and tick steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulePolicy {
    pub poll_interval: i64,
    pub draft_recent: i64,
    pub open_limit: u32,
    pub deploy_runs_cap: usize,
    pub main_reported_cap: usize,
    pub deploy_gate: String,
    pub replay_prefix: String,
    pub max_conflict_rounds: u32,
    pub max_pr_files: usize,
    pub max_pr_lines: u64,
}

impl Default for SchedulePolicy {
    fn default() -> Self {
        Self {
            poll_interval: 60,
            draft_recent: 1200,
            open_limit: 200,
            deploy_runs_cap: 200,
            main_reported_cap: 50,
            deploy_gate: "Confirm successful main push".into(),
            replay_prefix: "fleet/replay-".into(),
            max_conflict_rounds: 2,
            max_pr_files: 25,
            max_pr_lines: 1200,
        }
    }
}

/// True only when both SHAs are non-empty (after trimming) and differ (AR6).
pub fn main_moved(last: &str, now: &str) -> bool {
    let (last, now) = (last.trim(), now.trim());
    !last.is_empty() && !now.is_empty() && last != now
}

/// Outcome of a single daemon poll iteration (R24, AR3–AR5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollDecision {
    pub run_tick: bool,
    pub new_last_main: String,
    pub new_last_tick: i64,
    pub errors: Vec<String>,
    pub why: String,
}

/// Pure daemon poll decision: isolates `watch` and `remote_main` errors and decides whether to run
/// `tick` now (on GitHub watch event, `origin/main` movement, or 60s poll interval elapsed).
pub fn poll_decision(
    last_tick: i64,
    last_main: &str,
    now: i64,
    watch: Result<bool, String>,
    remote_main: Result<String, String>,
    policy: &SchedulePolicy,
) -> PollDecision {
    let mut errors = Vec::new();
    let woke = match watch {
        Ok(b) => b,
        Err(e) => {
            errors.push(format!("watch error: {e}"));
            false
        }
    };
    let mut new_last_main = last_main.to_string();
    let moved = match remote_main {
        Ok(sha) => {
            let trimmed = sha.trim();
            if !trimmed.is_empty() {
                let m = main_moved(last_main, trimmed);
                new_last_main = trimmed.to_string();
                m
            } else {
                false
            }
        }
        Err(e) => {
            errors.push(format!("remote_main error: {e}"));
            false
        }
    };
    let due = (now - last_tick) >= policy.poll_interval;
    if woke || moved || due {
        let why = if woke {
            "github watch activity detected".into()
        } else if moved {
            "origin/main moved".into()
        } else {
            format!("poll interval ({}s) elapsed", policy.poll_interval)
        };
        PollDecision {
            run_tick: true,
            new_last_main,
            new_last_tick: now,
            errors,
            why,
        }
    } else {
        PollDecision {
            run_tick: false,
            new_last_main,
            new_last_tick: last_tick,
            errors,
            why: "no watch activity, main unchanged, poll interval not yet elapsed".into(),
        }
    }
}

/// Every step of a single `tick()` in exact execution order (R6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Step {
    Fetch,
    Prs,
    Prune,
    Recover,
    Batches,
    Reviews,
    Merges,
    Conflicts,
    Shape,
    Finish,
    Retry,
    Blocked,
    Deploy,
    MainHealth,
    Flakes,
    Dispatch,
    SelfUpdate,
    TellOwner,
    Tidy,
    Snapshot,
}

impl Step {
    pub const ALL: [Step; 20] = [
        Step::Fetch,
        Step::Prs,
        Step::Prune,
        Step::Recover,
        Step::Batches,
        Step::Reviews,
        Step::Merges,
        Step::Conflicts,
        Step::Shape,
        Step::Finish,
        Step::Retry,
        Step::Blocked,
        Step::Deploy,
        Step::MainHealth,
        Step::Flakes,
        Step::Dispatch,
        Step::SelfUpdate,
        Step::TellOwner,
        Step::Tidy,
        Step::Snapshot,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Step::Fetch => "fetch",
            Step::Prs => "prs",
            Step::Prune => "prune",
            Step::Recover => "recover",
            Step::Batches => "batches",
            Step::Reviews => "reviews",
            Step::Merges => "merges",
            Step::Conflicts => "conflicts",
            Step::Shape => "shape",
            Step::Finish => "finish",
            Step::Retry => "retry",
            Step::Blocked => "blocked",
            Step::Deploy => "deploy",
            Step::MainHealth => "main_health",
            Step::Flakes => "flakes",
            Step::Dispatch => "dispatch",
            Step::SelfUpdate => "self_update",
            Step::TellOwner => "tell_owner",
            Step::Tidy => "tidy",
            Step::Snapshot => "snapshot",
        }
    }
}

/// Runs all 20 steps in order, catching any `Err(e)` as `<step> failed: <e>` (R6).
pub fn run_steps<F: FnMut(Step) -> Result<(), String>>(mut run_one: F) -> Vec<String> {
    let mut errors = Vec::new();
    for step in Step::ALL {
        if let Err(e) = run_one(step) {
            errors.push(format!("{} failed: {e}", step.name()));
        }
    }
    errors
}

/// Parses `--since` duration (`30s`, `15m`, `2h`, `1d`) or unix seconds relative to `now` (R56).
pub fn parse_since(text: &str, now: i64) -> Result<i64, String> {
    let s = text.trim();
    let err = || format!("cannot parse --since '{text}' (use 15m, 2h, 1d, or unix seconds)");
    if s.is_empty() {
        return Err(err());
    }
    if let Some((num_str, mult)) = s
        .strip_suffix('s')
        .map(|n| (n, 1_i64))
        .or_else(|| s.strip_suffix('m').map(|n| (n, 60_i64)))
        .or_else(|| s.strip_suffix('h').map(|n| (n, 3600_i64)))
        .or_else(|| s.strip_suffix('d').map(|n| (n, 86400_i64)))
    {
        if !num_str.is_empty() && num_str.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = num_str.parse::<i64>() {
                return Ok(now - n * mult);
            }
        }
        return Err(err());
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(ts) = s.parse::<i64>() {
            return Ok(ts);
        }
    }
    Err(err())
}

/// Filters JSONL event lines by `since` cutoff or returns the last `last` non-blank lines (R56).
pub fn select_events(lines: &[String], since: Option<i64>, last: usize) -> Vec<String> {
    let non_blank: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    if let Some(cutoff) = since {
        non_blank
            .into_iter()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("ts")?.as_i64())
                    .is_some_and(|ts| ts >= cutoff)
            })
            .cloned()
            .collect()
    } else if last == 0 {
        Vec::new()
    } else {
        let start = non_blank.len().saturating_sub(last);
        non_blank[start..].iter().map(|&s| s.clone()).collect()
    }
}

/// Summary of an open PR observed from GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub branch: String,
    pub base_ref: String,
    pub is_draft: bool,
    pub updated_at: i64,
    pub mergeable: String,
    pub review_state: String,
    pub checks_state: String,
    pub head_sha: String,
    pub files: Vec<String>,
    pub additions: u64,
    pub deletions: u64,
    pub issue: Option<u64>,
    pub author: String,
}

/// Mode of a replay train (synthetic single branch vs native stacked PRs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReplayMode {
    #[default]
    Synthetic,
    Native,
}

/// Lifecycle state of a replay train.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReplayState {
    #[default]
    Running,
    Ready,
    Held,
    Done,
    Abandoned,
}

/// A tracked replay train in state (R11, R12, RP1–RP15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplayTrain {
    pub key: String,
    pub prs: Vec<u64>,
    pub branches: Vec<String>,
    pub issue: Option<u64>,
    pub mode: ReplayMode,
    pub replay_branch: String,
    pub head_pr: Option<u64>,
    pub cursor: usize,
    pub state: ReplayState,
    pub reason: String,
    pub conflict_branch: Option<String>,
    pub conflict_base: Option<String>,
    pub conflict_rounds: BTreeMap<String, u32>,
}

impl ReplayTrain {
    pub fn is_active(&self) -> bool {
        !matches!(self.state, ReplayState::Done | ReplayState::Abandoned)
    }
}

/// Filters open PRs (R2, RP7, RP8).
pub fn filter_open_prs(
    raw: &[PrSummary],
    now: i64,
    tracked_prs: &[u64],
    tracked_branches: &[String],
    replays: &[ReplayTrain],
    policy: &SchedulePolicy,
) -> Vec<PrSummary> {
    raw.iter()
        .filter(|pr| {
            !pr.is_draft
                || now - pr.updated_at <= policy.draft_recent
                || tracked_prs.contains(&pr.number)
                || tracked_branches.contains(&pr.branch)
                || replays.iter().any(|r| {
                    r.is_active()
                        && (r.head_pr == Some(pr.number)
                            || (r.mode == ReplayMode::Native && r.prs.contains(&pr.number)))
                })
        })
        .cloned()
        .collect()
}

/// Prunes closed PRs from `known_prs`, `held_prs`, and `conflict_keys` only when `prs_seen_all`
/// is true (R4, RP8). Returns `(pruned_pr_numbers, pruned_conflict_keys)`.
pub fn prune_stale_ids(
    prs_seen_all: bool,
    open_prs: &[PrSummary],
    known_prs: &[u64],
    held_prs: &[u64],
    conflict_keys: &[String],
    replays: &[ReplayTrain],
) -> (Vec<u64>, Vec<String>) {
    if !prs_seen_all {
        return (Vec::new(), Vec::new());
    }
    let open_nums: BTreeSet<u64> = open_prs.iter().map(|p| p.number).collect();
    let open_branches: BTreeSet<&str> = open_prs.iter().map(|p| p.branch.as_str()).collect();
    let prot_prs: BTreeSet<u64> = replays
        .iter()
        .filter(|r| r.is_active())
        .flat_map(|r| r.prs.iter().copied().chain(r.head_pr))
        .collect();
    let prot_branches: BTreeSet<&str> = replays
        .iter()
        .filter(|r| r.is_active())
        .flat_map(|r| {
            r.branches
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(r.replay_branch.as_str()))
        })
        .collect();

    let mut pruned_prs: Vec<u64> = known_prs
        .iter()
        .chain(held_prs.iter())
        .copied()
        .filter(|n| !open_nums.contains(n) && !prot_prs.contains(n))
        .collect();
    pruned_prs.sort_unstable();
    pruned_prs.dedup();

    let pruned_conflicts: Vec<String> = conflict_keys
        .iter()
        .filter(|k| match k.parse::<u64>() {
            Ok(num) => !open_nums.contains(&num) && !prot_prs.contains(&num),
            Err(_) => !open_branches.contains(k.as_str()) && !prot_branches.contains(k.as_str()),
        })
        .cloned()
        .collect();

    (pruned_prs, pruned_conflicts)
}

/// A workflow run observed on `deploy.yml` (R16, DP1–DP5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployRunObs {
    pub id: u64,
    pub status: String,
    pub conclusion: String,
    pub title: String,
    pub job_conclusions: BTreeMap<String, String>,
}

/// Pure decision from [`deploy_step`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeployStepOutcome {
    pub newly_seen: Vec<u64>,
    pub clear_pending_deploy_fixes: bool,
    pub spawn_fix_for: Option<(u64, String)>,
    pub emit_failed_events: Vec<(u64, String)>,
    pub tell_facts: Vec<Fact>,
}

/// Evaluates `deploy.yml` runs (R16, EV2, EV3, DP1–DP5).
pub fn deploy_step(
    runs: &[DeployRunObs],
    seen: &BTreeSet<u64>,
    has_active_or_pending_deploy_fix: bool,
    policy: &SchedulePolicy,
) -> DeployStepOutcome {
    let mut out = DeployStepOutcome::default();
    let mut local_fix_busy = has_active_or_pending_deploy_fix;

    for run in runs {
        if seen.contains(&run.id) || out.newly_seen.contains(&run.id) || run.status != "completed" {
            continue;
        }
        out.newly_seen.push(run.id);
        let gate_passed = run
            .job_conclusions
            .get(&policy.deploy_gate)
            .is_some_and(|c| c == "success");
        if run.conclusion == "success" {
            if gate_passed || run.job_conclusions.is_empty() {
                out.clear_pending_deploy_fixes = true;
                local_fix_busy = false;
            }
        } else if run.conclusion == "failure" && gate_passed {
            out.emit_failed_events.push((run.id, run.title.clone()));
            if !local_fix_busy && out.spawn_fix_for.is_none() {
                out.spawn_fix_for = Some((run.id, run.title.clone()));
                local_fix_busy = true;
            } else {
                out.tell_facts.push(Fact::DeployDone {
                    run: run.id,
                    sha: String::new(),
                    ok: false,
                    what: run.title.clone(),
                });
            }
        }
    }
    out
}

/// Observation of the latest completed `ci.yml` run on `main` (R17, MH1, MH2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MainHealthObs {
    pub id: u64,
    pub status: String,
    pub conclusion: String,
    pub title: String,
}

/// Pure decision from [`main_health_step`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MainHealthOutcome {
    pub newly_reported: Vec<u64>,
    pub clear_pending_main_fixes: bool,
    pub spawn_fix_for: Option<(u64, String)>,
    pub tell_facts: Vec<Fact>,
}

/// Evaluates `ci.yml` on `main` (R17, MH1, MH2).
pub fn main_health_step(
    latest: Option<&MainHealthObs>,
    reported: &[u64],
    has_active_or_pending_main_fix: bool,
) -> MainHealthOutcome {
    let mut out = MainHealthOutcome::default();
    let Some(run) = latest.filter(|r| r.status == "completed") else {
        return out;
    };
    if run.conclusion == "success" {
        out.clear_pending_main_fixes = true;
    } else if run.conclusion == "failure" && !reported.contains(&run.id) {
        out.newly_reported.push(run.id);
        if !has_active_or_pending_main_fix {
            out.spawn_fix_for = Some((run.id, run.title.clone()));
        } else {
            out.tell_facts.push(Fact::MainJobFailed {
                run: run.id,
                job: run.title.clone(),
                sha: String::new(),
                first_line: run.title.clone(),
                url: String::new(),
            });
        }
    }
    out
}

/// Caps a reported-IDs list to `cap` most-recent entries.
pub fn cap_ids(mut ids: Vec<u64>, cap: usize) -> Vec<u64> {
    if ids.len() > cap {
        ids.drain(0..(ids.len() - cap));
    }
    ids
}

/// Detects PR trains (stacked `base_ref -> branch` chains or PRs sharing an issue) (R11, RP1–RP4).
pub fn find_trains(open_prs: &[PrSummary], existing: &[ReplayTrain]) -> Vec<Vec<u64>> {
    let tracked: BTreeSet<u64> = existing
        .iter()
        .filter(|r| r.is_active())
        .flat_map(|r| r.prs.iter().copied().chain(r.head_pr))
        .collect();
    let eligible: Vec<&PrSummary> = open_prs
        .iter()
        .filter(|p| !tracked.contains(&p.number))
        .collect();
    let by_branch: BTreeMap<&str, &PrSummary> =
        eligible.iter().map(|p| (p.branch.as_str(), *p)).collect();
    let mut children: BTreeMap<&str, Vec<&PrSummary>> = BTreeMap::new();
    for p in &eligible {
        children.entry(p.base_ref.as_str()).or_default().push(*p);
    }
    for list in children.values_mut() {
        list.sort_by_key(|p| p.number);
    }

    let mut used = BTreeSet::new();
    let mut trains = Vec::new();
    for root in &eligible {
        if by_branch.contains_key(root.base_ref.as_str()) || used.contains(&root.number) {
            continue;
        }
        let mut chain = vec![root.number];
        let mut cur = root.branch.as_str();
        while let Some(next) = children
            .get(cur)
            .and_then(|kids| kids.iter().find(|k| !used.contains(&k.number)))
        {
            chain.push(next.number);
            cur = next.branch.as_str();
        }
        if chain.len() >= 2 {
            used.extend(chain.iter().copied());
            trains.push(chain);
        }
    }
    let mut by_issue: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for p in &eligible {
        if !used.contains(&p.number) {
            if let Some(iss) = p.issue {
                by_issue.entry(iss).or_default().push(p.number);
            }
        }
    }
    for (_, mut nums) in by_issue {
        nums.sort_unstable();
        if nums.len() >= 2 {
            trains.push(nums);
        }
    }
    trains
}

/// Identifies oversized PRs (`files > max_pr_files` or `additions + deletions > max_pr_lines`) (RP5, RP6).
pub fn find_oversized(
    open_prs: &[PrSummary],
    held_oversized: &BTreeSet<u64>,
    existing: &[ReplayTrain],
    policy: &SchedulePolicy,
) -> Vec<u64> {
    let tracked: BTreeSet<u64> = existing
        .iter()
        .filter(|r| r.is_active())
        .flat_map(|r| r.prs.iter().copied().chain(r.head_pr))
        .collect();
    open_prs
        .iter()
        .filter(|p| !held_oversized.contains(&p.number) && !tracked.contains(&p.number))
        .filter(|p| {
            p.files.len() > policy.max_pr_files || p.additions + p.deletions > policy.max_pr_lines
        })
        .map(|p| p.number)
        .collect()
}

/// True when closing `issue` upon `merged_pr` merging is safe (R9, RP9).
pub fn should_close_issue_on_merge(
    merged_pr: u64,
    issue: u64,
    open_prs: &[PrSummary],
    replays: &[ReplayTrain],
) -> bool {
    let other_pr = open_prs
        .iter()
        .any(|p| p.number != merged_pr && p.issue == Some(issue));
    let active_train = replays.iter().any(|r| {
        r.is_active()
            && r.head_pr != Some(merged_pr)
            && (r.issue == Some(issue) || r.prs.contains(&merged_pr))
    });
    !other_pr && !active_train
}

/// Pure decision for a conflicting PR in the `conflicts` step (R10, RB1–RB5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictAction {
    SpawnRebase { pr: u64, branch: String, round: u32 },
    HoldExhausted { pr: u64, reason: String },
}

/// Decides conflict-rebase actions for `CONFLICTING` PRs (R10, RB1–RB5).
pub fn decide_conflicts(
    open_prs: &[PrSummary],
    replays: &[ReplayTrain],
    held_prs: &BTreeMap<u64, String>,
    active_rebase_prs: &BTreeSet<u64>,
    conflicts_resolved: &mut BTreeMap<String, u32>,
    policy: &SchedulePolicy,
) -> Vec<ConflictAction> {
    let in_replay: BTreeSet<u64> = replays
        .iter()
        .filter(|r| r.is_active())
        .flat_map(|r| r.prs.iter().copied().chain(r.head_pr))
        .collect();
    let mut actions = Vec::new();
    for pr in open_prs {
        if in_replay.contains(&pr.number)
            || held_prs.contains_key(&pr.number)
            || active_rebase_prs.contains(&pr.number)
        {
            continue;
        }
        let key = pr.number.to_string();
        if pr.mergeable == "CONFLICTING" {
            let count = conflicts_resolved.get(&key).copied().unwrap_or(0);
            if count < policy.max_conflict_rounds {
                let next = count + 1;
                conflicts_resolved.insert(key, next);
                actions.push(ConflictAction::SpawnRebase {
                    pr: pr.number,
                    branch: pr.branch.clone(),
                    round: next,
                });
            } else {
                actions.push(ConflictAction::HoldExhausted {
                    pr: pr.number,
                    reason: "conflict resolution rounds exhausted".into(),
                });
            }
        } else if pr.mergeable == "MERGEABLE" {
            conflicts_resolved.remove(&key);
        }
    }
    actions
}
