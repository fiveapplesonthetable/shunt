//! Holds, `--after` conditions, and task retirement (R16–R27).

use std::collections::{BTreeMap, BTreeSet};

use super::config::DispatchConfig;
use super::model::{
    Hold, Holds, PrRecord, PrStanding, RetiredRecord, RunKey, RunStatus, Runs, TodoItem,
};

/// The PR number an `--after` entry names with `pr:<n>`, or `None` when it names a task key.
pub fn pr_waited_on(entry: &str) -> Option<u64> {
    let n = entry.strip_prefix("pr:")?;
    if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
        n.parse::<u64>().ok().filter(|&v| v > 0)
    } else {
        None
    }
}

/// `None` when `--after` can use this entry, else the sentence saying what is wrong with it (R16).
pub fn after_problem(entry: &str) -> Option<String> {
    if entry.starts_with("pr:") && pr_waited_on(entry).is_none() {
        Some(format!(
            "'{entry}' is not a pull request: write pr:<number>, e.g. pr:954"
        ))
    } else {
        None
    }
}

/// Every task finished and every `pr:<n>` merged (`Passed` does NOT satisfy `pr:<n>`) (R17).
pub fn after_met(
    after: &[String],
    done: &BTreeSet<String>,
    standing: &BTreeMap<u64, PrStanding>,
) -> bool {
    after.iter().all(|entry| match pr_waited_on(entry) {
        Some(n) => standing.get(&n) == Some(&PrStanding::Merged),
        None => done.contains(entry),
    })
}

/// The first `pr:<n>` in `--after` whose PR was closed without landing, or `None` (R18).
pub fn closed_after(after: &[String], standing: &BTreeMap<u64, PrStanding>) -> Option<u64> {
    after.iter().find_map(|entry| {
        let n = pr_waited_on(entry)?;
        (standing.get(&n) == Some(&PrStanding::Closed)).then_some(n)
    })
}

/// The PRs that queued tasks wait on with `pr:<n>`, so their standing is looked up (R19).
pub fn pending_prs(todo: &[TodoItem]) -> BTreeSet<u64> {
    todo.iter()
        .flat_map(|t| t.after.iter().filter_map(|e| pr_waited_on(e)))
        .collect()
}

/// All PR numbers whose standing must be looked up for dispatch: `pending_prs(todo)` plus every
/// recorded PR of a run that has an area or whose base key is a queued area item (R19).
pub fn standings_to_lookup(todo: &[TodoItem], runs: &Runs) -> BTreeSet<u64> {
    let mut out = pending_prs(todo);
    let in_area: BTreeSet<&str> = todo
        .iter()
        .filter(|t| !t.area.is_empty())
        .map(|t| t.key.as_str())
        .collect();
    for (key, run) in runs {
        if let PrRecord::Found(pr) = &run.pr {
            let base = RunKey::parse(key).base;
            if !run.area.is_empty() || in_area.contains(base.as_str()) {
                out.insert(pr.number);
            }
        }
    }
    out
}

/// A task starts when it is not held and what it is after has happened (R20).
pub fn may_start(
    item: &TodoItem,
    done: &BTreeSet<String>,
    standing: &BTreeMap<u64, PrStanding>,
    holds: &Holds,
) -> bool {
    !holds.contains_key(&item.key) && after_met(&item.after, done, standing)
}

fn is_hands_target(target: &str, cfg: &DispatchConfig) -> bool {
    let base = match target.rsplit_once('~') {
        Some((b, digits)) => {
            if b.is_empty() || digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
                return false;
            }
            b
        }
        None => target,
    };
    let Some(n) = base.strip_prefix(&cfg.hands_prefix) else {
        return false;
    };
    !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())
}

/// `None` when `target` can be held, else what to fix (R21).
pub fn hold_problem(
    target: &str,
    why: &str,
    todo: &[TodoItem],
    holds: &Holds,
    runs: &Runs,
    cfg: &DispatchConfig,
) -> Option<String> {
    if why.trim().is_empty() {
        return Some(
            "say why with --why, so whoever reads status knows when to release it".to_string(),
        );
    }
    if let Some(h) = holds.get(target) {
        return Some(format!("{target} is already held: {}", h.why));
    }
    let in_groups = cfg.groups.iter().any(|g| g == target);
    let in_todo = todo.iter().any(|t| t.key == target);
    let is_running = runs
        .get(target)
        .is_some_and(|r| r.status == RunStatus::Running);
    if !in_groups && !in_todo && !is_running && !is_hands_target(target, cfg) {
        return Some(format!(
            "{target} is neither a queued task, a running agent, a {}<pr> rebase nor a batch group ({})",
            cfg.hands_prefix,
            cfg.groups.join(", ")
        ));
    }
    None
}

