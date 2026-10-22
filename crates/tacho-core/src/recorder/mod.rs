//! Pure, deterministic flight recorder state machine (design.md sections 4, 5, 8).
//!
//! Converts high-level parsed `Input` events into framed Perfetto `Event` objects,
//! managing track creation, timestamp clamping, lane allocation, state derivation,
//! privacy scrubbing, stall detection, and cumulative counters.
//!
//! Pure crate: no IO, no threads, no clocks. All wall-clock timestamps are supplied as `now_ns`.

mod counters;
mod routing;
mod slices;
mod state;
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
pub(crate) use slices::OpenToolSlice;

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
    pub(crate) custom_flows: HashMap<String, u64>,           // flow_name -> origin seq
    pub(crate) message_flows: HashMap<String, u64>,          // message_id -> origin seq
    pub(crate) subagent_flows: HashMap<String, u64>,         // tool_use_id -> origin seq
    pub(crate) last_counter_value: HashMap<String, i64>, // track_key -> last emitted counter value
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
            custom_flows: HashMap::new(),
            message_flows: HashMap::new(),
            subagent_flows: HashMap::new(),
            last_counter_value: HashMap::new(),
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

        match input {
            Input::RunStart {
                provider,
                session_id,
                agent,
                task,
                pid,
                cwd,
                model,
                parent_session,
                ..
            } => {
                self.handle_run_start(
                    provider,
                    session_id,
                    agent,
                    task,
                    pid,
                    cwd,
                    model,
                    parent_session,
                    ts,
                    out,
                );
            }

            Input::RunEnd {
                session,
                exit_code,
                reason,
                ..
            } => {
                self.handle_run_end(session, exit_code, reason, ts, out);
            }

            Input::Prompt {
                session,
                text_len,
                digest,
                ..
            } => {
                self.handle_prompt(session, text_len, digest, ts, out);
            }

            Input::TurnStart { session, model, .. } => {
                self.handle_turn_start(session, model, ts, out);
            }

            Input::TurnEnd {
                session,
                input_tokens,
                output_tokens,
                cached_tokens,
                text_bytes,
                cost_micros,
                ..
            } => {
                self.handle_turn_end(
                    session,
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    text_bytes,
                    cost_micros,
                    ts,
                    out,
                );
            }

            Input::ToolBegin {
                session,
                tool_use_id,
                tool_name,
                input: tool_in,
                ..
            } => {
                self.handle_tool_begin(session, tool_use_id, tool_name, tool_in, ts, out);
            }

            Input::ToolEnd {
                session,
                tool_use_id,
                error,
                result_bytes,
                exit_code,
                ..
            } => {
                self.handle_tool_end(
                    session,
                    tool_use_id,
                    error,
                    result_bytes,
                    exit_code,
                    ts,
                    out,
                );
            }

            Input::Wait {
                session,
                target,
                on,
                ..
            } => {
                self.handle_wait(session, target, on, ts, out);
            }

            Input::WaitEnd {
                session, timed_out, ..
            } => {
                self.handle_wait_end(session, timed_out, ts, out);
            }

            Input::Message {
                from,
                to,
                id,
                bytes,
                dir,
                ..
            } => {
                self.handle_message(from, to, id, bytes, dir, ts, out);
            }

            Input::Compacted {
                session,
                tokens_before,
                tokens_after,
                bytes_compacted,
                ..
            } => {
                self.handle_compacted(
                    session,
                    tokens_before,
                    tokens_after,
                    bytes_compacted,
                    ts,
                    out,
                );
            }

            Input::ProviderError {
                provider,
                error_code,
                message,
                rate_limit,
                retry_after_ms,
                ..
            } => {
                self.handle_provider_error(
                    provider,
                    error_code,
                    message,
                    rate_limit,
                    retry_after_ms,
                    ts,
                    out,
                );
            }

            Input::Task {
                slug,
                brief_digest,
                brief_bytes,
                paths,
                ..
            } => {
                self.handle_task(slug, brief_digest, brief_bytes, paths, ts, out);
            }

            Input::TaskState {
                slug,
                state,
                because,
                ..
            } => {
                self.handle_task_state(slug, state, because, ts, out);
            }

            Input::Finding {
                task,
                severity,
                path,
                line,
                title,
                ..
            } => {
                self.handle_finding(task, severity, path, line, title, ts, out);
            }

            Input::Custom {
                track,
                op,
                name,
                id,
                args,
                value,
                flow_out,
                flow_in,
                ..
            } => {
                self.handle_custom(track, op, name, id, args, value, flow_out, flow_in, ts, out);
            }

            Input::CounterSample {
                track_key,
                value,
                unit,
                ..
            } => {
                self.handle_counter_sample(track_key, value, unit, ts, out);
            }
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
                self.handle_detector_firings(&agent_id, vec![f], now_ns, out);
            }
        }
    }

    /// Deterministically rebuilds an identical flight recorder state machine from logged events.
    pub fn restore<'a>(events: impl Iterator<Item = &'a Event>) -> Self {
        let mut rec = Recorder::new();
        for event in events {
            rec.apply_restored_event(event);
        }
        rec
    }

    fn apply_restored_event(&mut self, event: &Event) {
        self.next_seq = self.next_seq.max(event.seq + 1);
        self.track_last_ts
            .insert(event.track_key.clone(), event.ts_ns);
        if let Some(v) = event.counter_value {
            self.last_counter_value.insert(event.track_key.clone(), v);
        }

        match event.kind {
            Kind::TrackCreated => self.restore_track_created(event),
            Kind::Run | Kind::RunEnded => self.restore_run_event(event),
            Kind::Turn
            | Kind::TurnEnded
            | Kind::Command
            | Kind::ToolCall
            | Kind::FileAccess
            | Kind::CommandEnded
            | Kind::ToolCallEnded
            | Kind::FileAccessEnded
            | Kind::Wait
            | Kind::WaitEnded
            | Kind::FileEdited => self.restore_slice_event(event),
            Kind::AgentState | Kind::Custom | Kind::Stall | Kind::TaskQueued => {
                self.restore_state_or_counter_event(event)
            }
            _ => {}
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
