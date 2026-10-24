//! Status, report, and watch commands querying the fleet snapshot and flight recorder state.
//!
//! Supports versioned fleet snapshots (`version: 1`), `--section` filtering, 300-second
//! staleness refusal, actionable `needs`/`attention` items with fix commands, `machine`
//! CPU vs. quota accounting, `cost`, `held`/`stale`, `progress`, `flakes`, and `Scrubber`
//! redaction across all outputs.

pub mod collect;
pub mod model;
pub mod render;
pub mod report;

#[cfg(test)]
mod tests;

pub use collect::{build_snapshot_from_fleet, load_status_snapshot, validate_sections};
pub use model::{
    ActionableItem, AgentStatusRow, CostSection, CpuOwnerRow, FlakeRowView, HeadlineSection,
    HeldSection, HoldRow, MachineSection, PerPrCost, PipelineSection, ProgressSection,
    QueuedTaskView, StatusSnapshot, TopSpendRow, ATTENTION_TEXT_CAP, MAX_SNAPSHOT_AGE_SECS,
    SNAPSHOT_VERSION, VALID_SECTIONS,
};
pub use render::{execute_status, render_snapshot_json, render_snapshot_text};
pub use report::{execute_report, execute_watch};
