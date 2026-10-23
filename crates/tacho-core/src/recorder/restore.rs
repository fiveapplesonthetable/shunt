//! Deterministic restore of flight recorder state machine from logged events.

use crate::classify::CommandClass;
use crate::event::Event;
use crate::input::{AgentRole, SessionKey};
use crate::kind::Kind;
use crate::live::{AgentLiveState, RecentCommand};
use crate::recorder::tool::OpenToolSlice;
use crate::recorder::tracks::{command_class_name, from_proto_cmd_class};
use crate::recorder::{Recorder, RunState};
use crate::track::TrackKind;
use tacho_proto::tacho;

impl Recorder {
    /// Deterministically rebuilds an identical flight recorder state machine from logged events.
    pub fn restore<'a>(events: impl Iterator<Item = &'a Event>) -> Self {
        let mut rec = Recorder::new();
        for event in events {
            rec.apply_restored_event(event);
        }
        rec
    }

    pub(crate) fn apply_restored_event(&mut self, event: &Event) {
        self.next_seq = self.next_seq.max(event.seq + 1);
        self.track_last_ts
            .insert(event.track_key.clone(), event.ts_ns);

        if let Some(v) = event.counter_value {
            self.last_counter_value.insert(event.track_key.clone(), v);
            if event.track_key == "machine/active_builds" {
                self.active_builds = v;
            } else if event.track_key == "machine/active_tests" {
                self.active_tests = v;
            } else if event.track_key.ends_with("/cpu") {
                let segs: Vec<&str> = event.track_key.split('/').collect();
                let agent_id = segs[0].strip_prefix("agent:").unwrap_or(segs[0]);
                let cpu_us = match event.custom_name.as_deref() {
                    Some("ms") => v.saturating_mul(1_000),
                    Some("ns") => v / 1_000,
                    Some("s") => v.saturating_mul(1_000_000),
                    _ => v,
                };
                if let Some(det) = self.detectors.get_mut(agent_id) {
                    det.record_cpu_sample(event.ts_ns, cpu_us);
                }
            }
            if let Some(entry) = self.registry.get_by_key_mut(&event.track_key) {
                entry.kind = TrackKind::Counter;
            }
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
            Kind::AgentState
            | Kind::Custom
            | Kind::CustomEnded
            | Kind::Milestone
            | Kind::Stall
            | Kind::TaskQueued
            | Kind::TaskState
            | Kind::Message
            | Kind::ClockStep => self.restore_state_or_counter_event(event),
            _ => {}
        }
    }

    pub(crate) fn restore_track_created(&mut self, event: &Event) {
        if let Some(tacho::event::Body::TrackCreated(tc)) = &event.body.body {
            if let Some(ref key) = tc.key {
                self.registered_tracks.insert(key.clone());
                let default_kd = if self.last_counter_value.contains_key(key) {
                    TrackKind::Counter
                } else {
                    TrackKind::Slices
                };
                let chain = self.hierarchy_chain(key, default_kd, None);
                for (k, parent_k, name, desc, kind, unit) in chain {
                    if &k == key {
                        self.registry.register(
                            &k,
                            parent_k.as_deref(),
                            tc.name.as_deref().unwrap_or(&name),
                            desc.as_deref(),
                            kind,
                            unit.as_deref(),
                        );
                    }
                }
            }
        }
    }

    pub(crate) fn restore_run_event(&mut self, event: &Event) {
        let segs: Vec<&str> = event.track_key.split('/').collect();
        let agent_id = segs[0].strip_prefix("agent:").unwrap_or(segs[0]);
        let run_n = segs
            .get(1)
            .and_then(|s| s.strip_prefix("run:"))
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);

        match event.kind {
            Kind::Run => {
                if let Some(tacho::event::Body::Run(r)) = &event.body.body {
                    let provider = r.provider.clone().unwrap_or_else(|| "unknown".to_string());
                    let session_id = r
                        .session_id
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());
                    let sess_key = SessionKey::new(&provider, &session_id);

                    let cur = self.agent_runs.entry(agent_id.to_string()).or_insert(0);
                    *cur = (*cur).max(run_n);
                    if self.sessions.get(&sess_key).is_none_or(|p| run_n >= p.1) {
                        self.sessions
                            .insert(sess_key, (agent_id.to_string(), run_n));
                    }

                    let mut run_state = RunState::new(
                        agent_id.to_string(),
                        run_n,
                        provider.clone(),
                        session_id.clone(),
                        r.cwd.clone(),
                    );
                    run_state
                        .lane_allocator
                        .allocate_begin(event.seq, Kind::Run);

                    let live_agent = self
                        .live
                        .agents
                        .entry(agent_id.to_string())
                        .or_insert_with(|| AgentLiveState::new(agent_id, provider, session_id));
                    live_agent.run_n = run_n;
                    live_agent.run_start_ts_ns = event.ts_ns;
                    live_agent.model = r.model.clone();
                    live_agent.cwd = r.cwd.clone();
                    live_agent.task = r.task.clone();

                    self.runs.insert((agent_id.to_string(), run_n), run_state);
                    let det = self.detectors.entry(agent_id.to_string()).or_default();
                    det.role = AgentRole::infer(agent_id, r.task.as_deref());
                    det.touch_event(event.ts_ns);

                    if let Some(ps) = r.parent_run_seq {
                        self.subagent_flows.retain(|_, &mut v| v != ps);
                    }
                    if let Some(ref slug) = r.task {
                        if let Some(tmeta) = self.task_meta.get_mut(slug) {
                            if !tmeta.paths.is_empty() {
                                det.set_declared_paths(tmeta.paths.clone());
                            }
                            det.task_token_budget = tmeta.token_budget;
                            det.task_cost_budget_micros = tmeta.cost_budget_micros;
                            tmeta.last_run_seq = Some(event.seq);
                        }
                    }
                }
            }
            Kind::RunEnded => {
                if let Some(run) = self.runs.get_mut(&(agent_id.to_string(), run_n)) {
                    run.is_exited = true;
                    run.turn_open = false;
                    run.turn_begin_seq = None;
                    run.open_tools.clear();
                }
                let procs_prefix = format!("agent:{}/run:{}/procs", agent_id, run_n);
                self.open_custom_slices
                    .retain(|k, _| !k.starts_with(&procs_prefix));
                if let Some(agent) = self.live.agents.get_mut(agent_id) {
                    agent.state = "Exited".to_string();
                    agent.state_since_ns = event.ts_ns;
                }
            }
            _ => {}
        }
    }

    pub(crate) fn restore_slice_event(&mut self, event: &Event) {
        let segs: Vec<&str> = event.track_key.split('/').collect();
        let agent_id = segs[0].strip_prefix("agent:").unwrap_or(segs[0]);
        let run_n = segs
            .get(1)
            .and_then(|s| s.strip_prefix("run:"))
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);

        let det = self.detectors.entry(agent_id.to_string()).or_default();
        det.touch_event(event.ts_ns);

        let Some(run) = self.runs.get_mut(&(agent_id.to_string(), run_n)) else {
            return;
        };

        match event.kind {
            Kind::Turn => {
                run.lane_allocator.allocate_begin(event.seq, Kind::Turn);
                run.turn_open = true;
                run.turn_begin_seq = Some(event.seq);
                run.current_turn_n += 1;
                if let Some(agent) = self.live.agents.get_mut(agent_id) {
                    agent.turn_n = run.current_turn_n;
                }
            }
            Kind::TurnEnded => {
                let _ = run
                    .lane_allocator
                    .allocate_end(event.cause_seq.unwrap_or(0), Kind::Turn);
                run.turn_open = false;
                run.turn_begin_seq = None;
                if let Some(tacho::event::Body::TurnEnded(te)) = &event.body.body {
                    let cfg = self.detector_cfg.clone();
                    det.on_turn_end(event.ts_ns, te.output_tokens, &cfg);
                }
            }
            Kind::Command | Kind::ToolCall | Kind::FileAccess => {
                run.lane_allocator.allocate_begin(event.seq, event.kind);
                run.tool_calls_count += 1;
                let (tid_opt, argv, cmd_class, file_path, file_edit) = match &event.body.body {
                    Some(tacho::event::Body::Command(c)) => {
                        let class = c.class.and_then(from_proto_cmd_class);
                        if let Some(ref a) = c.argv {
                            let cfg = self.detector_cfg.clone();
                            det.on_tool_begin(&format!("argv:{}", a), &cfg);
                        }
                        (c.tool_use_id.clone(), c.argv.clone(), class, None, None)
                    }
                    Some(tacho::event::Body::FileAccess(f)) => {
                        (f.tool_use_id.clone(), None, None, f.path.clone(), None)
                    }
                    Some(tacho::event::Body::ToolCall(t)) => {
                        if event.custom_name.as_deref() == Some("Subagent") {
                            if let Some(ref tid) = t.tool_use_id {
                                self.subagent_flows.insert(tid.clone(), event.seq);
                            }
                        }
                        if let Some(ref dig) = t.input_digest {
                            let cfg = self.detector_cfg.clone();
                            det.on_tool_begin(&dig.to_string(), &cfg);
                        }
                        (t.tool_use_id.clone(), None, None, None, None)
                    }
                    _ => (None, None, None, None, None),
                };
                if let Some(tid) = tid_opt {
                    run.open_tools.insert(
                        tid.clone(),
                        OpenToolSlice {
                            slice_id: event.seq,
                            tool_use_id: tid,
                            tool_name: event.custom_name.clone().unwrap_or_default(),
                            kind: event.kind,
                            assigned_track: event.track_key.clone(),
                            argv,
                            command_class: cmd_class,
                            file_path,
                            file_edit,
                            triggered_stall: false,
                        },
                    );
                }
            }
            Kind::CommandEnded | Kind::ToolCallEnded | Kind::FileAccessEnded => {
                let begin_kind = event.kind.pairing().unwrap_or(event.kind);
                if let Some(cause) = event.cause_seq {
                    let _ = run.lane_allocator.allocate_end(cause, begin_kind);
                    let matched_slice = run
                        .open_tools
                        .iter()
                        .find(|(_, s)| s.slice_id == cause)
                        .map(|(k, v)| (k.clone(), v.clone()));
                    if let Some((k, slice)) = matched_slice {
                        run.open_tools.remove(&k);
                        if event.kind == Kind::CommandEnded {
                            let exit_code = match &event.body.body {
                                Some(tacho::event::Body::CommandEnded(ce)) => ce.exit,
                                _ => None,
                            };
                            let argv = slice.argv.unwrap_or_default();
                            let class = slice.command_class.unwrap_or(CommandClass::Command);
                            let cfg = self.detector_cfg.clone();
                            det.on_command_ended(&argv, class, exit_code, event.ts_ns, &cfg);
                            if let Some(agent) = self.live.agents.get_mut(agent_id) {
                                agent.push_recent_command(RecentCommand {
                                    argv,
                                    class: command_class_name(class).to_string(),
                                    exit_code,
                                    ts_ns: event.ts_ns,
                                });
                                if class == CommandClass::Test && exit_code == Some(0) {
                                    agent.last_passing_test_ts_ns = Some(event.ts_ns);
                                } else if class == CommandClass::Commit && exit_code == Some(0) {
                                    agent.commits_succeeded += 1;
                                } else if class == CommandClass::Push && exit_code == Some(0) {
                                    agent.pushes_succeeded += 1;
                                }
                            }
                        }
                    } else {
                        run.open_tools.retain(|_, slice| slice.slice_id != cause);
                    }
                }
            }
            Kind::Wait => {
                run.lane_allocator.allocate_begin(event.seq, Kind::Wait);
            }
            Kind::WaitEnded => {
                if let Some(cause) = event.cause_seq {
                    let _ = run.lane_allocator.allocate_end(cause, Kind::Wait);
                }
            }
            Kind::FileEdited => {
                run.files_edited_count += 1;
                let path = match &event.body.body {
                    Some(tacho::event::Body::FileEdited(fe)) => fe.path.as_deref().unwrap_or(""),
                    _ => "",
                };
                let cfg = self.detector_cfg.clone();
                det.on_file_edited(path, event.ts_ns, &cfg);
                if let Some(agent) = self.live.agents.get_mut(agent_id) {
                    agent.last_edit_ts_ns = Some(event.ts_ns);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn restore_state_or_counter_event(&mut self, event: &Event) {
        match event.kind {
            Kind::AgentState => {
                let segs: Vec<&str> = event.track_key.split('/').collect();
                let agent_id = segs[0].strip_prefix("agent:").unwrap_or(segs[0]);
                if let Some(agent) = self.live.agents.get_mut(agent_id) {
                    if let Some(ref name) = event.custom_name {
                        agent.state = name.clone();
                        agent.state_since_ns = event.ts_ns;
                        if name != "Stalled" && name != "Exited" {
                            agent.flags.clear();
                        }
                    }
                }
                if let Some(ref name) = event.custom_name {
                    if name != "Stalled" && name != "Exited" {
                        if let Some(det) = self.detectors.get_mut(agent_id) {
                            det.active_stalls.clear();
                            det.is_stalled = false;
                        }
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
                        if segs[1] == "tokens_out" {
                            prov_entry.cumulative_tokens_out = val as u64;
                        } else if segs[1] == "cost" {
                            prov_entry.cumulative_cost_micros = val as u64;
                        }
                    }
                } else if let Some(tacho::event::Body::Custom(c)) = &event.body.body {
                    if c.id.is_some() || event.track_key.ends_with("/procs") {
                        self.open_custom_slices
                            .entry(event.track_key.clone())
                            .or_default()
                            .push(c.id.clone());
                    }
                }
            }
            Kind::CustomEnded => {
                let end_id = match &event.body.body {
                    Some(tacho::event::Body::CustomEnded(ce)) => ce.id.as_deref(),
                    _ => None,
                };
                if let Some(stack) = self.open_custom_slices.get_mut(&event.track_key) {
                    if let Some(target_id) = end_id {
                        if let Some(idx) = stack
                            .iter()
                            .rposition(|open_id| open_id.as_deref() == Some(target_id))
                        {
                            stack.remove(idx);
                        }
                    } else {
                        stack.pop();
                    }
                }
                if self
                    .open_custom_slices
                    .get(&event.track_key)
                    .is_some_and(|s| s.is_empty())
                {
                    self.open_custom_slices.remove(&event.track_key);
                }
            }
            Kind::TaskQueued => {
                if let Some(tacho::event::Body::TaskQueued(tq)) = &event.body.body {
                    if let Some(ref slug) = tq.slug {
                        let paths = tq.paths.clone();
                        self.tasks.insert(slug.clone(), paths.clone());
                        let meta = self.task_meta.entry(slug.clone()).or_default();
                        meta.paths = paths.clone();
                        meta.agent = event.agent.clone();
                        meta.last_state_seq = Some(event.seq);
                        if let Some(ref aid) = event.agent {
                            self.detectors
                                .entry(aid.clone())
                                .or_default()
                                .set_declared_paths(paths.clone());
                        }
                        let bound_agents: Vec<String> = self
                            .live
                            .agents
                            .iter()
                            .filter(|(_, a)| a.task.as_deref() == Some(slug.as_str()))
                            .map(|(id, _)| id.clone())
                            .collect();
                        for aid in bound_agents {
                            self.detectors
                                .entry(aid)
                                .or_default()
                                .set_declared_paths(paths.clone());
                        }
                    }
                }
            }
            Kind::TaskState => {
                if let Some(slug) = event
                    .track_key
                    .strip_prefix("task:")
                    .and_then(|s| s.strip_suffix("/state"))
                {
                    self.task_meta
                        .entry(slug.to_string())
                        .or_default()
                        .last_state_seq = Some(event.seq);
                }
            }
            Kind::Message => {
                if let Some(tacho::event::Body::Message(m)) = &event.body.body {
                    if let Some(ref mid) = m.message_id {
                        if m.direction == Some(tacho::MessageDirection::Sent as i32) {
                            self.message_flows.insert(mid.clone(), event.seq);
                        } else if m.direction == Some(tacho::MessageDirection::Delivered as i32) {
                            self.message_flows.remove(mid);
                        }
                    }
                }
            }
            Kind::ClockStep => {
                if let Some(tacho::event::Body::ClockStep(cs)) = &event.body.body {
                    self.last_clock_offset_ns = self
                        .last_clock_offset_ns
                        .saturating_add(cs.delta_ns.unwrap_or(0));
                }
            }
            Kind::Milestone => {
                if let Some(tacho::event::Body::Milestone(m)) = &event.body.body {
                    if matches!(
                        m.name.as_deref(),
                        Some("WorktreeCommit" | "WorktreeFileChange")
                    ) {
                        let segs: Vec<&str> = event.track_key.split('/').collect();
                        let agent_id = event
                            .agent
                            .as_deref()
                            .unwrap_or_else(|| segs[0].strip_prefix("agent:").unwrap_or(segs[0]));
                        let is_commit = m.name.as_deref() == Some("WorktreeCommit");
                        if let Some(det) = self.detectors.get_mut(agent_id) {
                            det.record_worktree_progress(event.ts_ns);
                        }
                        if let Some(agent) = self.live.agents.get_mut(agent_id) {
                            if let Some(ref cname) = event.custom_name {
                                agent.last_action = cname.clone();
                            }
                            if is_commit {
                                agent.commits_succeeded += 1;
                            } else {
                                agent.last_edit_ts_ns = Some(event.ts_ns);
                            }
                        }
                    }
                }
            }
            Kind::Stall => {
                let segs: Vec<&str> = event.track_key.split('/').collect();
                let agent_id = segs[0].strip_prefix("agent:").unwrap_or(segs[0]);
                let det = self.detectors.entry(agent_id.to_string()).or_default();
                det.is_stalled = true;

                let flag_str = match &event.body.body {
                    Some(tacho::event::Body::Stall(st)) => {
                        let k = st.kind.and_then(|k| tacho::StallKind::try_from(k).ok());
                        if let Some(kind) = k {
                            det.enter_stall(kind);
                        }
                        match k {
                            Some(tacho::StallKind::Silent) => "SILENT",
                            Some(tacho::StallKind::Loop) => "LOOP",
                            Some(tacho::StallKind::Spin) => "SPIN",
                            Some(tacho::StallKind::Burn) => "BURN",
                            Some(tacho::StallKind::Failing) => "FAILING",
                            Some(tacho::StallKind::OffBrief) => "OFF_BRIEF",
                            Some(tacho::StallKind::StallClaim) => "CLAIM",
                            _ => "STALL",
                        }
                    }
                    _ => "STALL",
                };

                if let Some(agent) = self.live.agents.get_mut(agent_id) {
                    if !agent.flags.contains(&flag_str.to_string()) {
                        agent.flags.push(flag_str.to_string());
                    }
                }
            }
            _ => {}
        }
    }
}
