//! Unit, parity (cases 1..=152), and property tests for `decide::dispatch`.
#![allow(clippy::field_reassign_with_default)]

mod parity_dispatch;
mod parity_dispatch_b;
mod parity_extra;
mod parity_hands;
mod parity_plan;
mod parity_plan_b;
mod parity_resume;
mod parity_runs;
mod props;

use super::config::DispatchConfig;
use super::engine::{DispatchObs, DispatchState};
use super::model::{
    FoundPr, HandsRec, Hold, ItemKind, Mergeable, OpenPr, PrHow, PrRecord, PrStanding, PrState,
    PullFacts, Run, RunStatus, Session, Stopped, TodoItem,
};

pub(super) fn test_cfg() -> DispatchConfig {
    let mut cfg = DispatchConfig::default();
    cfg.home = "/tmp/fleet-home".to_string();
    cfg.repo = "/tmp/fleet-repo".to_string();
    cfg.work_root = "/tmp/fleet-work".to_string();
    cfg.command = "claude -p --model {model} {brief}".to_string();
    cfg.claude_command = "claude -p --model {model} {brief}".to_string();
    cfg.codex_command = "codex exec --model {model} {brief}".to_string();
    cfg.providers.claude_default = "sonnet".to_string();
    cfg.providers.codex_default = "gpt-5.4".to_string();
    cfg.slots = 4;
    cfg.owner_issue = 42;
    cfg.fork_owner = "fork".to_string();
    cfg
}

pub(super) fn open_facts(head: &str, branch: &str) -> PullFacts {
    PullFacts {
        state: PrState::Open,
        head: head.to_string(),
        branch: branch.to_string(),
        branch_exists: true,
        head_exists: true,
    }
}

pub(super) fn make_run(status: RunStatus, task: &str) -> Run {
    Run {
        status,
        task: task.to_string(),
        ..Run::default()
    }
}

pub(super) fn make_todo(key: &str, task: &str, area: &str) -> TodoItem {
    TodoItem {
        key: key.to_string(),
        task: task.to_string(),
        area: area.to_string(),
        ..TodoItem::default()
    }
}
