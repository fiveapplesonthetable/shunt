//! Every threshold, vocabulary and wording the watch concern uses, with generic defaults.
//!
//! Nothing project-specific lives in the watch code itself: a deployment overrides these from
//! `tacho.toml` (the struct is `serde(default)`, so a partial table keeps the rest).

use serde::{Deserialize, Serialize};

/// Starve (`<tool> starve -- <cmd>`) limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StarveConfig {
    /// CPUs in the default slice.
    pub slice: usize,
    /// The slice may hold at most `max(1, total / max_share)` CPUs.
    pub max_share: usize,
    /// `yes` burners spawned per CPU in the slice.
    pub burners_per_cpu: usize,
    /// Hard cap on the starved command and on each burner, in seconds.
    pub max_call_secs: u64,
}

impl Default for StarveConfig {
    fn default() -> Self {
        Self {
            slice: 4,
            max_share: 8,
            burners_per_cpu: 4,
            max_call_secs: 300,
        }
    }
}

/// Configuration of the watch concern.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchConfig {
    /// Name of the tool, used in sentences (`<tool> starve`, `<tool> kill`).
    pub tool: String,
    /// CPU seconds per second below which a process tree counts as idle.
    pub idle_rate: f64,
    /// Nothing written and not one tick of CPU for this long: HUNG.
    pub hung_after: i64,
    /// Nothing written while the CPU is idle for this long: STALLED.
    pub stall_after: i64,
    /// A phase held for longer than this is flagged.
    pub phase_long: i64,
    /// Delay before a hung agent is resumed (the first resume backoff).
    pub resume_backoff: i64,
    /// Rows show tasks that finished within this window.
    pub row_window: i64,
    /// Interval of the health report / idle-chain review.
    pub report_interval: i64,
    /// Runs per task before an unfinished task counts as stuck instead of resuming.
    pub max_runs: u32,
    /// Agent slots; idle chains are not reported while all are taken.
    pub agent_slots: usize,
    /// A CPU burner older than this, outside starve, is reaped.
    pub burner_limit: i64,
    /// Program names that are CPU burners.
    pub burner_names: Vec<String>,
    /// Programs the burner guard looks through to find what a stage runs.
    pub wrappers: Vec<String>,
    /// Phase vocabulary for notes.
    pub phases: Vec<String>,
    /// Longest note line.
    pub note_limit: usize,
    /// Notes kept per task.
    pub notes_kept: usize,
    /// A cgroup component `<prefix>*.slice` belongs to agents.
    pub agent_slice_prefix: String,
    /// A cgroup component `<prefix><digits>.scope` belongs to agents (systemd-run scopes).
    pub run_scope_prefix: String,
    /// A watched mount with less free space than this makes the machine busy.
    pub low_disk_gb: f64,
    /// Quota use above this percentage makes the machine busy.
    pub max_cpu_percent: f64,
    /// Shortest window for a cgroup CPU reading, in seconds.
    pub cpu_window_secs: f64,
    /// Lock-file suffixes whose holders are diagnosed.
    pub lock_files: Vec<String>,
    /// Build tools whose locks are named when they block a check.
    pub lock_holder_tools: Vec<String>,
    /// Lines kept by the log tail.
    pub tail_lines: usize,
    /// Characters kept per tail line.
    pub tail_chars: usize,
    /// Prefix for systemd slices and transient agent scope units (`<slice_prefix>-agent.slice`).
    pub slice_prefix: String,
    /// Owner slice used as `--cgroup-parent` for local rootful Docker jobs.
    pub owner_slice: String,
    /// Canonical rootful Docker socket path.
    pub docker_socket: String,
    /// Maximum seconds a single `wait` call blocks before returning 3.
    pub wait_limit_secs: i64,
    /// Seconds between heartbeat lines during `wait`.
    pub wait_beat_secs: i64,
    /// Per-worktree directory for named job pid/log/exit files and burner notices.
    pub run_dir: String,
    /// Per-worktree directory for proof summaries.
    pub proof_dir: String,
    /// Notice filename written in `run_dir` when a rogue burner is stopped.
    pub notice_file: String,
    /// Starve limits.
    pub starve: StarveConfig,
}

impl Default for WatchConfig {
    fn default() -> Self {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        Self {
            tool: "tacho".to_string(),
            idle_rate: 0.05,
            hung_after: 30 * 60,
            stall_after: 45 * 60,
            phase_long: 2 * 3600,
            resume_backoff: 120,
            row_window: 24 * 3600,
            report_interval: 3600,
            max_runs: 2,
            agent_slots: 4,
            burner_limit: 10 * 60,
            burner_names: s(&["yes", "stress", "stress-ng"]),
            wrappers: s(&[
                "timeout", "sudo", "nohup", "setsid", "env", "nice", "taskset", "xargs", "time",
                "exec", "command",
            ]),
            phases: s(&["read", "write", "test", "fix", "push"]),
            note_limit: 200,
            notes_kept: 50,
            agent_slice_prefix: "agent".to_string(),
            run_scope_prefix: "run-u".to_string(),
            low_disk_gb: 20.0,
            max_cpu_percent: 80.0,
            cpu_window_secs: 30.0,
            lock_files: s(&[".cargo-lock", ".package-cache", "gradle.lock", ".lock"]),
            lock_holder_tools: s(&[
                "cargo", "rustc", "rustup", "docker", "npm", "node", "gradle", "java", "sccache",
            ]),
            tail_lines: 60,
            tail_chars: 240,
            slice_prefix: "tacho".to_string(),
            owner_slice: "user-1000.slice".to_string(),
            docker_socket: "/run/docker.sock".to_string(),
            wait_limit_secs: 9 * 60,
            wait_beat_secs: 60,
            run_dir: ".tacho-run".to_string(),
            proof_dir: ".tacho-proof".to_string(),
            notice_file: "NOTICE.txt".to_string(),
            starve: StarveConfig::default(),
        }
    }
}

impl WatchConfig {
    /// The environment variable that marks processes started under starve.
    pub fn starve_env(&self) -> String {
        format!("{}_STARVE", self.tool.to_ascii_uppercase())
    }

    pub(crate) fn is_burner_name(&self, name: &str) -> bool {
        self.burner_names.iter().any(|b| b == name)
    }
}
