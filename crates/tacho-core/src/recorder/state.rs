//! State machine transitions for agents and tasks, and stall detector processing.

use crate::detect::DetectorFiring;
use crate::event::Event;
use crate::input::{FindingSeverity, MessageDirection, TaskStatus};
use crate::kind::Kind;
use crate::live::{AgentLiveState, TimelineItem};
use crate::scrub::truncate_utf8;
use tacho_proto::tacho;

use super::Recorder;

impl Recorder {
    /// Derives and transitions agent state, emitting `AgentState` event only on change.
    pub(crate) fn transition_state(
        &mut self,
        agent_id: &str,
        state_str: &str,
        state_enum: tacho::AgentStateType,
        because: &str,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let live_agent = self
            .live
            .agents
            .entry(agent_id.to_string())
            .or_insert_with(|| AgentLiveState::new(agent_id, "unknown", "unknown"));

        if live_agent.state == state_str {
            return;
        }

        live_agent.state = state_str.to_string();
        live_agent.state_since_ns = ts;

        let state_track = format!("agent:{}/state", agent_id);
        self.ensure_track(&state_track, ts, out);

        let state_ts = self.clamp_ts(&state_track, ts);
        let seq = self.alloc_seq();

        out.push(Event {
            seq,
            ts_ns: state_ts,
            kind: Kind::AgentState,
            track_key: state_track,
            cause_seq: None,
            agent: Some(agent_id.to_string()),
            custom_name: Some(state_str.to_string()),
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::AgentState(tacho::AgentState {
                    state: Some(state_enum as i32),
                    because: Some(truncate_utf8(because, 128)),
                })),
            },
        });
    }

    /// Processes detector firings, emitting `StallDetected` and setting state to `Stalled`.
    pub(crate) fn handle_detector_firings(
        &mut self,
        agent_id: &str,
        firings: Vec<DetectorFiring>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        for firing in firings {
            let flag_str = match firing.kind {
                tacho::StallKind::Silent => "SILENT",
                tacho::StallKind::Loop => "LOOP",
                tacho::StallKind::Spin => "SPIN",
                tacho::StallKind::Burn => "BURN",
                tacho::StallKind::Failing => "FAILING",
                tacho::StallKind::OffBrief => "OFF_BRIEF",
                _ => "STALL",
            };

            let agent_track = format!("agent:{}", agent_id);
            self.ensure_track(&agent_track, ts, out);
            let stall_ts = self.clamp_ts(&agent_track, ts);
            let seq = self.alloc_seq();

            out.push(Event {
                seq,
                ts_ns: stall_ts,
                kind: Kind::Stall,
                track_key: agent_track,
                cause_seq: None,
                agent: Some(agent_id.to_string()),
                custom_name: Some("StallDetected".to_string()),
                flow_ids: Vec::new(),
                terminating_flow_ids: Vec::new(),
                counter_value: None,
                body: tacho::Event {
                    body: Some(tacho::event::Body::Stall(tacho::Stall {
                        kind: Some(firing.kind as i32),
                        since_us: firing.since_us,
                        repeats: firing.repeats,
                        evidence: Some(truncate_utf8(&firing.evidence, 512)),
                    })),
                },
            });

            if let Some(agent) = self.live.agents.get_mut(agent_id) {
                if !agent.flags.iter().any(|f| f == flag_str) {
                    agent.flags.push(flag_str.to_string());
                }
                agent.timeline.push(TimelineItem {
                    ts_ns: ts,
                    kind: "Stall".to_string(),
                    summary: format!("{}: {}", flag_str, firing.evidence),
                    detail: None,
                });
            }

            self.transition_state(
                agent_id,
                "Stalled",
                tacho::AgentStateType::Stalled,
                &firing.evidence,
                ts,
                out,
            );
        }
    }

    /// Handles `Input::Task`.
    pub(crate) fn handle_task(
        &mut self,
        slug: String,
        brief_digest: Option<String>,
        brief_bytes: Option<u64>,
        paths: Vec<String>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let track = format!("task:{}", slug);
        self.ensure_track(&track, ts, out);
        let t_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();

        self.tasks.insert(slug.clone(), paths.clone());
        for det in self.detectors.values_mut() {
            det.task_declared_paths = Some(paths.clone());
        }

        out.push(Event {
            seq,
            ts_ns: t_ts,
            kind: Kind::TaskQueued,
            track_key: track,
            cause_seq: None,
            agent: None,
            custom_name: None,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::TaskQueued(tacho::TaskQueued {
                    slug: Some(truncate_utf8(&slug, 128)),
                    brief_digest: brief_digest.as_deref().map(|d| truncate_utf8(d, 64)),
                    brief_bytes,
                    paths: paths.into_iter().map(|p| truncate_utf8(&p, 512)).collect(),
                })),
            },
        });
    }

    /// Handles `Input::TaskState`.
    pub(crate) fn handle_task_state(
        &mut self,
        slug: String,
        state: TaskStatus,
        because: Option<String>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let track = format!("task:{}/state", slug);
        self.ensure_track(&track, ts, out);
        let t_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();

        let proto_state: tacho::TaskStatus = state.into();
        let state_str = match state {
            TaskStatus::Queued => "Queued",
            TaskStatus::InProgress => "InProgress",
            TaskStatus::Review => "Review",
            TaskStatus::Done => "Done",
            TaskStatus::Failed => "Failed",
            TaskStatus::Cancelled => "Cancelled",
        };

        let scrubbed_because = because
            .as_deref()
            .map(|b| truncate_utf8(&self.scrubber.scrub(b), 128));

        out.push(Event {
            seq,
            ts_ns: t_ts,
            kind: Kind::TaskState,
            track_key: track,
            cause_seq: None,
            agent: None,
            custom_name: Some(state_str.to_string()),
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::TaskState(tacho::TaskState {
                    state: Some(proto_state as i32),
                    because: scrubbed_because,
                })),
            },
        });
    }

    /// Handles `Input::ProviderError`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_provider_error(
        &mut self,
        provider: String,
        error_code: Option<String>,
        message: Option<String>,
        rate_limit: bool,
        retry_after_ms: Option<u64>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let track = format!("provider:{}", provider);
        self.ensure_track(&track, ts, out);
        let p_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();

        let scrubbed_msg = message
            .as_deref()
            .map(|m| truncate_utf8(&self.scrubber.scrub(m), 120));

        out.push(Event {
            seq,
            ts_ns: p_ts,
            kind: Kind::ProviderError,
            track_key: track,
            cause_seq: None,
            agent: None,
            custom_name: None,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::ProviderError(tacho::ProviderError {
                    provider: Some(truncate_utf8(&provider, 64)),
                    error_code: error_code.as_deref().map(|e| truncate_utf8(e, 64)),
                    message: scrubbed_msg,
                    rate_limit: Some(rate_limit),
                    retry_after_ms,
                })),
            },
        });
    }

    /// Handles `Input::Message`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_message(
        &mut self,
        from: String,
        to: String,
        id: Option<String>,
        bytes: Option<u64>,
        dir: Option<MessageDirection>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let track = "tacho".to_string();
        self.ensure_track(&track, ts, out);
        let m_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();

        let mut flow_ids = Vec::new();
        let mut terminating_flows = Vec::new();

        if let Some(ref mid) = id {
            match dir {
                Some(MessageDirection::Sent) => {
                    flow_ids.push(seq);
                    self.message_flows.insert(mid.clone(), seq);
                }
                Some(MessageDirection::Delivered) => {
                    if let Some(f_seq) = self.message_flows.remove(mid) {
                        terminating_flows.push(f_seq);
                    }
                }
                None => {}
            }
        }

        let proto_dir = dir.map(Into::into);
        let custom_name = match dir {
            Some(MessageDirection::Sent) => "MessageSent",
            Some(MessageDirection::Delivered) => "MessageDelivered",
            None => "Message",
        };

        out.push(Event {
            seq,
            ts_ns: m_ts,
            kind: Kind::Message,
            track_key: track,
            cause_seq: None,
            agent: None,
            custom_name: Some(custom_name.to_string()),
            flow_ids,
            terminating_flow_ids: terminating_flows,
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::Message(tacho::Message {
                    from: Some(truncate_utf8(&from, 128)),
                    to: Some(truncate_utf8(&to, 128)),
                    message_id: id.as_deref().map(|i| truncate_utf8(i, 128)),
                    bytes,
                    direction: proto_dir.map(|d: tacho::MessageDirection| d as i32),
                })),
            },
        });
    }

    /// Handles `Input::Finding`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_finding(
        &mut self,
        task: String,
        severity: FindingSeverity,
        path: Option<String>,
        line: Option<u32>,
        title: String,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let track = format!("task:{}/review", task);
        self.ensure_track(&track, ts, out);
        let r_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();

        let proto_sev: tacho::FindingSeverity = severity.into();
        let scrubbed_title = truncate_utf8(&self.scrubber.scrub(&title), 128);
        let scrubbed_path = path.as_deref().map(|p| truncate_utf8(p, 512));

        out.push(Event {
            seq,
            ts_ns: r_ts,
            kind: Kind::ReviewFinding,
            track_key: track,
            cause_seq: None,
            agent: None,
            custom_name: None,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::ReviewFinding(tacho::ReviewFinding {
                    task: Some(truncate_utf8(&task, 128)),
                    severity: Some(proto_sev as i32),
                    path: scrubbed_path,
                    line,
                    title: Some(scrubbed_title),
                })),
            },
        });
    }

    /// Restores agent state, stall flags, or cumulative counters.
    pub(crate) fn restore_state_or_counter_event(&mut self, event: &Event) {
        match event.kind {
            Kind::AgentState => {
                let segs: Vec<&str> = event.track_key.split('/').collect();
                let agent_id = segs[0].strip_prefix("agent:").unwrap_or(segs[0]);
                if let Some(agent) = self.live.agents.get_mut(agent_id) {
                    if let Some(ref name) = event.custom_name {
                        agent.state = name.clone();
                        agent.state_since_ns = event.ts_ns;
                    }
                }
            }
            Kind::Custom => {
                if let Some(val) = event.counter_value {
                    let segs: Vec<&str> = event.track_key.split('/').collect();
                    if segs.len() > 2
                        && segs[0].starts_with("agent:")
                        && segs[1].starts_with("run:")
                    {
                        let agent_id = segs[0].strip_prefix("agent:").unwrap();
                        let run_n: u32 = segs[1].strip_prefix("run:").unwrap().parse().unwrap_or(1);
                        if let Some(run) = self.runs.get_mut(&(agent_id.to_string(), run_n)) {
                            match segs[2] {
                                "tokens_in" => run.cumulative_tokens_in = val as u64,
                                "tokens_out" => {
                                    run.cumulative_tokens_out = val as u64;
                                    if let Some(agent) = self.live.agents.get_mut(agent_id) {
                                        agent.tokens_out = val as u64;
                                    }
                                }
                                "tokens_cached" => run.cumulative_tokens_cached = val as u64,
                                "cost" => run.cumulative_cost_micros = val as u64,
                                _ => {}
                            }
                        }
                    } else if segs.len() > 1 && segs[0].starts_with("provider:") {
                        let prov_name = segs[0].strip_prefix("provider:").unwrap();
                        let prov_entry = self.providers.entry(prov_name.to_string()).or_default();
                        match segs[1] {
                            "tokens_out" => prov_entry.cumulative_tokens_out = val as u64,
                            "cost" => prov_entry.cumulative_cost_micros = val as u64,
                            _ => {}
                        }
                    }
                }
            }
            Kind::TaskQueued => {
                if let Some(tacho::event::Body::TaskQueued(tq)) = &event.body.body {
                    if let Some(ref slug) = tq.slug {
                        let paths = tq.paths.clone();
                        self.tasks.insert(slug.clone(), paths.clone());
                        for det in self.detectors.values_mut() {
                            det.task_declared_paths = Some(paths.clone());
                        }
                    }
                }
            }
            Kind::Stall => {
                let segs: Vec<&str> = event.track_key.split('/').collect();
                let agent_id = segs[0].strip_prefix("agent:").unwrap_or(segs[0]);
                if let Some(agent) = self.live.agents.get_mut(agent_id) {
                    if let Some(ref custom) = event.custom_name {
                        if !agent.flags.contains(custom) {
                            agent.flags.push(custom.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
