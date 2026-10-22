//! State machine transitions for agents and tasks, and stall detector processing.

use crate::detect::DetectorFiring;
use crate::event::Event;
use crate::input::{FindingSeverity, MessageDirection, TaskStatus};
use crate::kind::Kind;
use crate::live::{AgentLiveState, TimelineItem};
use crate::scrub::truncate_utf8;
use tacho_proto::tacho;

use super::{flow_ord, Recorder};

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

        if state_str != "Stalled" && state_str != "Exited" {
            live_agent.flags.clear();
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
        cause_slice_id: Option<u64>,
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
                tacho::StallKind::StallClaim => "CLAIM",
                _ => "STALL",
            };

            let agent_track = format!("agent:{}", agent_id);
            self.ensure_track(&agent_track, ts, out);
            let stall_ts = self.clamp_ts(&agent_track, ts);
            let seq = self.alloc_seq();
            let terminating_flows = cause_slice_id
                .map(|cs| vec![flow_ord::make(cs, flow_ord::STALL)])
                .unwrap_or_default();

            let stall_label = format!(
                "Stall [{}]: {}",
                flag_str,
                truncate_utf8(&firing.evidence, 64)
            );
            out.push(Event {
                seq,
                ts_ns: stall_ts,
                kind: Kind::Stall,
                track_key: agent_track,
                cause_seq: cause_slice_id,
                agent: Some(agent_id.to_string()),
                custom_name: Some(stall_label),
                flow_ids: Vec::new(),
                terminating_flow_ids: terminating_flows,
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
                agent.push_timeline(TimelineItem {
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_task(
        &mut self,
        slug: String,
        brief_digest: Option<String>,
        brief_bytes: Option<u64>,
        paths: Vec<String>,
        agent: Option<String>,
        base_commit: Option<String>,
        token_budget: Option<u64>,
        cost_budget_micros: Option<u64>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let track = format!("task:{}", slug);
        self.ensure_track(&track, ts, out);
        let t_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();

        self.tasks.insert(slug.clone(), paths.clone());
        let meta = self.task_meta.entry(slug.clone()).or_default();
        meta.paths = paths.clone();
        meta.agent = agent.clone();
        meta.base_commit = base_commit;
        meta.token_budget = token_budget;
        meta.cost_budget_micros = cost_budget_micros;
        meta.last_state_seq = Some(seq);

        if let Some(ref agent_id) = agent {
            let det = self.detectors.entry(agent_id.clone()).or_default();
            det.set_declared_paths(paths.clone());
            det.task_token_budget = token_budget;
            det.task_cost_budget_micros = cost_budget_micros;
        }
        // Also apply to any agent already bound to this task slug
        let bound_agents: Vec<String> = self
            .live
            .agents
            .iter()
            .filter(|(_, a)| a.task.as_deref() == Some(slug.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        for aid in bound_agents {
            let det = self.detectors.entry(aid).or_default();
            det.set_declared_paths(paths.clone());
            det.task_token_budget = token_budget;
            det.task_cost_budget_micros = cost_budget_micros;
        }

        let flow_id = flow_ord::make(seq, flow_ord::TASK_STATE);
        let task_label = if let Some(ref aid) = agent {
            format!("TaskQueued: {} ({})", truncate_utf8(&slug, 48), aid)
        } else {
            format!("TaskQueued: {}", truncate_utf8(&slug, 48))
        };

        out.push(Event {
            seq,
            ts_ns: t_ts,
            kind: Kind::TaskQueued,
            track_key: track,
            cause_seq: None,
            agent: agent.clone(),
            custom_name: Some(task_label),
            flow_ids: vec![flow_id],
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::TaskQueued(tacho::TaskQueued {
                    slug: Some(truncate_utf8(&slug, 128)),
                    brief_digest: brief_digest.as_deref().map(|d| truncate_utf8(d, 64)),
                    brief_bytes,
                    paths: paths
                        .into_iter()
                        .map(|p| truncate_utf8(&self.scrubber.scrub(&p), 512))
                        .collect(),
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

        let prev_seq = self
            .task_meta
            .entry(slug)
            .or_default()
            .last_state_seq
            .replace(seq);
        let terminating_flows = prev_seq
            .map(|ps| vec![flow_ord::make(ps, flow_ord::TASK_STATE)])
            .unwrap_or_default();
        let is_terminal = matches!(
            state,
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
        );
        let next_flows = if is_terminal {
            Vec::new()
        } else {
            vec![flow_ord::make(seq, flow_ord::TASK_STATE)]
        };

        out.push(Event {
            seq,
            ts_ns: t_ts,
            kind: Kind::TaskState,
            track_key: track,
            cause_seq: prev_seq,
            agent: None,
            custom_name: Some(state_str.to_string()),
            flow_ids: next_flows,
            terminating_flow_ids: terminating_flows,
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
        let mut cause_seq = None;

        if let Some(ref mid) = id {
            match dir {
                Some(MessageDirection::Sent) => {
                    flow_ids.push(flow_ord::make(seq, flow_ord::MESSAGE));
                    self.message_flows.insert(mid.clone(), seq);
                }
                Some(MessageDirection::Delivered) => {
                    if let Some(f_seq) = self.message_flows.remove(mid) {
                        cause_seq = Some(f_seq);
                        terminating_flows.push(flow_ord::make(f_seq, flow_ord::MESSAGE));
                    }
                }
                None => {}
            }
        }

        let proto_dir = dir.map(Into::into);
        let custom_name = match dir {
            Some(MessageDirection::Sent) => format!("MessageSent: {} -> {}", from, to),
            Some(MessageDirection::Delivered) => format!("MessageDelivered: {} -> {}", from, to),
            None => format!("Message: {} -> {}", from, to),
        };

        out.push(Event {
            seq,
            ts_ns: m_ts,
            kind: Kind::Message,
            track_key: track,
            cause_seq,
            agent: None,
            custom_name: Some(custom_name),
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
        let sev_str = match severity {
            FindingSeverity::Critical => "CRITICAL",
            FindingSeverity::Major => "MAJOR",
            FindingSeverity::Minor => "MINOR",
            FindingSeverity::Info => "INFO",
        };
        let scrubbed_title = truncate_utf8(&self.scrubber.scrub(&title), 128);
        let scrubbed_path = path
            .as_deref()
            .map(|p| truncate_utf8(&self.scrubber.scrub(p), 512));
        let finding_label = format!(
            "ReviewFinding [{}]: {}",
            sev_str,
            truncate_utf8(&scrubbed_title, 48)
        );

        out.push(Event {
            seq,
            ts_ns: r_ts,
            kind: Kind::ReviewFinding,
            track_key: track,
            cause_seq: None,
            agent: None,
            custom_name: Some(finding_label),
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
}
