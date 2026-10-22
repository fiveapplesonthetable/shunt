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

/// Deterministic Perfetto flow ordinal constants (`flow_id = cause_seq * 256 + ordinal`).
pub mod flow_ord {
    pub const STALL: u64 = 1;
    pub const SUBAGENT: u64 = 2;
    pub const MESSAGE: u64 = 3;
    pub const CUSTOM: u64 = 4;
    pub const TASK_STATE: u64 = 5;
    pub const TASK_RUN: u64 = 6;

    #[inline]
    pub const fn make(cause_seq: u64, ordinal: u64) -> u64 {
        cause_seq.saturating_mul(256).saturating_add(ordinal)
    }
}

/// Metadata and configuration for a tracked task.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskMeta {
    pub paths: Vec<String>,
    pub agent: Option<String>,
    pub base_commit: Option<String>,
    pub token_budget: Option<u64>,
    pub cost_budget_micros: Option<u64>,
    pub last_run_seq: Option<u64>,
    pub last_state_seq: Option<u64>,
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
    pub(crate) open_custom_slices: HashMap<String, Vec<Option<String>>>, // track_key -> stack of custom slice ids
    pub(crate) message_flows: HashMap<String, u64>,                      // message_id -> origin seq
    pub(crate) subagent_flows: HashMap<String, u64>, // tool_use_id -> origin seq
    pub(crate) last_counter_value: HashMap<String, i64>, // track_key -> last emitted counter value
    pub(crate) active_builds: i64,
    pub(crate) active_tests: i64,
    pub(crate) last_clock_offset_ns: i64,
    pub(crate) last_ingest_now_ns: u64,
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
            open_custom_slices: HashMap::new(),
            message_flows: HashMap::new(),
            subagent_flows: HashMap::new(),
            last_counter_value: HashMap::new(),
            active_builds: 0,
            active_tests: 0,
            last_clock_offset_ns: 0,
            last_ingest_now_ns: 0,
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
        self.last_ingest_now_ns = now_ns;
        // 1. Time stamping & per-track clamp (design 8)
        let (ts, clock_stepped) = if let Some(source_ts) = input.source_ts_ns() {
            const TEN_MIN_NS: i64 = 600 * 1_000_000_000;
            let offset = source_ts as i64 - now_ns as i64;
            let step = offset.saturating_sub(self.last_clock_offset_ns);
            if step < -TEN_MIN_NS || (offset > TEN_MIN_NS && step > TEN_MIN_NS) {
                self.last_clock_offset_ns = self.last_clock_offset_ns.saturating_add(step);
                (
                    source_ts,
                    Some((step, "source timestamp out of bounds (±10m)".to_string())),
                )
            } else {
                (source_ts, None)
            }
        } else {
            (now_ns, None)
        };

        if let Some((delta, reason)) = clock_stepped {
            self.ensure_track("daemon", ts, out);
            let d_ts = self.clamp_ts("daemon", ts);
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

    /// Updates an agent's event timestamp and marks historical replays so wall-clock ticks
    /// do not overwrite replayed state ages with `now_ns`.
    pub(crate) fn touch_agent_event(&mut self, agent_id: &str, ts_ns: u64) {
        let silent_timeout = self.detector_cfg.silent_timeout_ns;
        let now_ns = self.last_ingest_now_ns;
        let det = self.detectors.entry(agent_id.to_string()).or_default();
        det.touch_event(ts_ns);
        if now_ns > 0 {
            det.suppress_live_silent = now_ns.saturating_sub(ts_ns) >= silent_timeout;
        }
    }

    /// Records any flow origin IDs referenced by `kind`, `cause_seq`, or `parent_run_seq`
    /// into `origin_flows` so streaming encoders can attach `flow_ids` to the origin event.
    pub fn record_flow_origin(
        kind: Kind,
        cause_seq: Option<u64>,
        parent_run_seq: Option<u64>,
        origin_flows: &mut HashMap<u64, Vec<u64>>,
    ) {
        match kind {
            Kind::Stall => {
                if let Some(cs) = cause_seq {
                    origin_flows
                        .entry(cs)
                        .or_default()
                        .push(flow_ord::make(cs, flow_ord::STALL));
                }
            }
            Kind::Run => {
                if let Some(ps) = parent_run_seq {
                    origin_flows
                        .entry(ps)
                        .or_default()
                        .push(flow_ord::make(ps, flow_ord::SUBAGENT));
                }
                if let Some(cs) = cause_seq {
                    origin_flows
                        .entry(cs)
                        .or_default()
                        .push(flow_ord::make(cs, flow_ord::TASK_RUN));
                }
            }
            Kind::Message => {
                if let Some(cs) = cause_seq {
                    origin_flows
                        .entry(cs)
                        .or_default()
                        .push(flow_ord::make(cs, flow_ord::MESSAGE));
                }
            }
            Kind::Custom | Kind::CustomEnded => {
                if let Some(cs) = cause_seq {
                    origin_flows
                        .entry(cs)
                        .or_default()
                        .push(flow_ord::make(cs, flow_ord::CUSTOM));
                }
            }
            Kind::TaskState => {
                if let Some(cs) = cause_seq {
                    origin_flows
                        .entry(cs)
                        .or_default()
                        .push(flow_ord::make(cs, flow_ord::TASK_STATE));
                }
            }
            _ => {}
        }
    }

    /// Populates `flow_ids` and `terminating_flow_ids` on a single `Event` using `cause_seq * 256 + ordinal`
    /// and any downstream `origin_flows` referencing `ev.seq`.
    pub fn populate_event_flows(ev: &mut Event, origin_flows: &HashMap<u64, Vec<u64>>) {
        ev.flow_ids.clear();
        ev.terminating_flow_ids.clear();

        match ev.kind {
            Kind::Stall => {
                if let Some(cs) = ev.cause_seq {
                    ev.terminating_flow_ids
                        .push(flow_ord::make(cs, flow_ord::STALL));
                }
            }
            Kind::Run => {
                if let Some(tacho::event::Body::Run(r)) = &ev.body.body {
                    if let Some(ps) = r.parent_run_seq {
                        ev.terminating_flow_ids
                            .push(flow_ord::make(ps, flow_ord::SUBAGENT));
                    }
                    if r.task.is_some() {
                        ev.flow_ids.push(flow_ord::make(ev.seq, flow_ord::TASK_RUN));
                    }
                }
                if let Some(cs) = ev.cause_seq {
                    ev.terminating_flow_ids
                        .push(flow_ord::make(cs, flow_ord::TASK_RUN));
                }
            }
            Kind::ToolCall => {
                if ev.custom_name.as_deref() == Some("Subagent") {
                    ev.flow_ids.push(flow_ord::make(ev.seq, flow_ord::SUBAGENT));
                }
            }
            Kind::Message => {
                if let Some(tacho::event::Body::Message(m)) = &ev.body.body {
                    if m.message_id.is_some()
                        && m.direction == Some(tacho::MessageDirection::Sent as i32)
                    {
                        ev.flow_ids.push(flow_ord::make(ev.seq, flow_ord::MESSAGE));
                    }
                }
                if let Some(cs) = ev.cause_seq {
                    ev.terminating_flow_ids
                        .push(flow_ord::make(cs, flow_ord::MESSAGE));
                }
            }
            Kind::Custom | Kind::CustomEnded => {
                if let Some(cs) = ev.cause_seq {
                    ev.terminating_flow_ids
                        .push(flow_ord::make(cs, flow_ord::CUSTOM));
                }
            }
            Kind::TaskQueued => {
                ev.flow_ids
                    .push(flow_ord::make(ev.seq, flow_ord::TASK_STATE));
            }
            Kind::TaskState => {
                if let Some(cs) = ev.cause_seq {
                    ev.terminating_flow_ids
                        .push(flow_ord::make(cs, flow_ord::TASK_STATE));
                }
                if let Some(tacho::event::Body::TaskState(ts)) = &ev.body.body {
                    let is_terminal = matches!(
                        ts.state.and_then(|s| tacho::TaskStatus::try_from(s).ok()),
                        Some(
                            tacho::TaskStatus::TaskDone
                                | tacho::TaskStatus::TaskFailed
                                | tacho::TaskStatus::TaskCancelled
                        )
                    );
                    if !is_terminal {
                        ev.flow_ids
                            .push(flow_ord::make(ev.seq, flow_ord::TASK_STATE));
                    }
                }
            }
            _ => {}
        }

        if let Some(fids) = origin_flows.get(&ev.seq) {
            for &fid in fids {
                if !ev.flow_ids.contains(&fid) {
                    ev.flow_ids.push(fid);
                }
            }
        }
    }

    /// Deterministically populates `flow_ids` and `terminating_flow_ids` (`cause_seq * 256 + ordinal`)
    /// on a slice of events loaded from persistent storage.
    pub fn populate_flows(events: &mut [Event]) {
        let mut origin_flows: HashMap<u64, Vec<u64>> = HashMap::new();

        for ev in events.iter() {
            let parent_run_seq = match &ev.body.body {
                Some(tacho::event::Body::Run(r)) => r.parent_run_seq,
                _ => None,
            };
            Self::record_flow_origin(ev.kind, ev.cause_seq, parent_run_seq, &mut origin_flows);
        }

        for ev in events.iter_mut() {
            Self::populate_event_flows(ev, &origin_flows);
        }
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
