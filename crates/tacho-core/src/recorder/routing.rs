//! Session routing and agent run lifecycle management.

use crate::event::Event;
use crate::input::{AgentRole, SessionKey, WaitOn};
use crate::kind::Kind;
use crate::live::AgentLiveState;
use crate::scrub::{relativize_cwd, truncate_utf8};
use tacho_proto::tacho;

use super::flow_ord;
use super::slices::make_abandoned_body;
use super::{Recorder, RunState};

impl Recorder {
    /// Resolves agent_id and run_n for a session, auto-starting if unknown.
    pub(crate) fn resolve_session(
        &mut self,
        session: &SessionKey,
        now_ns: u64,
        out: &mut Vec<Event>,
    ) -> (String, u32) {
        if let Some(pair) = self.sessions.get(session).cloned() {
            return pair;
        }

        let prefix_len = std::cmp::min(8, session.session_id.len());
        let agent_id = format!("{}-{}", session.provider, &session.session_id[..prefix_len]);
        let run_n = {
            let count = self.agent_runs.entry(agent_id.clone()).or_insert(0);
            *count += 1;
            *count
        };

        self.sessions
            .insert(session.clone(), (agent_id.clone(), run_n));
        self.start_run_internal(
            &agent_id,
            run_n,
            &session.provider,
            &session.session_id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            now_ns,
            out,
        );

        (agent_id, run_n)
    }

    /// Internal logic to start a new Run slice and register its tracks.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_run_internal(
        &mut self,
        agent_id: &str,
        run_n: u32,
        provider: &str,
        session_id: &str,
        task: Option<String>,
        pid: Option<u32>,
        cwd: Option<String>,
        model: Option<String>,
        parent_session: Option<String>,
        git_tree: Option<String>,
        role: Option<AgentRole>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let key = (agent_id.to_string(), run_n);
        if self.runs.contains_key(&key) {
            return;
        }

        let effective_role = role.unwrap_or_else(|| AgentRole::infer(agent_id, task.as_deref()));

        let mut run_state = RunState::new(
            agent_id.to_string(),
            run_n,
            provider.to_string(),
            session_id.to_string(),
            cwd.clone(),
        );

        let run_track = format!("agent:{}/run:{}", agent_id, run_n);
        self.ensure_track(&run_track, ts, out);
        self.ensure_track(&format!("agent:{}/state", agent_id), ts, out);

        let slice_id = self.alloc_seq();
        run_state.lane_allocator.allocate_begin(slice_id, Kind::Run);

        let mut terminating_flows = Vec::new();
        let mut flow_ids = Vec::new();
        let mut parent_run_seq = None;
        let mut cause_seq = None;

        if let Some(ref parent) = parent_session {
            if let Some(p_seq) = self.subagent_flows.remove(parent) {
                parent_run_seq = Some(p_seq);
                terminating_flows.push(flow_ord::make(p_seq, flow_ord::SUBAGENT));
            }
        }
        if let Some(ref slug) = task {
            flow_ids.push(flow_ord::make(slice_id, flow_ord::TASK_RUN));
            if let Some(tmeta) = self.task_meta.get_mut(slug) {
                let det = self.detectors.entry(agent_id.to_string()).or_default();
                det.set_declared_paths(tmeta.paths.clone());
                det.task_token_budget = tmeta.token_budget;
                det.task_cost_budget_micros = tmeta.cost_budget_micros;
                if let Some(prev_seq) = tmeta.last_run_seq {
                    cause_seq = Some(prev_seq);
                    terminating_flows.push(flow_ord::make(prev_seq, flow_ord::TASK_RUN));
                }
                tmeta.last_run_seq = Some(slice_id);
            }
        }

        let run_ts = self.clamp_ts(&run_track, ts);
        let scrubbed_task = task
            .as_deref()
            .map(|t| truncate_utf8(&self.scrubber.scrub(t), 128));
        let scrubbed_cwd = cwd
            .as_deref()
            .map(|c| truncate_utf8(&self.scrubber.scrub(&relativize_cwd(c, None)), 512));
        let scrubbed_model = model
            .as_deref()
            .map(|m| truncate_utf8(&self.scrubber.scrub(m), 128));

