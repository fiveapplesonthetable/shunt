//! Pure, deterministic flight recorder state machine (design.md sections 4, 5, 8).
//!
//! Converts high-level parsed `Input` events into framed Perfetto `Event` objects,
//! managing track creation, timestamp clamping, lane allocation, state derivation,
//! privacy scrubbing, stall detection, and cumulative counters.
//!
//! Pure crate: no IO, no threads, no clocks. All wall-clock timestamps are supplied as `now_ns`.

mod apply;
mod counters;
mod restore;
mod routing;
mod slices;
mod state;
mod tool;
mod tracks;

use std::collections::{HashMap, HashSet};

use tacho_proto::tacho;

use crate::detect::{AgentDetectorState, DetectorConfig};
use crate::event::Event;
use crate::input::{Input, SessionKey};
use crate::kind::Kind;
use crate::lane::LaneAllocator;
use crate::live::LiveState;
use crate::scrub::{truncate_utf8, Scrubber};
use crate::track::TrackRegistry;

pub(crate) use counters::ProviderCounters;
pub(crate) use tool::OpenToolSlice;

/// Metadata and configuration for a tracked task.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskMeta {
    pub paths: Vec<String>,
    pub agent: Option<String>,
    pub base_commit: Option<String>,
    pub token_budget: Option<u64>,
    pub cost_budget_micros: Option<u64>,
    pub last_run_seq: Option<u64>,
}

/// Internal flight recorder state for a single agent session run.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct RunState {
    pub(crate) agent_id: String,
    pub(crate) run_n: u32,
    pub(crate) provider: String,
    pub(crate) session_id: String,
    pub(crate) cwd: Option<String>,
    pub(crate) lane_allocator: LaneAllocator,
    pub(crate) open_tools: HashMap<String, OpenToolSlice>, // tool_use_id -> OpenToolSlice
    pub(crate) turn_open: bool,
    pub(crate) turn_begin_seq: Option<u64>,
    pub(crate) current_turn_n: u32,
    pub(crate) tool_calls_count: u32,
    pub(crate) files_edited_count: u32,
    pub(crate) cumulative_tokens_in: u64,
    pub(crate) cumulative_tokens_out: u64,
    pub(crate) cumulative_tokens_cached: u64,
    pub(crate) cumulative_cost_micros: u64,
    pub(crate) is_exited: bool,
}

impl RunState {
    pub(crate) fn new(
        agent_id: String,
        run_n: u32,
        provider: String,
        session_id: String,
        cwd: Option<String>,
    ) -> Self {
        let run_track = format!("agent:{}/run:{}", agent_id, run_n);
        Self {
            agent_id,
            run_n,
            provider,
            session_id,
            cwd,
            lane_allocator: LaneAllocator::new(run_track),
            open_tools: HashMap::new(),
            turn_open: false,
            turn_begin_seq: None,
            current_turn_n: 0,
            tool_calls_count: 0,
            files_edited_count: 0,
            cumulative_tokens_in: 0,
            cumulative_tokens_out: 0,
            cumulative_tokens_cached: 0,
            cumulative_cost_micros: 0,
            is_exited: false,
        }
    }
}

