//! Stall detectors for autonomous coding agents (design.md section 7).
//!
//! Detectors are pure functions over agent state and event streams. Each stall kind
//! has distinct trigger and clearing semantics:
//!
//! - **SILENT**: Fires when a run is open and no events have arrived for `silent_timeout_ns`
//!   (default: 300s) while not in Wait. CPU-rate aware: suppressed when process-tree CPU
//!   consumption is at least 50ms/s (0.05 CPU-s/s). Cleared by any event arrival or active
//!   CPU rate sample.
//!
//! - **LOOP**: Fires when the identical tool input digest is observed `loop_repeats` times
//!   (default: 4) in a row. Cleared as soon as a non-repeating tool input digest arrives.
//!
//! - **SPIN**: Fires when `spin_turns` (default: 8) consecutive turns conclude without any
//!   file edits (`FileEdited`) and without any build/test/commit commands (`CommandClass::Build`,
//!   `CommandClass::Test`, `CommandClass::Commit`). Only applies to the Worker role. Cleared by
//!   worktree progress, file edits, or build/test/commit commands.
//!
//! - **BURN**: Fires when cumulative token usage (`tokens_out`) exceeds `burn_tokens`
//!   (default: 200,000, or `task_token_budget` if configured) within a sliding window of
//!   `burn_window_ns` (default: 10 minutes). Cleared when the rolling token sum falls back to
//!   or below the budget limit.
//!
//! - **FAILING**: Evaluates command completion specifically for commands classified as
//!   `CommandClass::Build` or `CommandClass::Test`. Command execution is tracked per normalized
//!   command string (`argv.trim()`). When the same test or build command fails consecutively
//!   `failing_repeats` times (default: 3), FAILING fires. Cleared only when that specific command
//!   subsequently succeeds (exit code 0), resetting its consecutive failure counter and clearing
//!   the FAILING stall. General commands and other classes do not trigger or clear FAILING.
//!
//! - **OFF_BRIEF**: Fires when a file edit (`FileEdited`) occurs outside declared task paths
//!   (`task_declared_paths`). Unlike other detectors, OFF_BRIEF is sticky across subsequent
//!   in-brief edits: an in-brief edit must NOT clear an active OFF_BRIEF stall. It clears only
//!   on task change (`set_declared_paths`) or run end.
//!
//! - **CLAIM**: Evaluates at run end for agents with the Worker role. Fires if files were
//!   edited during the run without a subsequent passing test command (`CommandClass::Test` with
//!   exit code 0).

use std::collections::{BTreeSet, HashMap, VecDeque};
use tacho_proto::tacho;

use crate::classify::CommandClass;
use crate::input::AgentRole;

/// Configuration thresholds for stall detectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorConfig {
    /// Silence duration without events before SILENT fires (default: 300s).
    pub silent_timeout_ns: u64,
    /// Number of identical tool input digests before LOOP fires (default: 4).
    pub loop_repeats: usize,
    /// Number of progress-free turns before SPIN fires (default: 8).
    pub spin_turns: usize,
    /// Token threshold within window before BURN fires (default: 200,000).
    pub burn_tokens: u64,
    /// Sliding window duration for BURN detector (default: 10m = 600s).
    pub burn_window_ns: u64,
    /// Number of consecutive test/build command failures before FAILING fires (default: 3).
    pub failing_repeats: usize,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            silent_timeout_ns: 300 * 1_000_000_000,
            loop_repeats: 4,
            spin_turns: 8,
            burn_tokens: 200_000,
            burn_window_ns: 600 * 1_000_000_000,
            failing_repeats: 3,
        }
    }
}

/// Represents a single detector firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorFiring {
    pub kind: tacho::StallKind,
    pub repeats: Option<u32>,
    pub since_us: Option<u64>,
    pub evidence: String,
}

/// Computes the 8-byte hexadecimal BLAKE3 digest of a text string.
pub fn blake3_digest_8(text: &str) -> String {
    let hash = blake3::hash(text.as_bytes());
    let b = hash.as_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
    )
}

