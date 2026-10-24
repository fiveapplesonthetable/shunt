//! Pure operational decisions: config, tell-once ledger, snapshot, render, self-update,
//! watchdog, tidy, schedule, setup, and agent kit.
//!
//! Everything here is a pure function of plain data: no I/O, no clocks (callers pass
//! `now: i64` in unix seconds), no processes. The "hands" (daemon) observe the world,
//! call these functions, and carry out the returned actions. Every action carries a
//! human-readable `why`/reason so it can be logged and replayed.
//!
//! Submodules:
//! - [`config`]: systemd `EnvironmentFile` parser, env-wins merge, hot reload, CPU limit,
//!   busy gate, private TMPDIR validation, agent command templates.
//! - [`tell`]: one tell-once ledger (`key -> fingerprint, at`) replacing per-fact lists,
//!   notices, delivery settlement, untied-failure counting and the daily digest.
//! - [`selfupdate`]: target choice, behind-distance text, and the update state machine.
//! - [`watchdog`]: liveness reason and the OPEN/UPDATE/CLOSE issue lifecycle.
//! - [`tidy`]: machine janitor decisions with ownership, rescue planning, and stale-run retirement.
//! - [`schedule`]: daemon poll decision, ordered isolated tick steps, PR/train/conflict/deploy/main
//!   step rules, and events `--since`.
//! - [`setup`]: env template filling, undocumented-variable detection, link actions, address pool
//!   and root advice.
//! - [`snapshot`]: `v1` status snapshot construction, section slicing, attention/needs-you rules,
//!   process owner classification, cost summary, and secret redaction.
//! - [`render`]: CLI `status_view`, `top` terminal frames, HTML/Markdown status mirror, HTTP
//!   routing, and `next_step` advisor.
//! - [`kit`]: agent kit pure rules (slug, port blocks, migration order, lock key, node cache,
//!   first error, proof problems, attribution, PR refusals, PR body).

pub mod config;
pub mod kit;
pub mod render;
pub mod schedule;
pub mod selfupdate;
pub mod setup;
pub mod snapshot;
pub mod tell;
pub mod tidy;
pub mod watchdog;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

/// Every project-specific value used by the ops decisions, with generic defaults.
///
/// Hands build one of these from the env file / project plugin and pass it in; no
/// decision reads a global.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpsConfig {
    /// The tool name used in human text (`<tool> version`, `<tool> tick`).
    pub tool: String,
    /// Environment variable prefix (e.g. `TACHO_` gives `TACHO_MAX_CPU_PERCENT`).
    pub env_prefix: String,
    /// Mail subject prefix (the project's name).
    pub project: String,
    /// Issue where the owner answers decisions.
    pub owner_issue: u64,
    pub tidy: tidy::TidyPolicy,
    pub tell: tell::TellPolicy,
    pub selfupdate: selfupdate::SelfUpdatePolicy,
    pub watchdog: watchdog::WatchdogPolicy,
    pub schedule: schedule::SchedulePolicy,
    pub setup: setup::SetupPolicy,
    pub attention: snapshot::AttentionRules,
    pub owners: snapshot::OwnerRules,
    /// Default CPU limit in percent when nothing valid is configured.
    pub default_cpu_percent: f64,
}

impl Default for OpsConfig {
    fn default() -> Self {
        Self {
            tool: "tacho".into(),
            env_prefix: "TACHO_".into(),
            project: "fleet".into(),
            owner_issue: 0,
            tidy: tidy::TidyPolicy::default(),
            tell: tell::TellPolicy::default(),
            selfupdate: selfupdate::SelfUpdatePolicy::default(),
            watchdog: watchdog::WatchdogPolicy::default(),
            schedule: schedule::SchedulePolicy::default(),
            setup: setup::SetupPolicy::default(),
            attention: snapshot::AttentionRules::default(),
            owners: snapshot::OwnerRules::default(),
            default_cpu_percent: 90.0,
        }
    }
}

/// `90 min`, `2 h`: a short human span used in reasons.
pub fn hours(seconds: i64) -> String {
    if seconds >= 3600 {
        format!("{} h", seconds / 3600)
    } else {
        format!("{} min", seconds.max(0) / 60)
    }
}

/// The first `n` characters of a sha (never panics on odd input).
pub fn short(sha: &str) -> &str {
    crate::char_prefix(sha, 8)
}
