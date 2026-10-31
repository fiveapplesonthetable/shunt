//! Shared verify record types and limits.

use super::*;

/// Maximum PRs combined into a single speculative verification batch.
pub const MAX_BATCH_SIZE: usize = 6;

/// Flake count threshold at which an automated flake-fix task is queued.
pub const FLAKE_TASK_THRESHOLD: u32 = 2;

/// First port in the isolated worktree port range.
pub const PORT_RANGE_START: u16 = 20_000;

/// Last port (exclusive) in the isolated worktree port range.
pub const PORT_RANGE_END: u16 = 32_000;

/// Number of ports reserved per worktree block.
pub const PORT_BLOCK_SIZE: u16 = 20;

/// A check modelled as an area name plus its affected units (packages/targets/crates).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AreaCheck {
    pub area: String,
    #[serde(default)]
    pub units: Vec<String>,
}

/// A planned verification batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchPlan {
    pub id: String,
    pub group: String,
    pub prs: Vec<u64>,
    pub shas: BTreeMap<u64, String>,
    pub areas: Vec<String>,
    #[serde(default)]
    pub checks: Vec<AreaCheck>,
    #[serde(default)]
    pub mode: BatchMode,
    #[serde(default)]
    pub because: String,
}

/// A recorded batch execution in fleet state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchRun {
    pub id: String,
    pub group: String,
    pub prs: Vec<u64>,
    #[serde(default)]
    pub shas: BTreeMap<u64, String>,
    #[serde(default)]
    pub main_sha: String,
    #[serde(default)]
    pub areas: Vec<String>,
    #[serde(default)]
    pub checks: Vec<AreaCheck>,
    /// Original successful batch for each complete area proof.
    #[serde(default)]
    pub area_sources: BTreeMap<String, String>,
    #[serde(default)]
    pub mode: BatchMode,
    /// `"running"`, `"passed"`, `"failed"`, `"flake"`, `"bisecting"`, `"head-moved"`, `"main-broken"`, `"hang"`
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub tree: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub log: String,
    #[serde(default)]
    pub culprit: Option<u64>,
    #[serde(default)]
    pub skipped: Vec<String>,
    #[serde(default)]
    pub flakes: Vec<String>,
}

/// Record of a PR passing batch verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PassedRecord {
    pub pr: u64,
    pub sha: String,
    pub main_sha: String,
    pub batch_id: String,
    pub passed_at: i64,
    #[serde(default)]
    pub tree: String,
    #[serde(default)]
    pub areas: Vec<String>,
}

/// Record of a PR failing batch verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FailedRecord {
    pub pr: u64,
    pub sha: String,
    pub batch_id: String,
    pub failed_at: i64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub failed_tests: Vec<String>,
    #[serde(default)]
    pub counterexample: Option<String>,
    #[serde(default)]
    pub log: String,
}

/// State of a binary search bisection over a batch's ordered `[pr, sha]` prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bisect {
    /// Ordered `(pr, sha)` members of the batch.
    pub members: Vec<(u64, String)>,
    /// Prefix length Results: `k -> passed` (`k == 0` is `main` alone, `k == i` merges `members[..i]`).
    pub results: BTreeMap<usize, bool>,
}

/// Next step returned by [`next_probe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeStep {
    /// Run a check on `members[..k]` (`k == 0` tests `main` alone).
    Probe(usize),
    /// Exact culprit member `(pr, sha)` identified.
    Culprit((u64, String)),
    /// `main` itself (`k == 0`) failed.
    MainBroken,
}

/// A recorded test flake entry in the persistent flake ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FlakeEntry {
    pub test: String,
    #[serde(default)]
    pub area: String,
    pub count: u32,
    pub first_seen: i64,
    pub last_seen: i64,
    #[serde(default)]
    pub batches: Vec<String>,
    #[serde(default)]
    pub excerpt: String,
    #[serde(default)]
    pub task_key: Option<String>,
}

/// Rendered row for `tacho flakes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlakeRow {
    pub test: String,
    pub area: String,
    pub count: u32,
    pub last_seen: i64,
    pub batches: Vec<String>,
    pub task_key: Option<String>,
    pub excerpt: String,
}

/// Result of the single-retry fix agent state machine for red PRs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FixDecisions {
    pub dispatch: Vec<(u64, FailedRecord)>,
    pub notify: Vec<(u64, String)>,
}

/// A single check command inside a [`ProofSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCheck {
    pub name: String,
    pub cmd: String,
    pub ok: bool,
    #[serde(default)]
    pub area: Option<String>,
    #[serde(default)]
    pub units: Vec<String>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub output_tail: String,
}

/// Proof artifact written by `tacho prove` to `.tachoproof/summary.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProofSummary {
    pub head: String,
    pub tree: String,
    pub base: String,
    pub ok: bool,
    pub timestamp: i64,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub checks: Vec<ProofCheck>,
    #[serde(default)]
    pub screenshots: Vec<String>,
}

/// Configurable rule mapping file path prefixes or regexes to area names and check commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AreaRule {
    pub area: String,
    #[serde(default)]
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub unit_prefixes: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub checks: Vec<(String, String)>,
    #[serde(default)]
    pub isolate_batch: bool,
    #[serde(default)]
    pub needs_screenshot: bool,
}
