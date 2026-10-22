//! Slice lifecycle (Turn, Tool, Wait, Prompt) and lane allocation.

use crate::classify::CommandClass;
use crate::event::Event;
use crate::input::{FileEditKind, SessionKey, ToolInput};
use crate::kind::Kind;
use crate::lane::LaneError;
use crate::live::RecentCommand;
use crate::scrub::truncate_utf8;
use tacho_proto::tacho;

pub(crate) use super::tool::{make_abandoned_body, make_tool_end_body, OpenToolSlice};
use super::tracks::{command_class_name, to_proto_edit_kind};
use super::Recorder;

impl Recorder {
    pub(crate) fn update_active_cmd_counter(
        &mut self,
        class: Option<CommandClass>,
        delta: i64,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (track, unit, cnt) = match class {
            Some(CommandClass::Build) => {
                self.active_builds = (self.active_builds + delta).max(0);
                ("machine/active_builds", "builds", self.active_builds)
            }
            Some(CommandClass::Test) => {
                self.active_tests = (self.active_tests + delta).max(0);
                ("machine/active_tests", "tests", self.active_tests)
            }
            _ => return,
        };
        self.handle_counter_sample(track.to_string(), cnt, Some(unit.to_string()), ts, out);
    }

    /// Handles `Input::ToolBegin`.
    pub(crate) fn handle_tool_begin(
        &mut self,
        session: SessionKey,
        tool_use_id: String,
        tool_name: String,
        tool_in: ToolInput,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (agent_id, run_n) = self.resolve_session(&session, ts, out);
        let run_key = (agent_id.clone(), run_n);
        let slice_id = self.alloc_seq();
        let cwd = self.runs.get(&run_key).and_then(|r| r.cwd.clone());
        let details = self.resolve_tool_details(tool_in, &tool_use_id, &tool_name, cwd.as_deref());
        let assigned_track = self.runs.get_mut(&run_key).map_or_else(
            || format!("agent:{}/run:{}/turns", agent_id, run_n),
            |run| {
                run.tool_calls_count += 1;
                run.lane_allocator.allocate_begin(slice_id, details.kind)
            },
        );
        if details.kind == Kind::ToolCall && details.custom_name.as_deref() == Some("Subagent") {
            self.subagent_flows.insert(tool_use_id.clone(), slice_id);
        }
        self.emit(
            &assigned_track,
            details.kind,
            ts,
            Some(slice_id),
            None,
            Some(agent_id.clone()),
            details.custom_name.clone(),
            Some(details.body),
            out,
        );
        self.update_active_cmd_counter(details.command_class, 1, ts, out);
        if let Some(agent) = self.live.agents.get_mut(&agent_id) {
            agent.last_action = details.custom_name.clone().unwrap_or(tool_name.clone());
        }
        self.transition_state(
            &agent_id,
            "Tool",
            tacho::AgentStateType::Tool,
            "tool begin",
            ts,
            out,
        );
        if let Some((edit_k, added, removed)) = details.file_edit {
            if let Some(ref path) = details.file_path {
                self.record_file_edit(
                    path.clone(),
                    edit_k,
                    added,
                    removed,
                    &assigned_track,
                    slice_id,
                    &agent_id,
                    &run_key,
                    ts,
                    out,
                );
            }
        }
        if let Some(run) = self.runs.get_mut(&run_key) {
            run.open_tools.insert(
                tool_use_id,
                OpenToolSlice {
                    slice_id,
                    tool_use_id: String::new(),
                    tool_name: tool_name.clone(),
                    kind: details.kind,
                    assigned_track,
                    argv: details.argv,
                    command_class: details.command_class,
                    file_path: None,
                    file_edit: None,
                },
            );
        }
        let cfg = self.detector_cfg.clone();
        let firing = if let Some(det) = self.detectors.get_mut(&agent_id) {
            det.touch_event(ts);
            det.on_tool_begin(&details.digest, &cfg)
        } else {
            None
        };
        if let Some(f) = firing {
            if let Some(ev) = out.iter_mut().rfind(|e| e.seq == slice_id) {
                ev.flow_ids.push(slice_id);
            }
            self.handle_detector_firings(&agent_id, vec![f], Some(slice_id), ts, out);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record_file_edit(
        &mut self,
        path: String,
        edit_k: Option<FileEditKind>,
        added: Option<u32>,
        removed: Option<u32>,
        track: &str,
        parent_seq: u64,
        agent_id: &str,
        run_key: &(String, u32),
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let proto_k = edit_k.map(to_proto_edit_kind);
        let fe_body = tacho::event::Body::FileEdited(tacho::FileEdited {
            path: Some(truncate_utf8(&path, 512)),
            kind: proto_k.map(|k| k as i32),
            lines_added: added,
            lines_removed: removed,
            in_brief: Some(false),
        });
        self.emit(
            track,
            Kind::FileEdited,
            ts,
            None,
            Some(parent_seq),
            Some(agent_id.to_string()),
            None,
            Some(fe_body),
            out,
        );
        if let Some(run) = self.runs.get_mut(run_key) {
            run.files_edited_count += 1;
        }
        if let Some(agent) = self.live.agents.get_mut(agent_id) {
            agent.last_edit_ts_ns = Some(ts);
        }
        let cfg = self.detector_cfg.clone();
        let firing = if let Some(det) = self.detectors.get_mut(agent_id) {
            det.touch_event(ts);
            det.on_file_edited(&path, ts, &cfg)
        } else {
            None
        };
        if let Some(f) = firing {
            if let Some(ev) = out.iter_mut().rfind(|e| e.seq == parent_seq) {
                ev.flow_ids.push(parent_seq);
            }
            self.handle_detector_firings(agent_id, vec![f], Some(parent_seq), ts, out);
        }
    }

    fn record_command_completion(
        &mut self,
        argv: String,
        class: CommandClass,
        exit_code: Option<i32>,
        agent_id: &str,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        if let Some(agent) = self.live.agents.get_mut(agent_id) {
            agent.recent_commands.push(RecentCommand {
                argv: argv.clone(),
                class: command_class_name(class).to_string(),
                exit_code,
                ts_ns: ts,
            });
            if class == CommandClass::Test && exit_code == Some(0) {
                agent.last_passing_test_ts_ns = Some(ts);
            } else if class == CommandClass::Commit && exit_code == Some(0) {
                agent.commits_succeeded += 1;
            } else if class == CommandClass::Push && exit_code == Some(0) {
                agent.pushes_succeeded += 1;
            }
        }
        let cfg = self.detector_cfg.clone();
        let firing = if let Some(det) = self.detectors.get_mut(agent_id) {
            det.touch_event(ts);
            det.on_command_ended(&argv, class, exit_code, ts, &cfg)
        } else {
            None
        };
        if let Some(f) = firing {
            self.handle_detector_firings(agent_id, vec![f], None, ts, out);
        }
    }

    /// Handles `Input::ToolEnd`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_tool_end(
        &mut self,
        session: SessionKey,
        tool_use_id: String,
        error: bool,
        result_bytes: Option<u64>,
        exit_code: Option<i32>,
        ts: u64,
        out: &mut Vec<Event>,
    ) {
        let (agent_id, run_n) = self.resolve_session(&session, ts, out);
        let run_key = (agent_id.clone(), run_n);
        let open_tool = self
            .runs
            .get_mut(&run_key)
            .and_then(|r| r.open_tools.remove(&tool_use_id));

        if let Some(tool) = open_tool {
            self.update_active_cmd_counter(tool.command_class, -1, ts, out);
            let end_kind = tool.kind.pairing().unwrap_or(tool.kind);
            let track_res = if let Some(run) = self.runs.get_mut(&run_key) {
                run.lane_allocator.allocate_end(tool.slice_id, tool.kind)
            } else {
                Ok(tool.assigned_track.clone())
            };
            let track = match track_res {
                Ok(t) => t,
                Err(LaneError::UnmatchedEnd { .. }) => {
                    let d_track = "daemon".to_string();
                    let body = tacho::event::Body::TraceError(tacho::TraceError {
                        message: Some(format!("unmatched end for tool {}", tool_use_id)),
                        track_key: Some(tool.assigned_track.clone()),
                    });
                    self.emit(
                        &d_track,
                        Kind::TraceError,
                        ts,
                        None,
                        None,
                        Some(agent_id.clone()),
                        None,
                        Some(body),
                        out,
                    );
                    tool.assigned_track
                }
            };
            let end_body = make_tool_end_body(end_kind, exit_code, result_bytes, error);
            let _seq = self.emit(
                &track,
                end_kind,
                ts,
                None,
                Some(tool.slice_id),
                Some(agent_id.clone()),
                None,
                Some(end_body),
                out,
            );
            if let (Some(argv), Some(class)) = (tool.argv, tool.command_class) {
                self.record_command_completion(argv, class, exit_code, &agent_id, ts, out);
            }
            if !error && exit_code.unwrap_or(0) == 0 {
                if let Some(det) = self.detectors.get_mut(&agent_id) {
                    if det.silent_fired {
                        det.silent_fired = false;
                        det.is_stalled = false;
                    }
                }
            }
            let is_turn_open = self
                .runs
                .get(&run_key)
                .map(|r| r.turn_open)
                .unwrap_or(false);
            let is_stalled = self
                .detectors
                .get(&agent_id)
                .map(|d| d.is_stalled)
                .unwrap_or(false);
            let (next_st, next_enum) = if is_stalled {
                ("Stalled", tacho::AgentStateType::Stalled)
            } else if is_turn_open {
                ("Thinking", tacho::AgentStateType::Thinking)
            } else {
                ("Idle", tacho::AgentStateType::Idle)
            };
            self.transition_state(&agent_id, next_st, next_enum, "tool ended", ts, out);
        } else {
            let track = format!("agent:{}/run:{}/turns", agent_id, run_n);
            let err_body = tacho::event::Body::TraceError(tacho::TraceError {
                message: Some(format!("unmatched end for tool_use_id {}", tool_use_id)),
                track_key: Some(track),
            });
            self.emit(
                "daemon",
                Kind::TraceError,
                ts,
                None,
                None,
                Some(agent_id),
                None,
                Some(err_body),
                out,
            );
        }
    }
}