/// Stateful tracking for one agent's detectors.
#[derive(Debug, Clone, Default)]
pub struct AgentDetectorState {
    pub role: AgentRole,
    pub last_event_ts_ns: u64,
    pub last_cpu_activity_ns: Option<u64>,
    pub last_cpu_sample: Option<(u64, i64)>, // (ts_ns, cumulative_cpu_us)
    pub suppress_live_silent: bool,
    pub last_edit_ts_ns: Option<u64>,
    pub last_passing_test_ts_ns: Option<u64>,
    pub commits_succeeded: u32,
    pub pushes_succeeded: u32,
    pub task_token_budget: Option<u64>,
    pub task_cost_budget_micros: Option<u64>,
    pub recent_tool_digests: VecDeque<String>,
    pub spin_turns_count: usize,
    pub spin_turn_has_progress: bool,
    pub burn_window: VecDeque<(u64, u64)>, // (ts_ns, tokens_out)
    pub failing_commands: HashMap<String, usize>, // normalized argv -> consecutive failures
    pub task_declared_paths: Option<Vec<String>>,
    pub active_stalls: BTreeSet<i32>,
    pub is_stalled: bool,
}

impl AgentDetectorState {
    /// Creates a new detector state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enters a stall kind, returning `true` only if this stall kind was not already active.
    pub fn enter_stall(&mut self, kind: tacho::StallKind) -> bool {
        let inserted = self.active_stalls.insert(kind as i32);
        self.is_stalled = !self.active_stalls.is_empty();
        inserted
    }

    /// Clears a specific stall kind and updates `is_stalled`.
    pub fn clear_stall(&mut self, kind: tacho::StallKind) {
        self.active_stalls.remove(&(kind as i32));
        self.is_stalled = !self.active_stalls.is_empty();
    }

    /// Records event arrival time and resets silence flag.
    pub fn touch_event(&mut self, ts_ns: u64) {
        self.last_event_ts_ns = self.last_event_ts_ns.max(ts_ns);
        self.clear_stall(tacho::StallKind::Silent);
    }

    /// Records CPU activity for CPU-aware silence detection.
    pub fn touch_cpu_activity(&mut self, ts_ns: u64) {
        self.last_cpu_activity_ns = Some(ts_ns);
        self.clear_stall(tacho::StallKind::Silent);
    }

    /// Records a cumulative CPU counter sample (in microseconds) and suppresses `SILENT`
    /// only when the process-tree CPU rate is at least 50ms/s (50,000 us/s = 0.05 CPU-s/s).
    /// The first sample only sets the baseline without rate checks or activity touching.
    pub fn record_cpu_sample(&mut self, ts_ns: u64, cpu_us: i64) {
        const MIN_ACTIVE_CPU_US_PER_SEC: u128 = 50_000;
        let Some((prev_ts, prev_cpu_us)) = self.last_cpu_sample else {
            self.last_cpu_sample = Some((ts_ns, cpu_us));
            return;
        };
        self.last_cpu_sample = Some((ts_ns, cpu_us));

        let dt_ns = ts_ns.saturating_sub(prev_ts);
        let d_cpu_us = cpu_us.saturating_sub(prev_cpu_us).max(0) as u128;
        if dt_ns > 0 {
            let rate_us_per_s = (d_cpu_us * 1_000_000_000) / (dt_ns as u128);
            if rate_us_per_s >= MIN_ACTIVE_CPU_US_PER_SEC {
                self.touch_cpu_activity(ts_ns);
            }
        } else if d_cpu_us >= MIN_ACTIVE_CPU_US_PER_SEC {
            self.touch_cpu_activity(ts_ns);
        }
    }

    /// Configures declared paths for OFF_BRIEF detector. Clears any existing OFF_BRIEF stall.
    pub fn set_declared_paths(&mut self, paths: Vec<String>) {
        self.task_declared_paths = Some(paths);
        self.clear_stall(tacho::StallKind::OffBrief);
    }

