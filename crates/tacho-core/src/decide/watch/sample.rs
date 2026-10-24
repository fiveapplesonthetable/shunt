//! Process-tree CPU sampling (`Sample` and `take_sample`).
//!
//! Requirements R1–R6:
//! - `cpu` is the aggregate CPU seconds of the agent's whole process tree
//!   (`utime + stime + cutime + cstime` of the root pid and every descendant),
//!   so finished, reaped children still count.
//! - On first sight (`prev == None`), `alive_at = busy_at = now`.
//! - `alive_at` advances to `now` iff `grew > 0.0`.
//! - `busy_at` advances to `now` iff `elapsed > 0 && grew / elapsed >= c.idle_rate` (`0.05` CPU-s/s).

use super::config::WatchConfig;
use serde::{Deserialize, Serialize};

/// The CPU seconds a process tree had used at `at`, when it last used any more (`alive_at`),
/// and when it last used them at or above `idle_rate` (`busy_at`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Sample {
    pub cpu: f64,
    pub at: i64,
    pub alive_at: i64,
    pub busy_at: i64,
}

impl Sample {
    /// Construct a fresh sample at `now` where `alive_at` and `busy_at` both start at `now`.
    pub fn first(cpu: f64, now: i64) -> Self {
        Self {
            cpu,
            at: now,
            alive_at: now,
            busy_at: now,
        }
    }
}

/// Update an agent's CPU [`Sample`] from a new process-tree `cpu` reading at `now`.
///
/// - When `prev` is `None`, first sight counts as both alive and busy (`alive_at = busy_at = now`).
/// - Otherwise `alive_at` becomes `now` iff `cpu - prev.cpu > 0.0`.
/// - `busy_at` becomes `now` iff `now - prev.at > 0` and `(cpu - prev.cpu) / (now - prev.at) >= c.idle_rate`.
pub fn take_sample(prev: Option<&Sample>, cpu: f64, now: i64, c: &WatchConfig) -> Sample {
    let Some(p) = prev else {
        return Sample::first(cpu, now);
    };
    let grew = cpu - p.cpu;
    let elapsed = now - p.at;
    let alive_at = if grew > 0.0 { now } else { p.alive_at };
    let busy_at = if elapsed > 0 && (grew / (elapsed as f64)) >= c.idle_rate {
        now
    } else {
        p.busy_at
    };
    Sample {
        cpu,
        at: now,
        alive_at,
        busy_at,
    }
}
