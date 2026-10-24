//! Pure progress verdict (`moving`, `STALLED`, `HUNG`) and deadline calculation.
//!
//! Requirements R13–R17:
//! - `last_work = max(last_commit, last_change, started)`, ignoring `None`.
//! - `HUNG` iff `now - last_work >= hung_after` (1800 s) AND `now - alive_at >= hung_after` (1800 s). Checked first.
//! - `STALLED` iff `now - last_work >= stall_after` (2700 s) AND `now - busy_at >= stall_after` (2700 s).
//! - Otherwise `MOVING`. A busy tree keeps a quiet agent moving; a breathing tree (alive recently) is not hung.
//! - Verdict strings: `"moving"`, `"STALLED"`, `"HUNG"`.

use super::activity::Activity;
use super::config::WatchConfig;
use super::sample::Sample;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Progress verdict for a running agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Verdict {
    #[serde(rename = "moving")]
    Moving,
    #[serde(rename = "STALLED")]
    Stalled,
    #[serde(rename = "HUNG")]
    Hung,
}

impl Verdict {
    /// Exact reference string (`"moving"`, `"STALLED"`, `"HUNG"`).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Verdict::Moving => "moving",
            Verdict::Stalled => "STALLED",
            Verdict::Hung => "HUNG",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Latest timestamp of work in the worktree (`max(last_commit, last_change, started)`).
pub fn last_work(act: &Activity, started: i64) -> i64 {
    [act.last_commit, act.last_change, Some(started)]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(started)
}

/// Judge a running agent's progress from its worktree [`Activity`] and process-tree [`Sample`].
///
/// - Checked first: `HUNG` when `now - last_work >= c.hung_after` AND `now - sample.alive_at >= c.hung_after`.
/// - Next: `STALLED` when `now - last_work >= c.stall_after` AND `now - sample.busy_at >= c.stall_after`.
/// - Otherwise: `MOVING`.
pub fn verdict(
    act: &Activity,
    sample: &Sample,
    started: i64,
    now: i64,
    c: &WatchConfig,
) -> Verdict {
    let work_at = last_work(act, started);
    if now - work_at >= c.hung_after && now - sample.alive_at >= c.hung_after {
        return Verdict::Hung;
    }
    if now - work_at >= c.stall_after && now - sample.busy_at >= c.stall_after {
        return Verdict::Stalled;
    }
    Verdict::Moving
}

/// Next timestamp (> `now`) at which this agent will transition to `Hung` or `Stalled`
/// if no further work or CPU activity occurs.
pub fn agent_next_deadline(
    act: &Activity,
    sample: &Sample,
    started: i64,
    now: i64,
    c: &WatchConfig,
) -> Option<i64> {
    let work_at = last_work(act, started);
    let hung_at = work_at.max(sample.alive_at) + c.hung_after;
    let stall_at = work_at.max(sample.busy_at) + c.stall_after;
    [hung_at, stall_at].into_iter().filter(|&t| t > now).min()
}
