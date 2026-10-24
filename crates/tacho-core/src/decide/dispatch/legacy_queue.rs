//! Legacy area gate and queue selection helpers.
#![allow(unused_imports)]

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::*;

/// Evaluate whether a new task in `area` may start, and which PR it should stack on.
#[allow(clippy::too_many_arguments)]
pub fn area_gate(
    area: &str,
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    live_keys: &BTreeSet<String>,
    passed_prs: &BTreeSet<u64>,
    failed_prs: &BTreeSet<u64>,
    merged_prs: &BTreeSet<u64>,
    closed_prs: &BTreeSet<u64>,
) -> AreaGate {
    if area.trim().is_empty() {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: "no area".to_string(),
        };
    }
    let mut area_runs: Vec<&AgentRun> = agents
        .values()
        .filter(|a| {
            !retired.contains(&a.key) && !retired.contains(base_key(&a.key)) && a.area == area
        })
        .collect();
    if area_runs.is_empty() {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: "clean".to_string(),
        };
    }
    for a in &area_runs {
        if live_keys.contains(&a.key) {
            return AreaGate {
                ok: false,
                base_pr: None,
                reason: format!("{} is running", a.key),
            };
        }
    }
    area_runs.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| attempt(&a.key).cmp(&attempt(&b.key)))
            .then_with(|| a.key.cmp(&b.key))
    });
    let last = area_runs.last().copied().unwrap();
    if let Some(prob) = pr_problem(last, None) {
        return AreaGate {
            ok: false,
            base_pr: None,
            reason: prob,
        };
    }
    if last.pr_none && last.exit_code == Some(0) {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: format!("{} finished with no PR", last.key),
        };
    }
    let Some(pr) = recorded_pr(last) else {
        if attempt(&last.key) < MAX_RETRIES {
            return AreaGate {
                ok: false,
                base_pr: None,
                reason: format!("{} waiting to resume", last.key),
            };
        }
        return AreaGate {
            ok: false,
            base_pr: None,
            reason: format!("{} stopped without a PR", last.key),
        };
    };
    if merged_prs.contains(&pr) {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: format!("PR #{pr} merged"),
        };
    }
    if closed_prs.contains(&pr) {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: format!("PR #{pr} closed"),
        };
    }
    if failed_prs.contains(&pr) {
        return AreaGate {
            ok: false,
            base_pr: Some(pr),
            reason: format!("PR #{pr} failed verification"),
        };
    }
    if passed_prs.contains(&pr) {
        return AreaGate {
            ok: true,
            base_pr: Some(pr),
            reason: format!("stacked on passed PR #{pr}"),
        };
    }
    AreaGate {
        ok: false,
        base_pr: Some(pr),
        reason: format!("waiting for PR #{pr} to pass batch verification"),
    }
}

/// Apply `base_pr` stacking to a `TodoItem` when its `base` is not already pinned.
pub fn stacked_task(item: &TodoItem, base_pr: Option<u64>) -> TodoItem {
    let mut out = item.clone();
    if out.base.is_none() {
        if let Some(pr) = base_pr {
            out.base = Some(pr.to_string());
        }
    }
    out
}

/// Build an [`AreaReport`] for every area present across `agents` and `todo`.
#[allow(clippy::too_many_arguments)]
pub fn area_report(
    agents: &BTreeMap<String, AgentRun>,
    todo: &[TodoItem],
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    passed_prs: &BTreeSet<u64>,
    failed_prs: &BTreeSet<u64>,
    merged_prs: &BTreeSet<u64>,
    closed_prs: &BTreeSet<u64>,
) -> Vec<AreaReport> {
    let mut areas = BTreeSet::new();
    for a in agents.values() {
        if !retired.contains(&a.key) && !a.area.is_empty() {
            areas.insert(a.area.clone());
        }
    }
    for t in todo {
        if !retired.contains(&t.key) && !t.area.is_empty() {
            areas.insert(t.area.clone());
        }
    }
    let mut out = Vec::new();
    for area in areas {
        let hold_key = format!("area:{area}");
        if let Some(h) = holds.get(&hold_key) {
            out.push(AreaReport {
                area,
                state: "held".to_string(),
                base_pr: None,
                detail: h.reason.clone(),
                last_key: None,
            });
            continue;
        }
        let gate = area_gate(
            &area, agents, retired, live_keys, passed_prs, failed_prs, merged_prs, closed_prs,
        );
        let last_key = agents
            .values()
            .filter(|a| !retired.contains(&a.key) && a.area == area)
            .max_by_key(|a| (a.started_at, attempt(&a.key)))
            .map(|a| a.key.clone());
        let state = if gate.ok {
            "ready"
        } else if gate.reason.contains("is running") {
            "running"
        } else if gate.reason.contains("waiting for PR")
            || gate.reason.contains("waiting to resume")
        {
            "verifying"
        } else {
            "blocked"
        };
        out.push(AreaReport {
            area,
            state: state.to_string(),
            base_pr: gate.base_pr,
            detail: gate.reason,
            last_key,
        });
    }
    out
}