    /// Credits worktree progress (git commit or file modification observed in `cwd`),
    /// resetting SILENT and SPIN stall detectors.
    pub fn record_worktree_progress(&mut self, ts_ns: u64) {
        self.touch_event(ts_ns);
        self.spin_turn_has_progress = true;
        self.spin_turns_count = 0;
        self.clear_stall(tacho::StallKind::Spin);
    }

    /// Evaluates if run ended without a passing test after file edits (CLAIM detector).
    /// Also clears run-scoped stall conditions such as OFF_BRIEF.
    pub fn check_claim(&mut self) -> Option<DetectorFiring> {
        self.clear_stall(tacho::StallKind::OffBrief);
        if self.role != AgentRole::Worker
            || self
                .active_stalls
                .contains(&(tacho::StallKind::StallClaim as i32))
        {
            return None;
        }
        if let Some(edit_ts) = self.last_edit_ts_ns {
            let needs_test = self.last_passing_test_ts_ns.is_none_or(|t| t < edit_ts);
            if needs_test && self.enter_stall(tacho::StallKind::StallClaim) {
                return Some(DetectorFiring {
                    kind: tacho::StallKind::StallClaim,
                    repeats: None,
                    since_us: None,
                    evidence: "run ended after file edits without a subsequent passing TEST"
                        .to_string(),
                });
            }
        }
        None
    }

    /// Handles run end, clearing run-scoped stall conditions such as OFF_BRIEF.
    pub fn on_run_end(&mut self) {
        self.clear_stall(tacho::StallKind::OffBrief);
    }

    /// Checks for silence timeout (SILENT detector) on tick.
    pub fn check_silent(
        &mut self,
        now_ns: u64,
        is_waiting: bool,
        is_exited: bool,
        cfg: &DetectorConfig,
    ) -> Option<DetectorFiring> {
        if is_waiting
            || is_exited
            || self.last_event_ts_ns == 0
            || self.suppress_live_silent
            || self
                .active_stalls
                .contains(&(tacho::StallKind::Silent as i32))
        {
            return None;
        }

        if let Some(last_cpu) = self.last_cpu_activity_ns {
            if now_ns.saturating_sub(last_cpu) < cfg.silent_timeout_ns {
                return None;
            }
        }

        if now_ns >= self.last_event_ts_ns + cfg.silent_timeout_ns {
            let elapsed_ns = now_ns - self.last_event_ts_ns;
            let since_us = elapsed_ns / 1_000;
            self.enter_stall(tacho::StallKind::Silent);
            Some(DetectorFiring {
                kind: tacho::StallKind::Silent,
                repeats: None,
                since_us: Some(since_us),
                evidence: format!(
                    "no event for {}s while not in wait",
                    elapsed_ns / 1_000_000_000
                ),
            })
        } else {
            None
        }
    }

    /// Evaluates tool begin for repetition (LOOP detector).
    pub fn on_tool_begin(&mut self, digest: &str, cfg: &DetectorConfig) -> Option<DetectorFiring> {
        if self.role != AgentRole::Worker {
            return None;
        }

        self.recent_tool_digests.push_back(digest.to_string());
        if self.recent_tool_digests.len() > cfg.loop_repeats {
            self.recent_tool_digests.pop_front();
        }

        if self.recent_tool_digests.len() == cfg.loop_repeats
            && self.recent_tool_digests.iter().all(|d| d == digest)
        {
            if !self.enter_stall(tacho::StallKind::Loop) {
                return None;
            }
            Some(DetectorFiring {
                kind: tacho::StallKind::Loop,
                repeats: Some(cfg.loop_repeats as u32),
                since_us: None,
                evidence: format!(
                    "tool input digest {} repeated {} times in a row",
                    digest, cfg.loop_repeats
                ),
            })
        } else {
            self.clear_stall(tacho::StallKind::Loop);
            None
        }
    }

