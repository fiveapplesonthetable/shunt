//! Versioned fleet status snapshot schema (`version: 1`) and section models.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Current schema version for `$TACHO_HOME/snapshot.json`.
pub const SNAPSHOT_VERSION: u32 = 1;

/// Maximum age in seconds (5 minutes) before a daemon snapshot is refused as stale.
pub const MAX_SNAPSHOT_AGE_SECS: i64 = 300;

/// Maximum number of `needs_you` / `attention` items rendered in human text before `... and N more`.
pub const ATTENTION_TEXT_CAP: usize = 5;

/// Valid `--section` names accepted by `tacho status`.
pub const VALID_SECTIONS: &[&str] = &[
    "headline",
    "needs",
    "needs_you",
    "attention",
    "pipeline",
    "agents",
    "machine",
    "cost",
    "held",
    "stale",
    "progress",
    "flakes",
];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActionableItem {
    pub id: String,
    pub target: String,
    pub kind: String,
    pub summary: String,
    pub fix: String,
    pub command_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeadlineSection {
    pub health: String,
    pub needs_you: usize,
    pub attention: usize,
    pub agents: usize,
    pub max_agents: usize,
    pub todo: usize,
    pub open_prs: usize,
    pub batches: usize,
    pub trains: usize,
    pub ready: usize,
    pub held: usize,
    pub stale_holds: usize,
    pub cost_today: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueuedTaskView {
    pub key: String,
    pub area: String,
    pub priority: i32,
    pub provider: String,
    pub model: Option<String>,
    pub ready: bool,
    pub reason: String,
    pub brief: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PipelineSection {
    pub open_prs: usize,
    pub batches: Vec<serde_json::Value>,
    pub trains: Vec<serde_json::Value>,
    pub ready: Vec<serde_json::Value>,
    pub queue: Vec<QueuedTaskView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentStatusRow {
    pub key: String,
    pub status: String,
    pub verdict: String,
    pub phase: String,
    pub phase_age_secs: i64,
    pub phase_long: bool,
    pub area: String,
    pub provider: String,
    pub model: Option<String>,
    pub pr: Option<u64>,
    pub idle_secs: i64,
    pub note: String,
    pub worktree: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CpuOwnerRow {
    pub owner: String,
    pub kind: String,
    pub cores: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MachineSection {
    pub cores_in_use: f64,
    pub quota_cores: f64,
    pub cpu_percent: f64,
    pub max_cpu_percent: f64,
    pub load1: f64,
    pub load5: f64,
    pub usable_cpus: usize,
    pub mem_used_gb: f64,
    pub mem_total_gb: f64,
    pub disk_free_gb: f64,
    pub by_owner: Vec<CpuOwnerRow>,
    pub zombies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TopSpendRow {
    pub key: String,
    pub model: String,
    pub usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PerPrCost {
    pub samples: usize,
    pub avg_usd: f64,
    pub max_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CostSection {
    pub hour: f64,
    pub today: f64,
    pub week: f64,
    pub total_usd: f64,
    pub runs: usize,
    pub by_model: BTreeMap<String, f64>,
    pub by_place: BTreeMap<String, f64>,
    pub by_provider: BTreeMap<String, f64>,
    pub by_area: BTreeMap<String, f64>,
    pub top_today: Vec<TopSpendRow>,
    pub per_pr: PerPrCost,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HoldRow {
    pub target: String,
    pub why: String,
    pub reason: String,
    pub since: i64,
    pub age_secs: i64,
    pub stale: bool,
    pub fix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeldSection {
    pub holds: Vec<HoldRow>,
    pub stale_holds: Vec<HoldRow>,
    pub refused: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProgressSection {
    pub agents: Vec<tacho_core::decide::watch::AgentProgressRow>,
    pub chains: Vec<tacho_core::decide::watch::ChainProgressRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FlakeRowView {
    pub test: String,
    pub area: String,
    pub count: usize,
    pub last_seen: i64,
    pub task_key: Option<String>,
    pub batches: Vec<String>,
    pub excerpt: String,
}

/// Complete versioned status snapshot (`version: 1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub version: u32,
    pub at: i64,
    pub headline: HeadlineSection,
    pub needs_you: Vec<ActionableItem>,
    pub needs: Vec<ActionableItem>,
    pub attention: Vec<ActionableItem>,
    pub pipeline: PipelineSection,
    pub agents: Vec<AgentStatusRow>,
    pub machine: MachineSection,
    pub cost: CostSection,
    pub held: HeldSection,
    pub stale: HeldSection,
    pub progress: ProgressSection,
    pub flakes: Vec<FlakeRowView>,
}