/// `None` when `target` is held, else the sentence saying it is not (R22).
pub fn release_problem(target: &str, holds: &Holds) -> Option<String> {
    if holds.contains_key(target) {
        None
    } else {
        Some(format!("{target} is not held"))
    }
}

/// `holds` with `target` held for `why` since `now` (R22).
pub fn with_hold(holds: &Holds, target: &str, why: &str, now: i64) -> Holds {
    let mut out = holds.clone();
    out.insert(
        target.to_string(),
        Hold {
            why: why.trim().to_string(),
            since: now,
        },
    );
    out
}

/// `holds` with `target` released (R22).
pub fn without_hold(holds: &Holds, target: &str) -> Holds {
    let mut out = holds.clone();
    out.remove(target);
    out
}

/// The batch groups whose batches must not start (R23).
pub fn held_groups(holds: &Holds, cfg: &DispatchConfig) -> BTreeSet<String> {
    holds
        .keys()
        .filter(|k| cfg.groups.iter().any(|g| g == *k))
        .cloned()
        .collect()
}

/// One line per hold, oldest first: `"{k:24} held {minutes} min: {why}"` (R23).
pub fn hold_lines(holds: &Holds, now: i64) -> Vec<String> {
    let mut ordered: Vec<(&String, &Hold)> = holds.iter().collect();
    ordered.sort_by_key(|(_, h)| h.since);
    ordered
        .into_iter()
        .map(|(k, h)| {
            let mins = (now - h.since).max(0) / 60;
            format!("{k:24} held {mins} min: {}", h.why)
        })
        .collect()
}

/// Keys of queued tasks held for strictly over `stale_hold_secs` (24 h), oldest first (R24).
pub fn stale_holds(
    holds: &Holds,
    todo: &[TodoItem],
    now: i64,
    cfg: &DispatchConfig,
) -> Vec<String> {
    let queued: BTreeSet<&str> = todo.iter().map(|t| t.key.as_str()).collect();
    let mut ordered: Vec<(&String, &Hold)> = holds
        .iter()
        .filter(|(k, h)| queued.contains(k.as_str()) && now - h.since > cfg.stale_hold_secs)
        .collect();
    ordered.sort_by_key(|(_, h)| h.since);
    ordered.into_iter().map(|(k, _)| k.clone()).collect()
}

/// `None` when `key` can be retired, else what to fix (R25).
pub fn retire_problem(
    key: &str,
    why: &str,
    todo: &[TodoItem],
    runs: &Runs,
    cfg: &DispatchConfig,
) -> Option<String> {
    if why.trim().is_empty() {
        return Some(
            "say why with --why, so the retired list records what made the work unnecessary"
                .to_string(),
        );
    }
    if !todo.iter().any(|t| t.key == key) {
        return Some(format!("{key} is not a queued task"));
    }
    if runs
        .get(key)
        .is_some_and(|r| r.status == RunStatus::Running)
    {
        return Some(format!(
            "{key} is running; stop it with {} kill first",
            cfg.tool
        ));
    }
    None
}

/// `(todo, holds, retired)` with `key` removed from the queue and its hold, recorded in `retired`,
/// and stripped from every other item's `after` (R26).
pub fn retire(
    todo: &[TodoItem],
    holds: &Holds,
    retired: &BTreeMap<String, RetiredRecord>,
    key: &str,
    why: &str,
    now: i64,
) -> (Vec<TodoItem>, Holds, BTreeMap<String, RetiredRecord>) {
    let item = todo.iter().find(|t| t.key == key);
    let mut new_retired = retired.clone();
    if let Some(item) = item {
        new_retired.insert(
            key.to_string(),
            RetiredRecord {
                why: why.trim().to_string(),
                at: now,
                task: item.task.clone(),
                held_for: holds.get(key).map(|h| h.why.clone()).unwrap_or_default(),
            },
        );
    }
    let rest = todo
        .iter()
        .filter(|t| t.key != key)
        .map(|t| {
            let mut copy = t.clone();
            copy.after.retain(|e| e != key);
            copy
        })
        .collect();
    (rest, without_hold(holds, key), new_retired)
}

/// One line per retired task, newest first: `"{k:28} retired: {why}"` (R26).
pub fn retired_lines(retired: &BTreeMap<String, RetiredRecord>) -> Vec<String> {
    let mut ordered: Vec<(&String, &RetiredRecord)> = retired.iter().collect();
    ordered.sort_by_key(|b| std::cmp::Reverse(b.1.at));
    ordered
        .into_iter()
        .map(|(k, r)| format!("{k:28} retired: {}", r.why))
        .collect()
}