    /// Evaluates file edits for task path compliance (OFF_BRIEF detector) and progress.
    ///
    /// Semantics of OFF_BRIEF:
    /// - Fires when a file edit occurs outside declared task paths.
    /// - Crucially, OFF_BRIEF is sticky across subsequent in-brief edits: an in-brief edit
    ///   does NOT clear an active OFF_BRIEF stall.
    /// - It clears only on run end or task change (new declared paths).
    pub fn on_file_edited(
        &mut self,
        path: &str,
        ts_ns: u64,
        cfg: &DetectorConfig,
    ) -> Option<DetectorFiring> {
        self.spin_turn_has_progress = true;
        self.last_edit_ts_ns = Some(ts_ns);
        self.clear_stall(tacho::StallKind::Spin);

        let off_brief_evidence = self.task_declared_paths.as_ref().and_then(|declared| {
            if declared.is_empty() {
                return None;
            }
            let matches_brief = declared.iter().any(|d| {
                path == d
                    || path.starts_with(&format!("{}/", d.trim_end_matches('/')))
                    || d.starts_with(&format!("{}/", path.trim_end_matches('/')))
            });
            if matches_brief {
                None
            } else {
                Some(format!(
                    "file '{}' edited outside declared task paths {:?}",
                    path, declared
                ))
            }
        });

        if let Some(evidence) = off_brief_evidence {
            if self.enter_stall(tacho::StallKind::OffBrief) {
                return Some(DetectorFiring {
                    kind: tacho::StallKind::OffBrief,
                    repeats: None,
                    since_us: None,
                    evidence,
                });
            }
        }

        let _ = cfg;
        None
    }

    /// Evaluates command termination for failures (FAILING detector) and progress.
    ///
    /// Semantics of FAILING:
    /// - Applies exclusively to commands classified as `CommandClass::Build` or `CommandClass::Test`.
    /// - Tracks failures per normalized command string (`argv.trim()`).
    /// - Fires when the same command fails `cfg.failing_repeats` consecutive times.
    /// - Cleared only when that specific command subsequently succeeds (`exit_code == Some(0)`),
    ///   resetting its consecutive failure count and removing the `Failing` stall kind.
    pub fn on_command_ended(
        &mut self,
        argv: &str,
        class: CommandClass,
        exit_code: Option<i32>,
        ts_ns: u64,
        cfg: &DetectorConfig,
    ) -> Option<DetectorFiring> {
        if matches!(
            class,
            CommandClass::Build | CommandClass::Test | CommandClass::Commit
        ) {
            self.spin_turn_has_progress = true;
            self.clear_stall(tacho::StallKind::Spin);
        }

        let is_success = exit_code == Some(0);
        if class == CommandClass::Test && is_success {
            self.last_passing_test_ts_ns = Some(ts_ns);
        } else if class == CommandClass::Commit && is_success {
            self.commits_succeeded += 1;
        } else if class == CommandClass::Push && is_success {
            self.pushes_succeeded += 1;
        }

        if matches!(class, CommandClass::Build | CommandClass::Test) {
            let failed = exit_code.map(|c| c != 0).unwrap_or(true);
            let key = argv.trim().to_string();
            if failed {
                let count = {
                    let entry = self.failing_commands.entry(key).or_insert(0);
                    *entry += 1;
                    *entry
                };
                if count >= cfg.failing_repeats && self.enter_stall(tacho::StallKind::Failing) {
                    return Some(DetectorFiring {
                        kind: tacho::StallKind::Failing,
                        repeats: Some(count as u32),
                        since_us: None,
                        evidence: format!("command '{}' failed {} times in a row", argv, count),
                    });
                }
            } else {
                self.failing_commands.insert(key, 0);
                self.clear_stall(tacho::StallKind::Failing);
            }
        }

        None
    }