/// The pure flight recorder state machine.
#[derive(Debug)]
pub struct Recorder {
    pub(crate) registry: TrackRegistry,
    pub(crate) scrubber: Scrubber,
    pub(crate) detector_cfg: DetectorConfig,
    pub(crate) next_seq: u64,
    pub(crate) track_last_ts: HashMap<String, u64>,
    pub(crate) registered_tracks: HashSet<String>,
    pub(crate) sessions: HashMap<SessionKey, (String, u32)>, // session -> (agent_id, run_n)
    pub(crate) agent_runs: HashMap<String, u32>,             // agent_id -> latest run_n
    pub(crate) runs: HashMap<(String, u32), RunState>,       // (agent_id, run_n) -> RunState
    pub(crate) detectors: HashMap<String, AgentDetectorState>, // agent_id -> detector state
    pub(crate) providers: HashMap<String, ProviderCounters>, // provider -> counters
    pub(crate) tasks: HashMap<String, Vec<String>>,          // task_slug -> declared paths
    pub(crate) task_meta: HashMap<String, TaskMeta>,
    pub(crate) custom_flows: HashMap<String, u64>, // flow_name -> origin seq
    pub(crate) message_flows: HashMap<String, u64>, // message_id -> origin seq
    pub(crate) subagent_flows: HashMap<String, u64>, // tool_use_id -> origin seq
    pub(crate) last_counter_value: HashMap<String, i64>, // track_key -> last emitted counter value
    pub(crate) active_builds: i64,
    pub(crate) active_tests: i64,
    pub(crate) live: LiveState,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    /// Creates a new empty flight recorder state machine.
    pub fn new() -> Self {
        Self {
            registry: TrackRegistry::new(),
            scrubber: Scrubber::new(),
            detector_cfg: DetectorConfig::default(),
            next_seq: 1,
            track_last_ts: HashMap::new(),
            registered_tracks: HashSet::new(),
            sessions: HashMap::new(),
            agent_runs: HashMap::new(),
            runs: HashMap::new(),
            detectors: HashMap::new(),
            providers: HashMap::new(),
            tasks: HashMap::new(),
            task_meta: HashMap::new(),
            custom_flows: HashMap::new(),
            message_flows: HashMap::new(),
            subagent_flows: HashMap::new(),
            last_counter_value: HashMap::new(),
            active_builds: 0,
            active_tests: 0,
            live: LiveState::new(),
        }
    }

    /// Read-only reference to the active track registry.
    pub fn registry(&self) -> &TrackRegistry {
        &self.registry
    }

    /// Read-only reference to current live state for queries.
    pub fn live_state(&self) -> &LiveState {
        &self.live
    }

    /// Mutable reference to current live state.
    pub fn live_state_mut(&mut self) -> &mut LiveState {
        &mut self.live
    }

    /// Updates detector configuration.
    pub fn set_detector_config(&mut self, cfg: DetectorConfig) {
        self.detector_cfg = cfg;
    }

    /// Applies an input event into the state machine, appending generated `Event` items to `out`.
    pub fn apply(&mut self, input: Input, now_ns: u64, out: &mut Vec<Event>) {
        // 1. Time stamping & per-track clamp (design 8)
        let (ts, clock_stepped) = if let Some(source_ts) = input.source_ts_ns() {
            const TEN_MIN_NS: u64 = 600 * 1_000_000_000;
            let diff = source_ts.abs_diff(now_ns);
            if diff > TEN_MIN_NS {
                let delta = source_ts as i64 - now_ns as i64;
                (
                    now_ns,
                    Some((delta, "source timestamp out of bounds (±10m)".to_string())),
                )
            } else {
                (source_ts, None)
            }
        } else {
            (now_ns, None)
        };

        if let Some((delta, reason)) = clock_stepped {
            self.ensure_track("daemon", now_ns, out);
            let d_ts = self.clamp_ts("daemon", now_ns);
            let seq = self.alloc_seq();
            out.push(Event {
                seq,
                ts_ns: d_ts,
                kind: Kind::ClockStep,
                track_key: "daemon".to_string(),
                cause_seq: None,
                agent: None,
                custom_name: None,
                flow_ids: Vec::new(),
                terminating_flow_ids: Vec::new(),
                counter_value: None,
                body: tacho::Event {
                    body: Some(tacho::event::Body::ClockStep(tacho::ClockStep {
                        delta_ns: Some(delta),
                        reason: Some(truncate_utf8(&reason, 128)),
                    })),
                },
            });
        }

        self.dispatch_input(input, ts, out);
    }

    /// Periodic tick for time-based detectors (e.g. SILENT detector).
    pub fn tick(&mut self, now_ns: u64, out: &mut Vec<Event>) {
        let agent_ids: Vec<String> = self.detectors.keys().cloned().collect();
        let cfg = self.detector_cfg.clone();

        for agent_id in agent_ids {
            let (is_waiting, is_exited) = self
                .live
                .agents
                .get(&agent_id)
                .map(|a| (a.state == "Waiting", a.state == "Exited"))
                .unwrap_or((false, false));

            let firing = if let Some(det) = self.detectors.get_mut(&agent_id) {
                det.check_silent(now_ns, is_waiting, is_exited, &cfg)
            } else {
                None
            };

            if let Some(f) = firing {
                self.handle_detector_firings(&agent_id, vec![f], None, now_ns, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recorder_new_empty() {
        let rec = Recorder::new();
        assert_eq!(rec.next_seq, 1);
        assert!(rec.sessions.is_empty());
    }
}
