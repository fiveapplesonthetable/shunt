//! Every project-specific value verify needs, with generic defaults.
//!
//! The decisions in this module never name an area, a CI file or a log phrase of
//! any particular project; they read them from [`VerifyConfig`]. The defaults are
//! deliberately neutral: one `core` fallback group, `gate` area, and log phrases
//! that describe machines failing (full disks, dead networks) rather than any one
//! project's tooling.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// How a [`GroupRule`] matches the areas of a stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupMatch {
    /// Every listed area is touched (the stack's areas are a superset).
    SupersetOf(BTreeSet<String>),
    /// The stack touches only listed areas, possibly none.
    SubsetOf(BTreeSet<String>),
    /// The stack touches at least one area, and only listed ones.
    NonEmptySubsetOf(BTreeSet<String>),
}

/// One rule of the batch grouping table. Rules are tried in order; the first match wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRule {
    pub name: String,
    pub when: GroupMatch,
}

/// Log phrases that drive classification, test extraction and excerpts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogPatterns {
    /// Regexes that mean the machine failed, not the code (R25).
    pub infrastructure: Vec<String>,
    /// Regexes that mean a transient network/registry outage eligible for 1 step retry (R41).
    #[serde(default)]
    pub network: Vec<String>,
    /// Regexes with one capture group naming a failed test (R25).
    pub failed_test: Vec<String>,
    /// Plain substring that marks a property-test counterexample (R25).
    pub counterexample: String,
    /// Plain substrings that make a log line worth quoting in a blame excerpt (R22).
    pub excerpt_markers: Vec<String>,
    /// Maximum excerpt lines (R22).
    pub excerpt_lines: usize,
}

impl Default for LogPatterns {
    fn default() -> Self {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        Self {
            infrastructure: s(&[
                "No space left on device",
                ": command not found",
                "terminating connection due to administrator command",
                "lease does not exist",
                "is not installed for the toolchain",
                "mount callback failed",
                "not found: content digest sha256:",
                "failed to export layer",
                "No such image: sha256:",
                "address already in use",
                "The operation was canceled",
                "TLC did not finish",
                "UnsatisfiedLinkError",
                "Could not resolve host",
                "received a shutdown signal",
                "fetch of main failed",
                "worktree failed",
                "Terminated",
                "containers failed to start",
                "could not kill container",
                "Error waiting for container",
                "Cannot connect to the Docker daemon",
                "network .* not found",
                "failed to set up container networking",
                "could not find an available, non-overlapping IPv4 address pool",
                "docker could not create the network",
            ]),
            network: s(&[
                "Could not resolve host",
                "Connection reset by peer",
                "TLS handshake timeout",
                "502 Bad Gateway",
                "503 Service Unavailable",
                "504 Gateway Time-out",
            ]),
            failed_test: s(&[
                r"(?m)^test (\S+) \.\.\. FAILED",
                r"(?m)^\s*FAIL\s+(.+)$",
                r"(?m)^(\w+ > \w+) FAILED",
                r"(?m)✘\s+\d+ \[\w+\] › (.+?) \(\d",
            ]),
            counterexample: "minimal failing input".to_string(),
            excerpt_markers: s(&[
                "FAILED", "panicked", "error[", "✘", "failed:", "Error:", "budget",
            ]),
            excerpt_lines: 12,
        }
    }
}

/// Patterns for parsing per-area cache and passed-test counts from logs (R39).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsPatterns {
    pub sccache_line: String,
    pub sccache_hits: String,
    pub sccache_misses: String,
    pub rust_passed: String,
    pub vitest_passed: String,
    pub pytest_passed: String,
    pub passed_tests: Vec<String>,
}

impl Default for StatsPatterns {
    fn default() -> Self {
        Self {
            sccache_line: r"(?m)^sccache: (\d+) hits, (\d+) misses$".to_string(),
            sccache_hits: r"(?i)cache hits\s+(\d+)".to_string(),
            sccache_misses: r"(?i)cache misses\s+(\d+)".to_string(),
            rust_passed: r"test result: ok\.\s+(\d+)\s+passed".to_string(),
            vitest_passed: r"Tests\s+.*?\((\d+)\)|Tests\s+(\d+)\s+passed".to_string(),
            pytest_passed: r"\b(\d+)\s+passed\b".to_string(),
            passed_tests: vec![
                r"(?m)^test result: \w+\. (\d+) passed".to_string(),
                r"(?m)^android tests: (\d+) ran, 0 failed$".to_string(),
                r"(?m)^\s+(\d+) passed \(".to_string(),
            ],
        }
    }
}

