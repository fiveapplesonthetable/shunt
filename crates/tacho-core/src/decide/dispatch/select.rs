//! Choosing what starts next from the queue, area serialization, and area reporting (R15, R28–R34, R40).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::config::DispatchConfig;
use super::holds::may_start;
use super::model::{Base, Gate, Holds, Mergeable, PrStanding, RunState, Runs, TodoItem};
use super::report::{best_pr, pr_problem};
use super::runs::{done_keys, run_state};

/// Where a PR stands for the work queue: `merged`, `closed` (without landing), `passed` (its
/// current head passed a batch), or `waiting` (R15).
pub fn pr_standing(head: &str, passed_sha: Option<&str>, merged: bool, closed: bool) -> PrStanding {
    if merged {
        return PrStanding::Merged;
    }
    if closed {
        return PrStanding::Closed;
    }
    if !head.is_empty() && passed_sha == Some(head) {
        PrStanding::Passed
    } else {
        PrStanding::Waiting
    }
}

/// GitHub's verdict on whether a PR merges; `Unknown` keeps the previous answer (R40).
pub fn is_conflicting(mergeable: Mergeable, was: bool) -> bool {
    match mergeable {
        Mergeable::Conflicting => true,
        Mergeable::Mergeable => false,
        Mergeable::Unknown => was,
    }
}

/// String-input convenience wrapper for `is_conflicting` (`"CONFLICTING"`, `"MERGEABLE"`, `"UNKNOWN"`).
pub fn is_conflicting_str(mergeable: &str, was: bool) -> bool {
    let m = if mergeable.eq_ignore_ascii_case("CONFLICTING") {
        Mergeable::Conflicting
    } else if mergeable.eq_ignore_ascii_case("MERGEABLE") {
        Mergeable::Mergeable
    } else {
        Mergeable::Unknown
    };
    is_conflicting(m, was)
}

/// Whether an item at `idx` in `todo` with an area may start (R30, R31).
///
/// The predecessor is the last earlier **non-`now`** queue item in the same area, else
/// `area_heads[area]`.
pub fn area_gate(
    idx: usize,
    todo: &[TodoItem],
    runs: &Runs,
    standing: &BTreeMap<u64, PrStanding>,
    area_heads: &BTreeMap<String, String>,
    cfg: &DispatchConfig,
) -> Gate {
    let item = &todo[idx];
    let prev = todo[..idx]
        .iter()
        .rev()
        .find(|t| t.area == item.area && !t.now)
        .map(|t| t.key.as_str())
        .or_else(|| area_heads.get(&item.area).map(String::as_str));
    let Some(prev) = prev else {
        return Gate::Go {
            base: Base::Main,
            skipped: None,
        };
    };
    match run_state(prev, runs, cfg) {
        RunState::Queued => {
            return Gate::Wait {
                why: format!("behind {prev}, which has not started"),
                person: false,
            };
        }
        RunState::Running => {
            return Gate::Wait {
                why: format!("waiting for {prev} to finish"),
                person: false,
            };
        }
        RunState::Resuming => {
            return Gate::Wait {
                why: format!("waiting for {prev} to be resumed"),
                person: false,
            };
        }
        RunState::Stuck => {
            return Gate::Wait {
                why: format!("{prev} did not finish; a person decides"),
                person: true,
            };
        }
        RunState::Done => {}
    }
    if let Some((why, person)) = pr_problem(prev, runs, cfg) {
        return Gate::Wait { why, person };
    }
    let Some(pr) = best_pr(prev, runs) else {
        return Gate::Go {
            base: Base::Main,
            skipped: None,
        };
    };
    match standing
        .get(&pr.number)
        .copied()
        .unwrap_or(PrStanding::Waiting)
    {
        PrStanding::Merged => Gate::Go {
            base: Base::Main,
            skipped: None,
        },
        PrStanding::Passed => Gate::Go {
            base: Base::Pr {
                number: pr.number,
                branch: pr.branch,
            },
            skipped: None,
        },
        PrStanding::Closed => Gate::Go {
            base: Base::Main,
            skipped: Some(pr.number),
        },
        PrStanding::Waiting => Gate::Wait {
            why: format!("waiting on #{} to pass a batch", pr.number),
            person: false,
        },
    }
}

/// A queued item chosen by [`next_work`], along with its area `base` and any `skipped` closed PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickedWork {
    pub item: TodoItem,
    pub base: Option<Base>,
    pub skipped: Option<u64>,
}

