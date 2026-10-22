//! Token, cost, and custom counter management.

use std::collections::BTreeMap;

use crate::event::Event;
use crate::input::{CustomOp, SessionKey};
use crate::kind::Kind;
use crate::scrub::truncate_utf8;
use crate::track::TrackKind;
use tacho_proto::tacho;

use super::slices::make_abandoned_body;
use super::Recorder;

/// Cumulative provider counters for tokens and cost.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub(crate) struct ProviderCounters {
    pub(crate) cumulative_tokens_out: u64,
    pub(crate) cumulative_cost_micros: u64,
}

impl Recorder {
    /// Handles `Input::CounterSample`.
    pub(crate) fn handle_counter_sample(
        &mut self,
        track_key: String,
        value: i64,
        unit: Option<String>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        if self.last_counter_value.get(&track_key) == Some(&value) {
            return;
        }
        let prev_val = self.last_counter_value.insert(track_key.clone(), value);
        if value > prev_val.unwrap_or(i64::MIN) && track_key.ends_with("/cpu") {
            if let Some(agent_id) = track_key
                .strip_prefix("agent:")
                .and_then(|s| s.split('/').next())
            {
                if let Some(det) = self.detectors.get_mut(agent_id) {
                    det.touch_cpu_activity(ts);
                }
            }
        }
        self.ensure_track_with_kind(&track_key, TrackKind::Counter, unit.as_deref(), ts, out);
        let c_ts = self.clamp_ts(&track_key, ts);
        let seq = self.alloc_seq();

        out.push(Event {
            seq,
            ts_ns: c_ts,
            kind: Kind::Custom,
            track_key,
            cause_seq: None,
            agent: None,
            custom_name: unit,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: Some(value),
            body: tacho::Event { body: None },
        });
    }

    fn emit_counter_event(
        &mut self,
        track: String,
        agent: Option<String>,
        name: &str,
        val: i64,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        self.ensure_track(&track, ts, out);
        let c_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();
        out.push(Event {
            seq,
            ts_ns: c_ts,
            kind: Kind::Custom,
            track_key: track,
            cause_seq: None,
            agent,
            custom_name: Some(name.to_string()),
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: Some(val),
            body: tacho::Event { body: None },
        });
    }

    /// Emits cumulative token and cost counters at turn completion.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit_turn_counters(
        &mut self,
        agent_id: &str,
        run_n: u32,
        provider_name: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        cost_micros: Option<u64>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let run_key = (agent_id.to_string(), run_n);
        let (c_tokens_in, c_tokens_out, c_tokens_cached, c_cost_micros) =
            if let Some(run) = self.runs.get_mut(&run_key) {
                if let Some(tin) = input_tokens {
                    run.cumulative_tokens_in += tin;
                }
                if let Some(tout) = output_tokens {
                    run.cumulative_tokens_out += tout;
                }
                if let Some(tcached) = cached_tokens {
                    run.cumulative_tokens_cached += tcached;
                }
                if let Some(cost) = cost_micros {
                    run.cumulative_cost_micros += cost;
                }
                (
                    input_tokens.map(|_| run.cumulative_tokens_in),
                    output_tokens.map(|_| run.cumulative_tokens_out),
                    cached_tokens.map(|_| run.cumulative_tokens_cached),
                    cost_micros.map(|_| run.cumulative_cost_micros),
                )
            } else {
                (None, None, None, None)
            };

