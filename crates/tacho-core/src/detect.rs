//! Stall detectors for autonomous coding agents (design.md section 7).
//!
//! Detectors are pure functions over agent state and event streams:
//! - SILENT: run open and no event for 300s while not in Wait
//! - LOOP: same tool input digest 4 times in a row
//! - SPIN: 8 turns with no FileEdited and no Command of class BUILD/TEST/COMMIT
//! - BURN: tokens_out over 200k in 10 minutes
//! - FAILING: same test/build command failed 3 times in a row
//! - OFF_BRIEF: a FileEdited outside declared task paths (when declared)

use std::collections::{HashMap, VecDeque};
use tacho_proto::tacho;

use crate::classify::CommandClass;

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
    pub last_event_ts_ns: u64,
    pub recent_tool_digests: VecDeque<String>,
    pub spin_turns_count: usize,
    pub spin_turn_has_progress: bool,
    pub burn_window: VecDeque<(u64, u64)>, // (ts_ns, tokens_out)
    pub failing_commands: HashMap<String, usize>, // normalized argv -> consecutive failures
    pub task_declared_paths: Option<Vec<String>>,
    pub is_stalled: bool,
}

impl AgentDetectorState {
    /// Creates a new detector state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records event arrival time and clears stalled state on non-evidence events.
    pub fn touch_event(&mut self, ts_ns: u64) {
        self.last_event_ts_ns = self.last_event_ts_ns.max(ts_ns);
    }

    /// Checks for silence timeout (SILENT detector) on tick.
    pub fn check_silent(
        &mut self,
        now_ns: u64,
        is_waiting: bool,
        is_exited: bool,
        cfg: &DetectorConfig,
    ) -> Option<DetectorFiring> {
        if is_waiting || is_exited || self.last_event_ts_ns == 0 {
            return None;
        }

        if now_ns >= self.last_event_ts_ns + cfg.silent_timeout_ns {
            let elapsed_ns = now_ns - self.last_event_ts_ns;
            let since_us = elapsed_ns / 1_000;
            self.is_stalled = true;
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
        self.recent_tool_digests.push_back(digest.to_string());
        if self.recent_tool_digests.len() > cfg.loop_repeats {
            self.recent_tool_digests.pop_front();
        }

        if self.recent_tool_digests.len() == cfg.loop_repeats
            && self.recent_tool_digests.iter().all(|d| d == digest)
        {
            self.is_stalled = true;
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
            None
        }
    }

    /// Evaluates file edits for task path compliance (OFF_BRIEF detector) and progress.
    pub fn on_file_edited(&mut self, path: &str, cfg: &DetectorConfig) -> Option<DetectorFiring> {
        self.spin_turn_has_progress = true;

        if let Some(ref declared) = self.task_declared_paths {
            if !declared.is_empty() {
                let matches_brief = declared.iter().any(|d| {
                    path == d
                        || path.starts_with(&format!("{}/", d.trim_end_matches('/')))
                        || d.starts_with(&format!("{}/", path.trim_end_matches('/')))
                });

                if !matches_brief {
                    self.is_stalled = true;
                    return Some(DetectorFiring {
                        kind: tacho::StallKind::OffBrief,
                        repeats: None,
                        since_us: None,
                        evidence: format!(
                            "file '{}' edited outside declared task paths {:?}",
                            path, declared
                        ),
                    });
                }
            }
        }

        let _ = cfg;
        None
    }

    /// Evaluates command termination for failures (FAILING detector) and progress.
    pub fn on_command_ended(
        &mut self,
        argv: &str,
        class: CommandClass,
        exit_code: Option<i32>,
        cfg: &DetectorConfig,
    ) -> Option<DetectorFiring> {
        if matches!(
            class,
            CommandClass::Build | CommandClass::Test | CommandClass::Commit
        ) {
            self.spin_turn_has_progress = true;
        }

        if matches!(class, CommandClass::Build | CommandClass::Test) {
            let failed = exit_code.map(|c| c != 0).unwrap_or(true);
            let key = argv.trim().to_string();
            if failed {
                let count = self.failing_commands.entry(key.clone()).or_insert(0);
                *count += 1;
                if *count >= cfg.failing_repeats {
                    self.is_stalled = true;
                    return Some(DetectorFiring {
                        kind: tacho::StallKind::Failing,
                        repeats: Some(*count as u32),
                        since_us: None,
                        evidence: format!("command '{}' failed {} times in a row", argv, *count),
                    });
                }
            } else {
                self.failing_commands.insert(key, 0);
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

        // 1. SPIN detector
        if self.spin_turn_has_progress {
            self.spin_turns_count = 0;
            self.spin_turn_has_progress = false;
        } else {
            self.spin_turns_count += 1;
            if self.spin_turns_count >= cfg.spin_turns {
                self.is_stalled = true;
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

            let sum: u64 = self.burn_window.iter().map(|(_, k)| *k).sum();
            if sum > cfg.burn_tokens {
                self.is_stalled = true;
                firings.push(DetectorFiring {
                    kind: tacho::StallKind::Burn,
                    repeats: Some(sum as u32),
                    since_us: None,
                    evidence: format!(
                        "tokens_out {} in last 10m exceeded limit {}",
                        sum, cfg.burn_tokens
                    ),
                });
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
            .on_command_ended("cargo test", CommandClass::Test, Some(1), &cfg)
            .is_none());
        assert!(state
            .on_command_ended("cargo test", CommandClass::Test, Some(1), &cfg)
            .is_none());
        let firing = state
            .on_command_ended("cargo test", CommandClass::Test, Some(1), &cfg)
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

        assert!(state.on_file_edited("src/main.rs", &cfg).is_none());
        let firing = state
            .on_file_edited("secret/keys.pem", &cfg)
            .expect("should fire off-brief");
        assert_eq!(firing.kind, tacho::StallKind::OffBrief);
    }
}
