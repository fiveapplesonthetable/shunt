//! PR stack ancestry, batch planning, prefix bisection, log failure classification
//! (code vs. infrastructure), flake ledger, single-retry fix state machine, and
//! worker proof/PR preflight guardrails.

use crate::decide::dispatch::{already_ran, AgentRun, Hold, PullFacts, TodoItem};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::LazyLock;

pub mod areas;
pub use areas::*;
pub mod bisect;
pub use bisect::{
    is_culprit, next_prefix, step as bisect_step, BisectDecision, BisectProbe, Probe,
};
pub mod ci_job;
pub use ci_job::*;
pub mod ci_refs;
pub use ci_refs::*;
pub mod ci_runner;
pub use ci_runner::*;
pub mod config;
pub use config::*;
pub mod flakes;
pub use flakes::*;
pub mod grouping;
pub use grouping::*;
pub mod logs;
pub use logs::*;
pub mod machine;
pub use machine::*;
pub mod plan;
pub use plan::*;
pub mod proof;
pub use proof::*;
pub mod ready;
pub use ready::*;
pub mod result;
pub use result::*;
pub mod stats;
pub use stats::*;
pub mod status;
pub use status::*;
pub mod types;
pub use types::*;
mod verdict;
pub use verdict::*;
pub mod worktree;
pub use worktree::*;
mod yaml;
pub(crate) use yaml::*;

#[cfg(test)]
mod tests;