/// A rule mapping a file path regex to areas on the `change` and `train` tiers (R13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PathAreaRule {
    pub pattern: String,
    pub areas: BTreeSet<String>,
    #[serde(default)]
    pub train_areas: Option<BTreeSet<String>>,
}

/// Area selection inputs (R13–R16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AreaSelectConfig {
    /// Area that must pass before any other runs, and whose failure reruns everything.
    pub gate: Option<String>,
    /// Areas that never run in a batch; they belong to post-merge.
    pub post_merge: BTreeSet<String>,
    /// Regexes of files that draw or compare pixels; a change touching one needs `pixel_areas`.
    pub pixel_files: Vec<String>,
    pub pixel_areas: BTreeSet<String>,
    /// Extra areas added on the change tier unless skipped.
    pub change_extra: BTreeSet<String>,
    /// `area -> input regex`: a slow area is dropped when no file matches its inputs (change tier).
    pub change_inputs: BTreeMap<String, String>,
    /// Same as `change_inputs`, for the train tier.
    pub train_inputs: BTreeMap<String, String>,
    /// Ordered path rules for tier selection (R13).
    #[serde(default)]
    pub path_rules: Vec<PathAreaRule>,
    /// Patterns for documentation files ignored by area selection unless matching `doc_keep`.
    #[serde(default)]
    pub doc_ignore: Vec<String>,
    #[serde(default)]
    pub doc_keep: Vec<String>,
    /// All known areas when `tier == All` or a path is unplaced on `Tier::Change` (R13).
    #[serde(default)]
    pub all_areas: BTreeSet<String>,
    /// Areas added on `Tier::Train` when a path cannot be placed (R13).
    #[serde(default)]
    pub train_unplaced_areas: BTreeSet<String>,
    /// `file regex -> regenerator area` for generated files whose merge conflicts can be settled automatically (R30).
    #[serde(default)]
    pub generated: BTreeMap<String, String>,
}

/// Configuration for mapping areas to CI workflow jobs and building container/host argvs (R34–R43).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRunnerConfig {
    pub job_of_area: BTreeMap<String, String>,
    pub stand_ins: Vec<String>,
    pub skip_steps: BTreeSet<String>,
    pub allowed_artifact_downloads: BTreeMap<String, BTreeSet<String>>,
    pub warm_volumes: BTreeSet<String>,
    pub drop_step_env_keys: BTreeSet<String>,
    pub drop_step_env_patterns: Vec<(Option<String>, String)>,
    pub step_env_overrides: BTreeMap<String, (BTreeSet<String>, String)>,
    pub host_resource_areas: BTreeSet<String>,
    pub checks_script: String,
    pub wrapper_rel_path: String,
    pub private_node_modules_areas: BTreeSet<String>,
    pub private_node_modules_rel: String,
    pub github_repository: String,
    pub tuning_vars: Vec<String>,
    pub driver_name: String,
    pub cache_root: String,
    pub sccache_size: String,
    pub cache_subdirs: Vec<String>,
    pub default_image: String,
    pub container_memory: String,
    pub container_memory_swap: String,
    pub container_pids_limit: u64,
}

impl Default for CiRunnerConfig {
    fn default() -> Self {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        Self {
            job_of_area: BTreeMap::new(),
            stand_ins: s(&[
                "actions/checkout",
                "Swatinem/rust-cache",
                "actions/setup-node",
                "actions/upload-artifact",
                "actions/download-artifact",
            ]),
            skip_steps: BTreeSet::from(["Fetch selected base".to_string()]),
            allowed_artifact_downloads: BTreeMap::new(),
            warm_volumes: BTreeSet::new(),
            drop_step_env_keys: BTreeSet::new(),
            drop_step_env_patterns: vec![
                (None, r"^\$\{\{ matrix\.(lane|partition) \}\}$".to_string()),
                (None, r"^\$\{\{ matrix\.shard \}\}/\d+$".to_string()),
            ],
            step_env_overrides: BTreeMap::new(),
            host_resource_areas: BTreeSet::from(["android".to_string()]),
            checks_script: "scripts/checks.sh".to_string(),
            wrapper_rel_path: "scripts/rustc-wrapper.sh".to_string(),
            private_node_modules_areas: BTreeSet::from([
                "console".to_string(),
                "console-crawl".to_string(),
                "deploy".to_string(),
            ]),
            private_node_modules_rel: "frontend/admin/node_modules".to_string(),
            github_repository: "org/repo".to_string(),
            tuning_vars: s(&[
                "CARGO_PROFILE_DEV_DEBUG",
                "CARGO_PROFILE_TEST_DEBUG",
                "CARGO_INCREMENTAL",
            ]),
            driver_name: "tacho".to_string(),
            cache_root: "/var/cache/tacho-ci".to_string(),
            sccache_size: "20G".to_string(),
            cache_subdirs: s(&[
                "cargo",
                "sccache",
                "npm",
                "pip",
                "playwright",
                "gradle",
                "android",
                "target",
            ]),
            default_image: "ubuntu:24.04".to_string(),
            container_memory: "12g".to_string(),
            container_memory_swap: "12g".to_string(),
            container_pids_limit: 4096,
        }
    }
}