        out.push(Event {
            seq: slice_id,
            ts_ns: run_ts,
            kind: Kind::Run,
            track_key: run_track,
            cause_seq,
            agent: Some(agent_id.to_string()),
            custom_name: None,
            flow_ids,
            terminating_flow_ids: terminating_flows,
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::Run(tacho::Run {
                    provider: Some(truncate_utf8(provider, 64)),
                    session_id: Some(truncate_utf8(session_id, 128)),
                    task: scrubbed_task.clone(),
                    cwd: scrubbed_cwd.clone(),
                    model: scrubbed_model.clone(),
                    parent_run_seq,
                    resume: Some(false),
                    pid,
                    git_tree: git_tree.as_deref().map(|g| truncate_utf8(g, 64)),
                })),
            },
        });

        // Initialize or update live state
        let live_agent = self
            .live
            .agents
            .entry(agent_id.to_string())
            .or_insert_with(|| AgentLiveState::new(agent_id, provider, session_id));
        live_agent.run_n = run_n;
        live_agent.run_start_ts_ns = run_ts;
        live_agent.model = scrubbed_model;
        live_agent.cwd = scrubbed_cwd;
        live_agent.task = scrubbed_task;

        self.runs.insert(key, run_state);
        {
            let det = self.detectors.entry(agent_id.to_string()).or_default();
            det.role = effective_role;
        }
        self.touch_agent_event(agent_id, ts);

        // Derive initial Idle state
        self.transition_state(
            agent_id,
            "Idle",
            tacho::AgentStateType::Idle,
            "run started",
            ts,
            out,
        );
    }

    /// Handles `Input::RunStart`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_run_start(
        &mut self,
        provider: String,
        session_id: String,
        agent: Option<String>,
        task: Option<String>,
        pid: Option<u32>,
        cwd: Option<String>,
        model: Option<String>,
        parent_session: Option<String>,
        git_tree: Option<String>,
        role: Option<AgentRole>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let prefix_len = std::cmp::min(8, session_id.len());
        let agent_id =
            agent.unwrap_or_else(|| format!("{}-{}", provider, &session_id[..prefix_len]));
        let run_n = {
            let count = self.agent_runs.entry(agent_id.clone()).or_insert(0);
            *count += 1;
            *count
        };
        let sess_key = SessionKey::new(&provider, &session_id);
        self.sessions.insert(sess_key, (agent_id.clone(), run_n));

        self.start_run_internal(
            &agent_id,
            run_n,
            &provider,
            &session_id,
            task,
            pid,
            cwd,
            model,
            parent_session,
            git_tree,
            role,
            ts,
            out,
        );
    }

    /// Handles `Input::RunEnd`.
    pub(crate) fn handle_run_end(
        &mut self,
        session: SessionKey,
        exit_code: Option<i32>,
        reason: Option<String>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (agent_id, run_n) = self.resolve_session(&session, ts, out);
        let run_key = (agent_id.clone(), run_n);

        // Drain abandoned open slices before closing the run
        let (abandoned, turn_n, tool_calls_count, files_edited_count, abandoned_tools) =
            if let Some(run) = self.runs.get_mut(&run_key) {
                let abandoned: Vec<_> = run
                    .lane_allocator
                    .drain_abandoned()
                    .into_iter()
                    .filter(|ab| ab.kind != Kind::Run)
                    .collect();
                run.is_exited = true;
                run.turn_open = false;
                run.turn_begin_seq = None;
                let mut tools: Vec<_> = run.open_tools.values().cloned().collect();
                tools.sort_by_key(|t| t.slice_id);
                run.open_tools.clear();
                (
                    abandoned,
                    run.current_turn_n,
                    run.tool_calls_count,
                    run.files_edited_count,
                    tools,
                )
            } else {
                (Vec::new(), 0, 0, 0, Vec::new())
            };

        for tool in abandoned_tools {
            self.update_active_cmd_counter(tool.command_class, -1, ts, out);
        }

        for ab in abandoned {
            let ab_ts = self.clamp_ts(&ab.track_key, ts);
            let ab_seq = self.alloc_seq();
            let ab_kind = ab.kind.pairing().unwrap_or(ab.kind);

            let ab_body = make_abandoned_body(ab_kind);

            out.push(Event {
                seq: ab_seq,
                ts_ns: ab_ts,
                kind: ab_kind,
                track_key: ab.track_key,
                cause_seq: Some(ab.slice_id),
                agent: Some(agent_id.clone()),
                custom_name: None,
                flow_ids: Vec::new(),
                terminating_flow_ids: Vec::new(),
                counter_value: None,
                body: tacho::Event {
                    body: Some(ab_body),
                },
            });
        }

        // Close any process slices the sampler left open on this run's `procs/slot:*` tracks.
        let procs_prefix = format!("agent:{}/run:{}/procs", agent_id, run_n);
        let mut proc_tracks: Vec<String> = self
            .open_custom_slices
            .keys()
            .filter(|k| k.starts_with(&procs_prefix))
            .cloned()
            .collect();
        proc_tracks.sort();
        for procs_track in proc_tracks {
            let Some(mut stack) = self.open_custom_slices.remove(&procs_track) else {
                continue;
            };
            while let Some(custom_id) = stack.pop() {
                let ab_ts = self.clamp_ts(&procs_track, ts);
                let ab_seq = self.alloc_seq();
                out.push(Event {
                    seq: ab_seq,
                    ts_ns: ab_ts,
                    kind: Kind::CustomEnded,
                    track_key: procs_track.clone(),
                    cause_seq: None,
                    agent: Some(agent_id.clone()),
                    custom_name: None,
                    flow_ids: Vec::new(),
                    terminating_flow_ids: Vec::new(),
                    counter_value: None,
                    body: tacho::Event {
                        body: Some(tacho::event::Body::CustomEnded(tacho::CustomEnded {
                            id: custom_id.as_deref().map(|i| truncate_utf8(i, 128)),
                            args: Vec::new(),
                            abandoned: Some(true),
                        })),
                    },
                });
            }
        }

        let track = format!("agent:{}/run:{}", agent_id, run_n);
        let run_ts = self.clamp_ts(&track, ts);
        let seq = self.alloc_seq();
        let scrubbed_reason = reason
            .as_deref()
            .map(|r| truncate_utf8(&self.scrubber.scrub(r), 256));

        let exit_because = scrubbed_reason
            .clone()
            .unwrap_or_else(|| "run ended".to_string());

        out.push(Event {
            seq,
            ts_ns: run_ts,
            kind: Kind::RunEnded,
            track_key: track,
            cause_seq: None,
            agent: Some(agent_id.clone()),
            custom_name: None,
            flow_ids: Vec::new(),
            terminating_flow_ids: Vec::new(),
            counter_value: None,
            body: tacho::Event {
                body: Some(tacho::event::Body::RunEnded(tacho::RunEnded {
                    reason: scrubbed_reason,
                    exit_code,
                    turns: Some(turn_n),
                    tool_calls: Some(tool_calls_count),
                    files_edited: Some(files_edited_count),
                })),
            },
        });

        if exit_code.unwrap_or(0) == 0 {
            let firing = if let Some(det) = self.detectors.get_mut(&agent_id) {
                det.check_claim()
            } else {
                None
            };
            if let Some(f) = firing {
                self.handle_detector_firings(&agent_id, vec![f], None, ts, out);
            }
        }

        self.transition_state(
            &agent_id,
            "Exited",
            tacho::AgentStateType::Exited,
            &exit_because,
            ts,
            out,
        );
    }

    /// Handles `Input::Wait`.
    pub(crate) fn handle_wait(
        &mut self,
        session: SessionKey,
        target: Option<String>,
        on: Option<WaitOn>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (agent_id, run_n) = self.resolve_session(&session, ts, out);
        let track = format!("agent:{}/run:{}/waits", agent_id, run_n);
        let slice_id = self.alloc_seq();
        if let Some(run) = self.runs.get_mut(&(agent_id.clone(), run_n)) {
            run.lane_allocator.allocate_begin(slice_id, Kind::Wait);
        }
        let proto_on = on.map(Into::into);
        let s_target = target
            .as_deref()
            .map(|t| truncate_utf8(&self.scrubber.scrub(t), 128));
        let on_str = match on {
            Some(WaitOn::Input) => Some("input"),
            Some(WaitOn::Message) => Some("message"),
            Some(WaitOn::Subagent) => Some("subagent"),
            Some(WaitOn::RateLimit) => Some("rate_limit"),
            Some(WaitOn::Other) => Some("other"),
            None => None,
        };
        let wait_label = match (on_str, s_target.as_deref()) {
            (Some(o), Some(t)) if !t.is_empty() => {
                Some(format!("Wait: {} {}", o, truncate_utf8(t, 48)))
            }
            (None, Some(t)) if !t.is_empty() => Some(format!("Wait: {}", truncate_utf8(t, 48))),
            (Some(o), _) => Some(format!("Wait: {}", o)),
            (None, _) => None,
        };
        let body = tacho::event::Body::Wait(tacho::Wait {
            target: s_target,
            on: proto_on.map(|o: tacho::WaitOn| o as i32),
        });
        self.emit(
            &track,
            Kind::Wait,
            ts,
            Some(slice_id),
            None,
            Some(agent_id.clone()),
            wait_label,
            Some(body),
            out,
        );
        self.transition_state(
            &agent_id,
            "Waiting",
            tacho::AgentStateType::Waiting,
            "wait begin",
            ts,
            out,
        );
    }

    /// Handles `Input::WaitEnd`.
    pub(crate) fn handle_wait_end(
        &mut self,
        session: SessionKey,
        timed_out: bool,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (agent_id, run_n) = self.resolve_session(&session, ts, out);
        let track = format!("agent:{}/run:{}/waits", agent_id, run_n);
        let body = tacho::event::Body::WaitEnded(tacho::WaitEnded {
            timed_out: Some(timed_out),
            abandoned: Some(false),
        });
        self.emit(
            &track,
            Kind::WaitEnded,
            ts,
            None,
            None,
            Some(agent_id.clone()),
            None,
            Some(body),
            out,
        );
        let is_turn_open = self
            .runs
            .get(&(agent_id.clone(), run_n))
            .map(|r| r.turn_open)
            .unwrap_or(false);
        let (next_st, next_enum) = if is_turn_open {
            ("Thinking", tacho::AgentStateType::Thinking)
        } else {
            ("Idle", tacho::AgentStateType::Idle)
        };
        self.transition_state(&agent_id, next_st, next_enum, "wait ended", ts, out);
    }
}
