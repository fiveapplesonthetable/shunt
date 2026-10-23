//! Token, cost, and custom counter management.

use std::collections::BTreeMap;

use crate::event::Event;
use crate::input::{CustomOp, SessionKey};
use crate::kind::Kind;
use crate::scrub::truncate_utf8;
use crate::track::TrackKind;
use tacho_proto::tacho;

use super::slices::make_abandoned_body;
use super::{flow_ord, Recorder};

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
                let agent_id = agent_id.to_string();
                let cpu_us = match unit.as_deref() {
                    Some("ms") => value.saturating_mul(1_000),
                    Some("ns") => value / 1_000,
                    Some("s") => value.saturating_mul(1_000_000),
                    _ => value,
                };
                let cleared_all = if let Some(det) = self.detectors.get_mut(&agent_id) {
                    det.record_cpu_sample(ts, cpu_us);
                    !det.is_stalled
                } else {
                    false
                };
                let was_stalled = if let Some(agent) = self.live.agents.get(&agent_id) {
                    agent.state == "Stalled"
                } else {
                    false
                };
                if cleared_all && was_stalled {
                    self.transition_state(
                        &agent_id,
                        "Idle",
                        tacho::AgentStateType::Idle,
                        "cpu activity",
                        ts,
                        out,
                    );
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
        if op == CustomOp::Instant
            && track.starts_with("agent:")
            && matches!(
                name.as_deref(),
                Some("WorktreeCommit" | "WorktreeFileChange" | "WorktreeHeadMoved")
            )
        {
            let segs: Vec<&str> = track.split('/').collect();
            let agent_id = segs[0]
                .strip_prefix("agent:")
                .unwrap_or(segs[0])
                .to_string();
            let run_n = self.agent_runs.get(&agent_id).copied().unwrap_or(1);
            let target_track = if self.runs.contains_key(&(agent_id.clone(), run_n)) {
                format!("agent:{}/run:{}/turns", agent_id, run_n)
            } else {
                format!("agent:{}", agent_id)
            };
            let short_hex = |s: Option<&String>| {
                s.map(|s| s.trim())
                    .filter(|s| s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    .map(|s| s[..s.len().min(8)].to_string())
            };

            // Checkout, reset, rebase and pull move HEAD without the agent making progress:
            // record the move, but give no progress credit.
            if name.as_deref() == Some("WorktreeHeadMoved") {
                let from = short_hex(args.get("from")).unwrap_or_else(|| "?".into());
                let to = short_hex(args.get("to")).unwrap_or_else(|| "?".into());
                let label = format!("HeadMoved: {} -> {}", from, to);
                let body = tacho::event::Body::Milestone(tacho::Milestone {
                    name: name.clone(),
                    description: Some(label.clone()),
                });
                self.emit(
                    &target_track,
                    Kind::Milestone,
                    ts,
                    None,
                    None,
                    Some(agent_id),
                    Some(label),
                    Some(body),
                    out,
                );
                return;
            }
            let is_commit = name.as_deref() == Some("WorktreeCommit");
            let raw_sha = args.get("sha").map(|s| s.trim());
            let short_sha = raw_sha.map(|s| {
                if s.len() >= 7 && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    s[..s.len().min(8)].to_string()
                } else {
                    let scrubbed = self.scrubber.scrub(s);
                    truncate_utf8(&scrubbed, 16)
                }
            });
            let sha = raw_sha.map(|s| {
                if s.len() >= 7 && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    s.to_string()
                } else {
                    truncate_utf8(&self.scrubber.scrub(s), 64)
                }
            });
            let detail = args
                .get("detail")
                .or_else(|| args.get("path"))
                .map(|s| truncate_utf8(&self.scrubber.scrub(s), 256));
            let custom_label = if is_commit {
                if let Some(ref s) = short_sha {
                    format!("WorktreeCommit: {}", s)
                } else {
                    "WorktreeCommit".to_string()
                }
            } else if let Some(ref d) = detail {
                format!("WorktreeFileChange: {}", truncate_utf8(d, 48))
            } else {
                "WorktreeFileChange".to_string()
            };

            let description = if is_commit { sha.or(detail) } else { detail };
            let body = tacho::event::Body::Milestone(tacho::Milestone {
                name: name.clone(),
                description,
            });
            self.emit(
                &target_track,
                Kind::Milestone,
                ts,
                None,
                None,
                Some(agent_id.clone()),
                Some(custom_label.clone()),
                Some(body),
                out,
            );

            let cleared_all = if let Some(det) = self.detectors.get_mut(&agent_id) {
                det.record_worktree_progress(ts);
                !det.is_stalled
            } else {
                false
            };
            let was_stalled = if let Some(agent) = self.live.agents.get_mut(&agent_id) {
                agent.last_action = custom_label;
                if is_commit {
                    agent.commits_succeeded += 1;
                } else {
                    agent.last_edit_ts_ns = Some(ts);
                }
                agent.state == "Stalled"
            } else {
                false
            };
            if cleared_all && was_stalled {
                self.transition_state(
                    &agent_id,
                    "Idle",
                    tacho::AgentStateType::Idle,
                    "worktree progress",
                    ts,
                    out,
                );
            }
            return;
        }

        if op == CustomOp::Instant && track == "milestones" {
            let ms_name = name
                .as_deref()
                .map(|n| truncate_utf8(&self.scrubber.scrub(n), 128));
            let desc = args
                .get("description")
                .or_else(|| args.get("detail"))
                .map(|d| truncate_utf8(&self.scrubber.scrub(d), 256));
            let body = tacho::event::Body::Milestone(tacho::Milestone {
                name: ms_name.clone(),
                description: desc,
            });
            self.emit(
                "milestones",
                Kind::Milestone,
                ts,
                None,
                None,
                None,
                ms_name,
                Some(body),
                out,
            );
            return;
        }

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
        let mut cause_seq = None;

        if let Some(ref fo) = flow_out {
            flow_ids.push(flow_ord::make(seq, flow_ord::CUSTOM));
            self.custom_flows.insert(fo.clone(), seq);
        }
        if let Some(ref fi) = flow_in {
            if let Some(orig) = self.custom_flows.remove(fi) {
                cause_seq = Some(orig);
                terminating_flows.push(flow_ord::make(orig, flow_ord::CUSTOM));
            }
        }

        let kind = match op {
            CustomOp::Begin | CustomOp::Instant | CustomOp::Counter | CustomOp::State => {
                Kind::Custom
            }
            CustomOp::End => Kind::CustomEnded,
        };

        if op == CustomOp::Begin {
            self.open_custom_slices
                .entry(track.clone())
                .or_default()
                .push(id.clone());
        } else if op == CustomOp::End {
            if let Some(stack) = self.open_custom_slices.get_mut(&track) {
                if let Some(ref target_id) = id {
                    let truncated_target = truncate_utf8(target_id, 128);
                    if let Some(idx) = stack.iter().rposition(|open_id| {
                        open_id.as_deref() == Some(target_id.as_str())
                            || open_id.as_deref() == Some(truncated_target.as_str())
                    }) {
                        stack.remove(idx);
                    }
                } else {
                    stack.pop();
                }
            }
            if self
                .open_custom_slices
                .get(&track)
                .is_some_and(|s| s.is_empty())
            {
                self.open_custom_slices.remove(&track);
            }
        }

        let proto_args: Vec<tacho::CustomArg> = args
            .into_iter()
            .map(|(k, v)| tacho::CustomArg {
                key: Some(truncate_utf8(&k, 64)),
                value: Some(truncate_utf8(&self.scrubber.scrub(&v), 512)),
            })
            .collect();

        let scrubbed_name = if op != CustomOp::Counter {
            name.as_deref()
                .map(|n| truncate_utf8(&self.scrubber.scrub(n), 128))
        } else {
            name
        };

        let custom_body = match op {
            CustomOp::End => tacho::event::Body::CustomEnded(tacho::CustomEnded {
                id: id.as_deref().map(|i| truncate_utf8(i, 128)),
                args: proto_args,
                abandoned: Some(false),
            }),
            _ => tacho::event::Body::Custom(tacho::Custom {
                name: scrubbed_name.clone(),
                id: id.as_deref().map(|i| truncate_utf8(i, 128)),
                args: proto_args,
            }),
        };

        out.push(Event {
            seq,
            ts_ns: c_ts,
            kind,
            track_key: track,
            cause_seq,
            agent: None,
            custom_name: scrubbed_name,
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

        let custom_label = match (tokens_before, tokens_after) {
            (Some(b), Some(a)) => Some(format!("Compacted ({} -> {} tokens)", b, a)),
            _ => Some("Compacted".to_string()),
        };

        out.push(Event {
            seq,
            ts_ns: c_ts,
            kind: Kind::Compacted,
            track_key: track,
            cause_seq: None,
            agent: Some(agent_id),
            custom_name: custom_label,
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
        let prompt_label = format!("Prompt ({}B)", text_len);
        self.emit(
            &track,
            Kind::Prompt,
            ts,
            None,
            None,
            Some(agent_id.clone()),
            Some(prompt_label),
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
            .map(|m| truncate_utf8(&self.scrubber.scrub(m), 128))
            .or_else(|| {
                self.live
                    .agents
                    .get(&agent_id)
                    .and_then(|a| a.model.clone())
            });
        let turn_label = if let Some(ref m) = s_model {
            format!("Turn {} ({})", turn_n, truncate_utf8(m, 32))
        } else {
            format!("Turn {}", turn_n)
        };
        let body = tacho::event::Body::Turn(tacho::Turn {
            turn_number: Some(turn_n),
            model: s_model.clone(),
        });
        self.emit(
            &track,
            Kind::Turn,
            ts,
            Some(slice_id),
            None,
            Some(agent_id.clone()),
            Some(turn_label),
            Some(body),
            out,
        );
        if let Some(agent) = self.live.agents.get_mut(&agent_id) {
            agent.turn_n = turn_n;
            if s_model.is_some() {
                agent.model = s_model;
            }
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
        self.touch_agent_event(&agent_id, ts);
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
        self.transition_state(
            &agent_id,
            "Idle",
            tacho::AgentStateType::Idle,
            "turn end",
            ts,
            out,
        );
        self.touch_agent_event(&agent_id, ts);
        let cfg = self.detector_cfg.clone();
        let firings = if let Some(det) = self.detectors.get_mut(&agent_id) {
            det.on_turn_end(ts, output_tokens, &cfg)
        } else {
            Vec::new()
        };
        if !firings.is_empty() {
            if let Some(tseq) = turn_seq {
                if let Some(ev) = out.iter_mut().rfind(|e| e.seq == tseq) {
                    ev.flow_ids.push(flow_ord::make(tseq, flow_ord::STALL));
                }
            }
            self.handle_detector_firings(&agent_id, firings, turn_seq, ts, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Input;
    use std::collections::BTreeMap;

    #[test]
    fn test_custom_slice_out_of_order_end_determinism() {
        let mut rec = Recorder::new();
        let mut out = Vec::new();
        let track = "custom_track".to_string();

        rec.apply(
            Input::Custom {
                source_ts_ns: Some(1_000_000),
                track: track.clone(),
                op: CustomOp::Begin,
                name: Some("slice_a".to_string()),
                id: Some("a".to_string()),
                args: BTreeMap::new(),
                value: None,
                flow_out: None,
                flow_in: None,
            },
            1_000_000,
            &mut out,
        );

        rec.apply(
            Input::Custom {
                source_ts_ns: Some(2_000_000),
                track: track.clone(),
                op: CustomOp::Begin,
                name: Some("slice_b".to_string()),
                id: Some("b".to_string()),
                args: BTreeMap::new(),
                value: None,
                flow_out: None,
                flow_in: None,
            },
            2_000_000,
            &mut out,
        );

        // End "a" first (out of order relative to LIFO stack)
        rec.apply(
            Input::Custom {
                source_ts_ns: Some(3_000_000),
                track: track.clone(),
                op: CustomOp::End,
                name: Some("slice_a".to_string()),
                id: Some("a".to_string()),
                args: BTreeMap::new(),
                value: None,
                flow_out: None,
                flow_in: None,
            },
            3_000_000,
            &mut out,
        );

        // End "b" second
        rec.apply(
            Input::Custom {
                source_ts_ns: Some(4_000_000),
                track: track.clone(),
                op: CustomOp::End,
                name: Some("slice_b".to_string()),
                id: Some("b".to_string()),
                args: BTreeMap::new(),
                value: None,
                flow_out: None,
                flow_in: None,
            },
            4_000_000,
            &mut out,
        );

        // open_custom_slices must be empty
        assert!(
            rec.open_custom_slices
                .get(&track)
                .is_none_or(|stack| stack.is_empty()),
            "Expected open_custom_slices to be empty, got: {:?}",
            rec.open_custom_slices
        );
        assert!(rec.open_custom_slices.is_empty());

        // Rebuild state via Recorder::restore
        let mut restored = Recorder::restore(out.iter());

        assert!(
            restored
                .open_custom_slices
                .get(&track)
                .is_none_or(|stack| stack.is_empty()),
            "Expected restored open_custom_slices to be empty, got: {:?}",
            restored.open_custom_slices
        );
        assert!(restored.open_custom_slices.is_empty());

        assert_eq!(rec.open_custom_slices, restored.open_custom_slices);
        assert_eq!(rec.next_seq, restored.next_seq);
        assert_eq!(rec.registered_tracks, restored.registered_tracks);

        // Subsequent inputs applied to both live and restored state produce identical events
        let mut live_events = Vec::new();
        let mut restored_events = Vec::new();
        let next_input = Input::Custom {
            source_ts_ns: Some(5_000_000),
            track,
            op: CustomOp::Instant,
            name: Some("instant_c".to_string()),
            id: None,
            args: BTreeMap::new(),
            value: None,
            flow_out: None,
            flow_in: None,
        };
        rec.apply(next_input.clone(), 5_000_000, &mut live_events);
        restored.apply(next_input, 5_000_000, &mut restored_events);

        assert_eq!(live_events.len(), restored_events.len());
        for (l, r) in live_events.iter().zip(restored_events.iter()) {
            assert_eq!(l.seq, r.seq);
            assert_eq!(l.ts_ns, r.ts_ns);
            assert_eq!(l.kind, r.kind);
            assert_eq!(l.track_key, r.track_key);
            assert_eq!(l.custom_name, r.custom_name);
        }
    }

    #[test]
    fn test_handle_custom_name_is_scrubbed() {
        let mut rec = Recorder::new();
        let mut out = Vec::new();
        let track = "custom_track".to_string();

        rec.apply(
            Input::Custom {
                source_ts_ns: Some(1_000_000),
                track,
                op: CustomOp::Begin,
                name: Some("password: secretpassword123".to_string()),
                id: Some("id1".to_string()),
                args: BTreeMap::new(),
                value: None,
                flow_out: None,
                flow_in: None,
            },
            1_000_000,
            &mut out,
        );

        let ev = out.iter().find(|e| e.kind == Kind::Custom).unwrap();
        assert_eq!(ev.custom_name.as_deref(), Some("password: <redacted>"));
        if let Some(tacho::event::Body::Custom(c)) = &ev.body.body {
            assert_eq!(c.name.as_deref(), Some("password: <redacted>"));
        } else {
            panic!("Expected Custom body");
        }
    }
}