/// Select up to `free_slots` ready tasks from `todo` (plus any due automatic resumes).
#[allow(clippy::too_many_arguments)]
pub fn next_work(
    todo: &[TodoItem],
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    passed_prs: &BTreeSet<u64>,
    failed_prs: &BTreeSet<u64>,
    merged_prs: &BTreeSet<u64>,
    closed_prs: &BTreeSet<u64>,
    free_slots: usize,
) -> (Vec<TodoItem>, Vec<TodoItem>) {
    if free_slots == 0 {
        return (Vec::new(), todo.to_vec());
    }
    let done = done_keys(agents, retired);
    let mut indexed: Vec<(usize, &TodoItem)> = todo.iter().enumerate().collect();
    indexed.sort_by(|(ia, a), (ib, b)| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| ia.cmp(ib))
    });

    let mut chosen = Vec::new();
    let mut chosen_indices = HashSet::new();
    let mut claimed_areas = HashSet::new();

    for (idx, item) in indexed {
        if chosen.len() >= free_slots {
            break;
        }
        let root = base_key(&item.key);
        if retired.contains(&item.key) || retired.contains(root) {
            chosen_indices.insert(idx);
            continue;
        }
        if holds.contains_key(&item.key) || holds.contains_key(root) {
            continue;
        }
        if !item.area.is_empty() && holds.contains_key(&format!("area:{}", item.area)) {
            continue;
        }
        if live_keys.contains(&item.key) || live_keys.contains(root) {
            continue;
        }
        if !closed_after(item, closed_prs).is_empty() {
            continue;
        }
        if !after_met(item, &done, passed_prs, merged_prs) {
            continue;
        }
        let mut base_pr = None;
        if !item.area.is_empty() {
            if claimed_areas.contains(&item.area) {
                continue;
            }
            let gate = area_gate(
                &item.area, agents, retired, live_keys, passed_prs, failed_prs, merged_prs,
                closed_prs,
            );
            if !gate.ok {
                continue;
            }
            base_pr = gate.base_pr;
            claimed_areas.insert(item.area.clone());
        }
        chosen.push(stacked_task(item, base_pr));
        chosen_indices.insert(idx);
    }

    let remaining = todo
        .iter()
        .enumerate()
        .filter(|(i, _)| !chosen_indices.contains(i))
        .map(|(_, t)| t.clone())
        .collect();
    (chosen, remaining)
}

/// Explain why a specific queued task can or cannot start right now.
#[allow(clippy::too_many_arguments)]
pub fn may_start(
    item: &TodoItem,
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    passed_prs: &BTreeSet<u64>,
    failed_prs: &BTreeSet<u64>,
    merged_prs: &BTreeSet<u64>,
    closed_prs: &BTreeSet<u64>,
) -> (bool, String) {
    let root = base_key(&item.key);
    if retired.contains(&item.key) || retired.contains(root) {
        return (false, format!("{} is retired", item.key));
    }
    if let Some(h) = holds.get(&item.key).or_else(|| holds.get(root)) {
        return (false, format!("held: {}", h.reason));
    }
    if !item.area.is_empty() {
        if let Some(h) = holds.get(&format!("area:{}", item.area)) {
            return (false, format!("area:{} held: {}", item.area, h.reason));
        }
    }
    let closed = closed_after(item, closed_prs);
    if !closed.is_empty() {
        return (
            false,
            format!("prerequisite PR {:?} closed unmerged", closed),
        );
    }
    let done = done_keys(agents, retired);
    if !after_met(item, &done, passed_prs, merged_prs) {
        return (false, format!("waiting on after={:?}", item.after));
    }
    if !item.area.is_empty() {
        let gate = area_gate(
            &item.area, agents, retired, live_keys, passed_prs, failed_prs, merged_prs, closed_prs,
        );
        if !gate.ok {
            return (false, gate.reason);
        }
        return (true, gate.reason);
    }
    (true, "ready".to_string())
}