        for (val, name) in [
            (c_tokens_in, "tokens_in"),
            (c_tokens_cached, "tokens_cached"),
            (c_cost_micros, "cost"),
        ] {
            if let Some(v) = val {
                self.emit_counter_event(
                    format!("agent:{}/run:{}/{}", agent_id, run_n, name),
                    Some(agent_id.to_string()),
                    name,
                    v as i64,
                    ts,
                    out,
                );
            }
        }
        if let Some(cum_out) = c_tokens_out {
            self.emit_counter_event(
                format!("agent:{}/run:{}/tokens_out", agent_id, run_n),
                Some(agent_id.to_string()),
                "tokens_out",
                cum_out as i64,
                ts,
                out,
            );
            let cum_prov_out = {
                let prov_entry = self.providers.entry(provider_name.to_string()).or_default();
                prov_entry.cumulative_tokens_out += output_tokens.unwrap_or(0);
                prov_entry.cumulative_tokens_out
            };
            self.emit_counter_event(
                format!("provider:{}/tokens_out", provider_name),
                None,
                "tokens_out",
                cum_prov_out as i64,
                ts,
                out,
            );
        }
    }

    /// Handles `Input::Custom`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_custom(
        &mut self,
        track: String,
        op: CustomOp,
        name: Option<String>,
        id: Option<String>,
        args: BTreeMap<String, String>,
        value: Option<i64>,
        flow_out: Option<String>,
        flow_in: Option<String>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let custom_kind = match op {
            CustomOp::Counter => TrackKind::Counter,
            CustomOp::State => TrackKind::State,
            _ => TrackKind::Slices,
        };
        self.ensure_track_with_kind(&track, custom_kind, None, ts, out);
        let c_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();

        let mut flow_ids = Vec::new();
        let mut terminating_flows = Vec::new();

        if let Some(ref fo) = flow_out {
            flow_ids.push(seq);
            self.custom_flows.insert(fo.clone(), seq);
        }
        if let Some(ref fi) = flow_in {
            if let Some(orig) = self.custom_flows.remove(fi) {
                terminating_flows.push(orig);
            }
        }

        let kind = match op {
            CustomOp::Begin | CustomOp::Instant | CustomOp::Counter | CustomOp::State => {
                Kind::Custom
            }
            CustomOp::End => Kind::CustomEnded,
        };

        let proto_args: Vec<tacho::CustomArg> = args
            .into_iter()
            .map(|(k, v)| tacho::CustomArg {
                key: Some(truncate_utf8(&k, 64)),
                value: Some(truncate_utf8(&self.scrubber.scrub(&v), 512)),
            })
            .collect();

        let custom_body = match op {
            CustomOp::End => tacho::event::Body::CustomEnded(tacho::CustomEnded {
                id: id.as_deref().map(|i| truncate_utf8(i, 128)),
                args: proto_args,
                abandoned: Some(false),
            }),
            _ => tacho::event::Body::Custom(tacho::Custom {
                name: name.as_deref().map(|n| truncate_utf8(n, 128)),
                id: id.as_deref().map(|i| truncate_utf8(i, 128)),
                args: proto_args,
            }),
        };

        out.push(Event {
            seq,
            ts_ns: c_ts,
            kind,
            track_key: track,
            cause_seq: None,
            agent: None,
            custom_name: name,
            flow_ids,
            terminating_flow_ids: terminating_flows,
            counter_value: value,
            body: tacho::Event {
                body: Some(custom_body),
            },
        });
    }

    /// Handles `Input::Compacted`.
    pub(crate) fn handle_compacted(
        &mut self,
        session: SessionKey,
        tokens_before: Option<u64>,
        tokens_after: Option<u64>,
        bytes_compacted: Option<u64>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (agent_id, run_n) = self.resolve_session(&session, ts, out);
        let track = format!("agent:{}/run:{}/turns", agent_id, run_n);
        self.ensure_track(&track, ts, out);
        let c_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();

        out.push(Event {
            seq,
            ts_ns: c_ts,
            kind: Kind::Compacted,
            track_key: track,
            cause_seq: None,
            agent: Some(agent_id),
            custom_name: None,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::Compacted(tacho::Compacted {
                    tokens_before,
                    tokens_after,
                    bytes_compacted,
                })),
            },
        });
    }

    /// Handles `Input::Prompt`.
    pub(crate) fn handle_prompt(
        &mut self,
        session: SessionKey,
        text_len: u64,
        digest: Option<String>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (agent_id, run_n) = self.resolve_session(&session, ts, out);
        let track = format!("agent:{}/run:{}/turns", agent_id, run_n);
        let body = tacho::event::Body::Prompt(tacho::Prompt {
            digest: digest.as_deref().map(|d| truncate_utf8(d, 64)),
            bytes: Some(text_len),
        });
        self.emit(
            &track,
            Kind::Prompt,
            ts,
            None,
            None,
            Some(agent_id.clone()),
            None,
            Some(body),
            out,
        );
        if let Some(agent) = self.live.agents.get_mut(&agent_id) {
            agent.last_action = format!("Prompt ({} bytes)", text_len);
        }
    }

    /// Handles `Input::TurnStart`.
    pub(crate) fn handle_turn_start(
        &mut self,
        session: SessionKey,
        model: Option<String>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (agent_id, run_n) = self.resolve_session(&session, ts, out);
        let run_key = (agent_id.clone(), run_n);
        let track = format!("agent:{}/run:{}/turns", agent_id, run_n);
        let slice_id = self.alloc_seq();
        let turn_n = if let Some(run) = self.runs.get_mut(&run_key) {
            run.lane_allocator.allocate_begin(slice_id, Kind::Turn);
            run.turn_open = true;
            run.turn_begin_seq = Some(slice_id);
            run.current_turn_n += 1;
            run.current_turn_n
        } else {
            1
        };
        let s_model = model
            .as_deref()
            .map(|m| truncate_utf8(&self.scrubber.scrub(m), 128));
        let body = tacho::event::Body::Turn(tacho::Turn {
            turn_number: Some(turn_n),
            model: s_model,
        });
        self.emit(
            &track,
            Kind::Turn,
            ts,
            Some(slice_id),
            None,
            Some(agent_id.clone()),
            None,
            Some(body),
            out,
        );
        if let Some(agent) = self.live.agents.get_mut(&agent_id) {
            agent.turn_n = turn_n;
            agent.last_action = format!("Turn {}", turn_n);
        }
        self.transition_state(
            &agent_id,
            "Thinking",
            tacho::AgentStateType::Thinking,
            "turn begin",
            ts,
            out,
        );
        if let Some(det) = self.detectors.get_mut(&agent_id) {
            det.touch_event(ts);
        }
    }

    /// Handles `Input::TurnEnd`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_turn_end(
        &mut self,
        session: SessionKey,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        text_bytes: Option<u64>,
        cost_micros: Option<u64>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (agent_id, run_n) = self.resolve_session(&session, ts, out);
        let run_key = (agent_id.clone(), run_n);
        let track = format!("agent:{}/run:{}/turns", agent_id, run_n);
        let (turn_seq, provider_name, abandoned_turns_tool) =
            if let Some(run) = self.runs.get_mut(&run_key) {
                let ab_tool = run.lane_allocator.drain_turns_tool();
                if let Some(ref ab) = ab_tool {
                    run.open_tools
                        .retain(|_, slice| slice.slice_id != ab.slice_id);
                }
                let _ = run
                    .lane_allocator
                    .allocate_end(run.turn_begin_seq.unwrap_or(0), Kind::Turn);
                run.turn_open = false;
                (run.turn_begin_seq.take(), run.provider.clone(), ab_tool)
            } else {
                (None, "unknown".to_string(), None)
            };
        if let Some(ab) = abandoned_turns_tool {
            let ab_kind = ab.kind.pairing().unwrap_or(ab.kind);
            self.emit(
                &ab.track_key,
                ab_kind,
                ts,
                None,
                Some(ab.slice_id),
                Some(agent_id.clone()),
                None,
                Some(make_abandoned_body(ab_kind)),
                out,
            );
        }
        let body = tacho::event::Body::TurnEnded(tacho::TurnEnded {
            input_tokens,
            output_tokens,
            cached_tokens,
            text_bytes,
            cost_usd_micros: cost_micros,
        });
        self.emit(
            &track,
            Kind::TurnEnded,
            ts,
            None,
            turn_seq,
            Some(agent_id.clone()),
            None,
            Some(body),
            out,
        );
        self.emit_turn_counters(
            &agent_id,
            run_n,
            &provider_name,
            input_tokens,
            output_tokens,
            cached_tokens,
            cost_micros,
            ts,
            out,
        );
        if let Some(agent) = self.live.agents.get_mut(&agent_id) {
            if let Some(tin) = input_tokens {
                agent.tokens_in += tin;
            }
            if let Some(tout) = output_tokens {
                agent.tokens_out += tout;
            }
            if let Some(tcached) = cached_tokens {
                agent.tokens_cached += tcached;
            }
            if let Some(cost) = cost_micros {
                agent.cost_micros += cost;
            }
        }
        let is_stalled = self
            .detectors
            .get(&agent_id)
            .map(|d| d.is_stalled)
            .unwrap_or(false);
        let (next_st, next_enum) = if is_stalled {
            ("Stalled", tacho::AgentStateType::Stalled)
        } else {
            ("Idle", tacho::AgentStateType::Idle)
        };
        self.transition_state(&agent_id, next_st, next_enum, "turn end", ts, out);
        let cfg = self.detector_cfg.clone();
        let firings = if let Some(det) = self.detectors.get_mut(&agent_id) {
            det.touch_event(ts);
            det.on_turn_end(ts, output_tokens, &cfg)
        } else {
            Vec::new()
        };
        if !firings.is_empty() {
            if let Some(tseq) = turn_seq {
                if let Some(ev) = out.iter_mut().rfind(|e| e.seq == tseq) {
                    ev.flow_ids.push(tseq);
                }
            }
            self.handle_detector_firings(&agent_id, firings, turn_seq, ts, out);
        }
    }
}