    /// Evaluates turn conclusion for progress (SPIN detector) and token velocity (BURN detector).
    pub fn on_turn_end(
        &mut self,
        ts_ns: u64,
        tokens_out: Option<u64>,
        cfg: &DetectorConfig,
    ) -> Vec<DetectorFiring> {
        let mut firings = Vec::new();

        // 1. SPIN detector (only for Worker role)
        if self.spin_turn_has_progress {
            self.spin_turns_count = 0;
            self.spin_turn_has_progress = false;
            self.clear_stall(tacho::StallKind::Spin);
        } else if self.role == AgentRole::Worker {
            self.spin_turns_count += 1;
            if self.spin_turns_count >= cfg.spin_turns && self.enter_stall(tacho::StallKind::Spin) {
                firings.push(DetectorFiring {
                    kind: tacho::StallKind::Spin,
                    repeats: Some(self.spin_turns_count as u32),
                    since_us: None,
                    evidence: format!(
                        "{} turns with no edits and no build/test/commit commands",
                        self.spin_turns_count
                    ),
                });
            }
        }

        // 2. BURN detector
        if let Some(tokens) = tokens_out {
            self.burn_window.push_back((ts_ns, tokens));
            let cutoff = ts_ns.saturating_sub(cfg.burn_window_ns);
            while let Some(&(t, _)) = self.burn_window.front() {
                if t < cutoff {
                    self.burn_window.pop_front();
                } else {
                    break;
                }
            }

            let limit = self.task_token_budget.unwrap_or(cfg.burn_tokens);
            let sum: u64 = self.burn_window.iter().map(|(_, k)| *k).sum();
            if sum > limit && self.enter_stall(tacho::StallKind::Burn) {
                firings.push(DetectorFiring {
                    kind: tacho::StallKind::Burn,
                    repeats: Some(sum as u32),
                    since_us: None,
                    evidence: format!("tokens_out {} in last 10m exceeded limit {}", sum, limit),
                });
            } else if sum <= limit {
                self.clear_stall(tacho::StallKind::Burn);
            }
        }

        firings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_detector() {
        let mut state = AgentDetectorState::new();
        let cfg = DetectorConfig::default();

        let d = blake3_digest_8("cargo test");
        assert!(state.on_tool_begin(&d, &cfg).is_none());
        assert!(state.on_tool_begin(&d, &cfg).is_none());
        assert!(state.on_tool_begin(&d, &cfg).is_none());
        let firing = state
            .on_tool_begin(&d, &cfg)
            .expect("should fire on 4th repeat");
        assert_eq!(firing.kind, tacho::StallKind::Loop);
        assert_eq!(firing.repeats, Some(4));
    }

    #[test]
    fn test_spin_detector() {
        let mut state = AgentDetectorState::new();
        let cfg = DetectorConfig::default();

        for _ in 0..7 {
            assert!(state.on_turn_end(1_000, None, &cfg).is_empty());
        }
        let firings = state.on_turn_end(2_000, None, &cfg);
        assert_eq!(firings.len(), 1);
        assert_eq!(firings[0].kind, tacho::StallKind::Spin);
        assert_eq!(firings[0].repeats, Some(8));
    }

    #[test]
    fn test_failing_detector() {
        let mut state = AgentDetectorState::new();
        let cfg = DetectorConfig::default();

        assert!(state
            .on_command_ended("cargo test", CommandClass::Test, Some(1), 1_000, &cfg)
            .is_none());
        assert!(state
            .on_command_ended("cargo test", CommandClass::Test, Some(1), 2_000, &cfg)
            .is_none());
        let firing = state
            .on_command_ended("cargo test", CommandClass::Test, Some(1), 3_000, &cfg)
            .expect("should fire on 3rd failure");
        assert_eq!(firing.kind, tacho::StallKind::Failing);
        assert_eq!(firing.repeats, Some(3));
    }

    #[test]
    fn test_burn_detector() {
        let mut state = AgentDetectorState::new();
        let cfg = DetectorConfig::default();

        assert!(state.on_turn_end(1_000, Some(100_000), &cfg).is_empty());
        let firings = state.on_turn_end(2_000, Some(150_000), &cfg);
        assert_eq!(firings.len(), 1);
        assert_eq!(firings[0].kind, tacho::StallKind::Burn);
    }

    #[test]
    fn test_off_brief_detector() {
        let mut state = AgentDetectorState::new();
        let cfg = DetectorConfig::default();
        state.task_declared_paths = Some(vec!["src/main.rs".to_string(), "Cargo.toml".to_string()]);

        assert!(state.on_file_edited("src/main.rs", 1_000, &cfg).is_none());
        let firing = state
            .on_file_edited("secret/keys.pem", 2_000, &cfg)
            .expect("should fire off-brief");
        assert_eq!(firing.kind, tacho::StallKind::OffBrief);
        assert!(state
            .active_stalls
            .contains(&(tacho::StallKind::OffBrief as i32)));

        // Crucial test for Item 5: subsequent in-brief edit must NOT clear OFF_BRIEF
        assert!(state.on_file_edited("src/main.rs", 3_000, &cfg).is_none());
        assert!(state
            .active_stalls
            .contains(&(tacho::StallKind::OffBrief as i32)));

        // Clears only on task change
        state.set_declared_paths(vec![
            "src/main.rs".to_string(),
            "secret/keys.pem".to_string(),
        ]);
        assert!(!state
            .active_stalls
            .contains(&(tacho::StallKind::OffBrief as i32)));

        // Or on run end
        let _ = state.on_file_edited("other.txt", 4_000, &cfg);
        assert!(state
            .active_stalls
            .contains(&(tacho::StallKind::OffBrief as i32)));
        state.on_run_end();
        assert!(!state
            .active_stalls
            .contains(&(tacho::StallKind::OffBrief as i32)));
    }

    #[test]
    fn test_record_cpu_sample_baseline_and_activity() {
        let mut state = AgentDetectorState::new();
        state.enter_stall(tacho::StallKind::Silent);
        assert!(state.is_stalled);

        // First sample: only sets baseline, no activity, doesn't clear Silent stall
        state.record_cpu_sample(1_000_000_000, 100_000_000); // 100s cumulative CPU at t=1s
        assert!(state
            .active_stalls
            .contains(&(tacho::StallKind::Silent as i32)));
        assert_eq!(state.last_cpu_activity_ns, None);

        // Second sample with low rate (< 50ms/s = 50,000 us/s):
        // dt = 1s = 1_000_000_000ns. d_cpu = 10,000us (10ms). Rate = 10,000 us/s < 50,000 us/s.
        state.record_cpu_sample(2_000_000_000, 100_010_000);
        assert!(state
            .active_stalls
            .contains(&(tacho::StallKind::Silent as i32)));
        assert_eq!(state.last_cpu_activity_ns, None);

        // Third sample with high rate (>= 50,000 us/s):
        // dt = 1s = 1_000_000_000ns. d_cpu = 200,000us (200ms). Rate = 200,000 us/s.
        state.record_cpu_sample(3_000_000_000, 100_210_000);
        // Active CPU rate detected: touch_cpu_activity clears Silent!
        assert!(!state
            .active_stalls
            .contains(&(tacho::StallKind::Silent as i32)));
        assert!(!state.is_stalled);
        assert_eq!(state.last_cpu_activity_ns, Some(3_000_000_000));
    }

    #[test]
    fn test_active_stalls_single_source_of_truth() {
        let mut state = AgentDetectorState::new();
        assert!(!state.is_stalled);
        assert!(state.active_stalls.is_empty());

        assert!(state.enter_stall(tacho::StallKind::Spin));
        assert!(state.is_stalled);
        // Repeated enter returns false (deduplication)
        assert!(!state.enter_stall(tacho::StallKind::Spin));

        // Add second stall
        assert!(state.enter_stall(tacho::StallKind::Loop));
        assert_eq!(state.active_stalls.len(), 2);

        // Clear one stall: still stalled
        state.clear_stall(tacho::StallKind::Spin);
        assert!(state.is_stalled);
        assert_eq!(state.active_stalls.len(), 1);

        // Clear remaining stall: no longer stalled
        state.clear_stall(tacho::StallKind::Loop);
        assert!(!state.is_stalled);
        assert!(state.active_stalls.is_empty());
    }
}
