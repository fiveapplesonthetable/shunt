//! Run lineage and task state (`base`, `base~2`, … stopping at the first gap).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

use super::config::DispatchConfig;
use super::model::{Run, RunState, RunStatus, Runs};

pub const RESUME_LIMIT: u32 = 1;
pub const BACKOFF: [i64; 1] = [120];
pub const SHOWN_COMMITS: usize = 10;
pub const SHOWN_FILES: usize = 30;
pub const TAIL_LINES: usize = 8;
pub const ESCALATION_LINES: usize = 20;

/// A run key: `base` for attempt 1, `base~n` for attempt `n >= 2`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RunKey {
    pub base: String,
    pub attempt: u32,
}

impl RunKey {
    /// The first run of `base`.
    pub fn first(base: &str) -> Self {
        RunKey {
            base: base.to_string(),
            attempt: 1,
        }
    }

    /// Parse `a~3` into `(a, 3)`; a key without a numeric `~n` suffix is attempt 1.
    pub fn parse(key: &str) -> Self {
        if let Some((base, n)) = key.rsplit_once('~') {
            if !base.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(attempt) = n.parse::<u32>() {
                    return RunKey {
                        base: base.to_string(),
                        attempt: attempt.max(1),
                    };
                }
            }
        }
        RunKey::first(key)
    }

    /// The next attempt of the same task.
    pub fn next(&self) -> Self {
        RunKey {
            base: self.base.clone(),
            attempt: self.attempt + 1,
        }
    }
}

impl fmt::Display for RunKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.attempt <= 1 {
            write!(f, "{}", self.base)
        } else {
            write!(f, "{}~{}", self.base, self.attempt)
        }
    }
}

/// The task a run belongs to: `a~3` is the third run of `a`.
pub fn base_key(key: &str) -> String {
    RunKey::parse(key).base
}

/// The attempt number of `key` (`1` for `a`, `3` for `a~3`).
pub fn attempt(key: &str) -> u32 {
    RunKey::parse(key).attempt
}

/// Format a run key (`base` for `n <= 1`, `base~n` for `n >= 2`).
pub fn run_key(base: &str, n: u32) -> String {
    RunKey {
        base: base.to_string(),
        attempt: n.max(1),
    }
    .to_string()
}

/// The task's runs as `(key, run)`, first to latest, stopping at the first missing attempt.
pub fn lineage<'a>(base: &str, runs: &'a Runs) -> Vec<(String, &'a Run)> {
    let mut out = Vec::new();
    let mut key = RunKey::first(base);
    while let Some(run) = runs.get(&key.to_string()) {
        out.push((key.to_string(), run));
        key = key.next();
    }
    out
}

/// The newest run of the task, or `None` before it started.
pub fn latest<'a>(base: &str, runs: &'a Runs) -> Option<(String, &'a Run)> {
    lineage(base, runs).pop()
}

/// The distinct task bases with runs, in insertion/key order.
pub fn bases(runs: &Runs) -> Vec<String> {
    let set: BTreeSet<String> = runs.keys().map(|k| RunKey::parse(k).base).collect();
    set.into_iter().collect()
}

/// `queued`, `running`, `done`, `resuming` (unfinished with retries left) or `stuck`.
pub fn run_state(base: &str, runs: &Runs, cfg: &DispatchConfig) -> RunState {
    let Some((key, run)) = latest(base, runs) else {
        return RunState::Queued;
    };
    match run.status {
        RunStatus::Unfinished if RunKey::parse(&key).attempt <= cfg.resume_limit => {
            RunState::Resuming
        }
        RunStatus::Running => RunState::Running,
        RunStatus::Done => RunState::Done,
        _ => RunState::Stuck,
    }
}

/// Keys that count as done for `--after`: every done run, and the base of every done run.
pub fn done_keys(runs: &Runs) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (k, r) in runs {
        if r.status == RunStatus::Done {
            out.insert(k.clone());
            out.insert(RunKey::parse(k).base);
        }
    }
    out
}

/// How many runs are live.
pub fn running_count(runs: &Runs) -> u32 {
    runs.values()
        .filter(|r| r.status == RunStatus::Running)
        .count() as u32
}