/// The queued task to start now, or `None`: urgent (`now`) items first (ignoring `limit` and area
/// lanes), then oldest first whose prerequisites finished while `running < limit` (R29, R31).
#[allow(clippy::too_many_arguments)]
pub fn next_work(
    todo: &[TodoItem],
    runs: &Runs,
    running: u32,
    limit: i32,
    busy: bool,
    standing: &BTreeMap<u64, PrStanding>,
    holds: &Holds,
    area_heads: &BTreeMap<String, String>,
    cfg: &DispatchConfig,
) -> Option<PickedWork> {
    if busy {
        return None;
    }
    let done = done_keys(runs);
    for item in todo {
        if item.now && !runs.contains_key(&item.key) && may_start(item, &done, standing, holds) {
            return Some(PickedWork {
                item: item.clone(),
                base: if item.area.is_empty() {
                    None
                } else {
                    Some(Base::Main)
                },
                skipped: None,
            });
        }
    }
    if limit <= 0 || running as i32 >= limit {
        return None;
    }
    for (idx, item) in todo.iter().enumerate() {
        if runs.contains_key(&item.key) || !may_start(item, &done, standing, holds) {
            continue;
        }
        if item.area.is_empty() {
            return Some(PickedWork {
                item: item.clone(),
                base: None,
                skipped: None,
            });
        }
        if let Gate::Go { base, skipped } = area_gate(idx, todo, runs, standing, area_heads, cfg) {
            return Some(PickedWork {
                item: item.clone(),
                base: Some(base),
                skipped,
            });
        }
    }
    None
}

/// The item's brief, followed by what to build on when it has an area (R32).
pub fn stacked_task(item: &TodoItem, base: Option<&Base>, cfg: &DispatchConfig) -> String {
    let Some(base) = base else {
        return item.task.clone();
    };
    let tool = &cfg.tool;
    let how = match base {
        Base::Main => format!("Branch from upstream/main (`{tool} start <slug>`)."),
        Base::Pr { number, branch } => format!(
            "Build on PR #{number}, which has passed its batch but has not merged: \
             `{tool} start <slug> --base {number}` (its branch is {branch}). Open your PR against \
             main and say in its body that it stacks on #{number}."
        ),
    };
    format!(
        "{}\n\nArea {}: one agent at a time works here. {how}",
        item.task, item.area
    )
}

/// News line when an item starts from main because its predecessor's PR was closed (R33).
pub fn skipped_news(key: &str, skipped_pr: u64) -> String {
    format!(
        "{key} starts from main: #{skipped_pr}, which its area waited on, was closed without landing"
    )
}

/// For each area, one line per item in queue order: what is running, what waits and on what (R34).
pub fn area_report(
    todo: &[TodoItem],
    runs: &Runs,
    standing: &BTreeMap<u64, PrStanding>,
    area_heads: &BTreeMap<String, String>,
    cfg: &DispatchConfig,
) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (idx, item) in todo.iter().enumerate() {
        if item.area.is_empty() {
            continue;
        }
        let key = &item.key;
        let state = run_state(key, runs, cfg);
        let line = match state {
            RunState::Queued => match area_gate(idx, todo, runs, standing, area_heads, cfg) {
                Gate::Go { .. } => "queued, ready to start".to_string(),
                Gate::Wait { why, .. } => format!("queued, {why}"),
            },
            RunState::Done => {
                if let Some((problem, _)) = pr_problem(key, runs, cfg) {
                    format!("done, {problem}")
                } else if let Some(pr) = best_pr(key, runs) {
                    let st = standing
                        .get(&pr.number)
                        .copied()
                        .unwrap_or(PrStanding::Waiting)
                        .as_str();
                    format!("done, PR #{} {st}", pr.number)
                } else {
                    "done, no PR to wait for".to_string()
                }
            }
            other => other.as_str().to_string(),
        };
        out.entry(item.area.clone())
            .or_default()
            .push(format!("{key}: {line}"));
    }
    out
}

/// Queued area items that are waiting on a person's decision (`owner.decisions`).
pub fn person_decisions(
    todo: &[TodoItem],
    runs: &Runs,
    standing: &BTreeMap<u64, PrStanding>,
    area_heads: &BTreeMap<String, String>,
    cfg: &DispatchConfig,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (idx, item) in todo.iter().enumerate() {
        if runs.contains_key(&item.key) || item.area.is_empty() {
            continue;
        }
        if let Gate::Wait { why, person: true } =
            area_gate(idx, todo, runs, standing, area_heads, cfg)
        {
            out.insert(item.key.clone(), why);
        }
    }
    out
}