/// Everything verify decides with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyConfig {
    /// CLI tool name used in human reasons and prompts (default `"tacho"`).
    #[serde(default = "default_tool_name")]
    pub tool_name: String,
    /// Members per batch before a stack is only taken whole (R8).
    pub batch_size: usize,
    /// Culprit rounds before a batch gives up (R19).
    pub rounds: u32,
    /// Maximum reruns on red to separate a flake from a real break (R20).
    pub reruns: u32,
    /// Skip flaky rerun when a batch has only 1 member (R20).
    pub no_rerun_single_member: bool,
    /// Areas that never rerun on a single-member failure (R20).
    pub no_rerun_areas: BTreeSet<String>,
    /// Batch-wide wall clock, seconds (R18).
    pub wall_clock_secs: i64,
    /// Ceiling for a single check's timeout, seconds.
    pub check_timeout_cap_s: i64,
    /// Group name a person chose; such batches report red once and never rerun (R20).
    pub manual_group: String,
    /// Group name for pipeline-touching PRs; such a batch takes only its first member (R5).
    pub ci_group: String,
    /// Regexes of files that change the pipeline itself (R5). Empty means none do.
    pub ci_files: Vec<String>,
    /// Areas ignored when choosing a group (touched by nearly everything).
    pub group_ignore: BTreeSet<String>,
    /// Ordered grouping table (R4).
    pub groups: Vec<GroupRule>,
    /// Group when no rule matches.
    pub fallback_group: String,
    /// Flakes of one test before a fix task is queued (R24).
    pub flake_threshold: u32,
    /// Model assigned to queued flake-fix tasks (R24).
    #[serde(default = "default_flake_model")]
    pub flake_model: String,
    /// After a green rerun, demand one more green run on the same tree before passing anyone.
    pub confirm_flaky_green: bool,
    /// Explicit owner opt-in to pass members after a single green rerun on the same tree.
    #[serde(default)]
    pub pass_on_flaky_rerun: bool,
    /// Commit status context, e.g. `merge-check`.
    pub status_context: String,
    /// Maximum commit status description length.
    pub status_max_len: usize,
    /// CPUs held back from area allocation (R35).
    pub reserved_cpus: usize,
    pub log_patterns: LogPatterns,
    #[serde(default)]
    pub stats: StatsPatterns,
    pub areas: AreaSelectConfig,
    #[serde(default)]
    pub ci_runner: CiRunnerConfig,
}

fn default_tool_name() -> String {
    "tacho".to_string()
}

fn default_flake_model() -> String {
    "sonnet".to_string()
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            tool_name: default_tool_name(),
            batch_size: 4,
            rounds: 3,
            reruns: 1,
            no_rerun_single_member: true,
            no_rerun_areas: BTreeSet::from(["specs".to_string(), "fuzz".to_string()]),
            wall_clock_secs: 1800,
            check_timeout_cap_s: 4 * 3600,
            manual_group: "manual".to_string(),
            ci_group: "ci".to_string(),
            ci_files: vec![
                r"^\.github/workflows/".to_string(),
                r"^docker/".to_string(),
                r"^scripts/(checks|changed-areas)\.sh$".to_string(),
                r"^scripts/areas/".to_string(),
            ],
            group_ignore: BTreeSet::from(["gate".to_string(), "fuzz".to_string()]),
            groups: Vec::new(),
            fallback_group: "core".to_string(),
            flake_threshold: 2,
            flake_model: default_flake_model(),
            confirm_flaky_green: false,
            pass_on_flaky_rerun: false,
            status_context: "merge-check".to_string(),
            status_max_len: 140,
            reserved_cpus: 2,
            log_patterns: LogPatterns::default(),
            stats: StatsPatterns::default(),
            areas: AreaSelectConfig::default(),
            ci_runner: CiRunnerConfig::default(),
        }
    }
}
