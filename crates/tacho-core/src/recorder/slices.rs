//! Slice lifecycle (Turn, Tool, Wait, Prompt) and lane allocation.

use crate::classify::{classify, CommandClass};
use crate::detect::blake3_digest_8;
use crate::event::Event;
use crate::input::{FileEditKind, SessionKey, ToolInput};
use crate::kind::Kind;
use crate::lane::LaneError;
use crate::live::RecentCommand;
use crate::scrub::{relativize_cwd, relativize_path, truncate_utf8};
use tacho_proto::tacho;

use super::tracks::{command_class_name, to_proto_cmd_class, to_proto_edit_kind};
use super::Recorder;

/// Metadata for an active tool execution slice.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct OpenToolSlice {
    pub(crate) slice_id: u64,
    pub(crate) tool_use_id: String,
    pub(crate) tool_name: String,
    pub(crate) kind: Kind,
    pub(crate) assigned_track: String,
    pub(crate) argv: Option<String>,
    pub(crate) command_class: Option<CommandClass>,
    pub(crate) file_path: Option<String>,
    pub(crate) file_edit: Option<(Option<FileEditKind>, Option<u32>, Option<u32>)>,
}

struct ToolDetails {
    kind: Kind,
    custom_name: Option<String>,
    body: tacho::event::Body,
    argv: Option<String>,
    command_class: Option<CommandClass>,
    file_path: Option<String>,
    file_edit: Option<(Option<FileEditKind>, Option<u32>, Option<u32>)>,
    digest: String,
}

fn make_tool_end_body(
    end_kind: Kind,
    exit: Option<i32>,
    bytes: Option<u64>,
    error: bool,
) -> tacho::event::Body {
    match end_kind {
        Kind::CommandEnded => tacho::event::Body::CommandEnded(tacho::CommandEnded {
            exit,
            timed_out: Some(false),
            stdout_bytes: bytes,
            stderr_bytes: None,
            tests_run: None,
            tests_failed: None,
            abandoned: Some(false),
        }),
        Kind::FileAccessEnded => tacho::event::Body::FileAccessEnded(tacho::FileAccessEnded {
            bytes_read: bytes,
            items_found: None,
            error: Some(error),
            abandoned: Some(false),
        }),
        _ => tacho::event::Body::ToolCallEnded(tacho::ToolCallEnded {
            error: Some(error),
            result_bytes: bytes,
            abandoned: Some(false),
        }),
    }
}

fn make_simple_tool_call(
    name: &str,
    digest: &str,
    bytes: Option<u64>,
    id: &str,
) -> tacho::event::Body {
    tacho::event::Body::ToolCall(tacho::ToolCall {
        name: Some(truncate_utf8(name, 64)),
        input_digest: Some(digest.to_string()),
        input_bytes: bytes,
        tool_use_id: Some(truncate_utf8(id, 128)),
    })
}

impl Recorder {
    fn resolve_tool_details(
        &self,
        tool_in: ToolInput,
        tool_use_id: &str,
        tool_name: &str,
        cwd: Option<&str>,
    ) -> ToolDetails {
        match tool_in {
            ToolInput::Shell {
                argv,
                cwd: tool_cwd,
            } => {
                let s_argv = truncate_utf8(&self.scrubber.scrub(&argv), 512);
                let eff_cwd = tool_cwd.as_deref().or(cwd);
                let s_cwd = eff_cwd.map(|c| truncate_utf8(&relativize_cwd(c, None), 512));
                let words: Vec<&str> = argv.split_whitespace().collect();
                let class = classify(&words);
                let proto_class = to_proto_cmd_class(class);
                ToolDetails {
                    kind: Kind::Command,
                    custom_name: Some(command_class_name(class).to_string()),
                    body: tacho::event::Body::Command(tacho::Command {
                        argv: Some(s_argv),
                        cwd: s_cwd,
                        class: Some(proto_class as i32),
                        check_name: None,
                        via_wrap: Some(false),
                        program: None,
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                    }),
                    argv: Some(argv.clone()),
                    command_class: Some(class),
                    file_path: None,
                    file_edit: None,
                    digest: blake3_digest_8(&argv),
                }
            }
            ToolInput::FileRead { path } => {
                let rel = relativize_path(&path, cwd.unwrap_or("."));
                let digest = blake3_digest_8(&path);
                ToolDetails {
                    kind: Kind::FileAccess,
                    custom_name: Some("Read".to_string()),
                    body: tacho::event::Body::FileAccess(tacho::FileAccess {
                        path: Some(truncate_utf8(&rel, 512)),
                        kind: Some(tacho::FileAccessKind::Read as i32),
                        query: None,
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                    }),
                    argv: None,
                    command_class: None,
                    file_path: Some(path),
                    file_edit: None,
                    digest,
                }
            }
            ToolInput::FileSearch { pattern, path } => {
                let rel = path
                    .as_deref()
                    .map(|p| relativize_path(p, cwd.unwrap_or(".")));
                let s_path = rel.as_deref().map(|p| truncate_utf8(p, 512));
                let digest = blake3_digest_8(&format!("{}:{:?}", pattern, path));
                ToolDetails {
                    kind: Kind::FileAccess,
                    custom_name: Some("Search".to_string()),
                    body: tacho::event::Body::FileAccess(tacho::FileAccess {
                        path: s_path,
                        kind: Some(tacho::FileAccessKind::Search as i32),
                        query: Some(truncate_utf8(&pattern, 256)),
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                    }),
                    argv: None,
                    command_class: None,
                    file_path: path,
                    file_edit: None,
                    digest,
                }
            }
            ToolInput::FileEdit {
                path,
                kind,
                lines_added,
                lines_removed,
            } => {
                let digest = blake3_digest_8(&path);
                ToolDetails {
                    kind: Kind::ToolCall,
                    custom_name: Some("Edit".to_string()),
                    body: tacho::event::Body::ToolCall(tacho::ToolCall {
                        name: Some("Edit".to_string()),
                        input_digest: Some(digest.clone()),
                        input_bytes: None,
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                    }),
                    argv: None,
                    command_class: None,
                    file_path: Some(path),
                    file_edit: Some((kind, lines_added, lines_removed)),
                    digest,
                }
            }
            ToolInput::Subagent { description_len } => {
                let digest = blake3_digest_8(&format!("subagent:{}", tool_use_id));
                ToolDetails {
                    kind: Kind::ToolCall,
                    custom_name: Some("Subagent".to_string()),
                    body: make_simple_tool_call("Subagent", &digest, description_len, tool_use_id),
                    argv: None,
                    command_class: None,
                    file_path: None,
                    file_edit: None,
                    digest,
                }
            }
            ToolInput::Other {
                input_digest,
                input_bytes,
            } => {
                let digest = input_digest.unwrap_or_else(|| blake3_digest_8(tool_name));
                ToolDetails {
                    kind: Kind::ToolCall,
                    custom_name: Some(tool_name.to_string()),
                    body: make_simple_tool_call(tool_name, &digest, input_bytes, tool_use_id),
                    argv: None,
                    command_class: None,
                    file_path: None,
                    file_edit: None,
                    digest,
                }
            }
        }
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
        let assigned_track = if let Some(run) = self.runs.get_mut(&run_key) {
            run.tool_calls_count += 1;
            run.lane_allocator.allocate_begin(slice_id, details.kind)
        } else {
            format!("agent:{}/run:{}/turns", agent_id, run_n)
        };
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
        if let Some(agent) = self.live.agents.get_mut(&agent_id) {
            agent.last_action = details.custom_name.unwrap_or(tool_name);
        }
        self.transition_state(
            &agent_id,
            "Tool",
            tacho::AgentStateType::Tool,
            "tool begin",
            ts,
            out,
        );
        let cfg = self.detector_cfg.clone();
        let firing = if let Some(det) = self.detectors.get_mut(&agent_id) {
            det.touch_event(ts);
            det.on_tool_begin(&details.digest, &cfg)
        } else {
            None
        };
        if let Some(f) = firing {
            self.handle_detector_firings(&agent_id, vec![f], ts, out);
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
        let cfg = self.detector_cfg.clone();
        let firing = if let Some(det) = self.detectors.get_mut(agent_id) {
            det.touch_event(ts);
            det.on_file_edited(&path, &cfg)
        } else {
            None
        };
        if let Some(f) = firing {
            self.handle_detector_firings(agent_id, vec![f], ts, out);
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
        }
        let cfg = self.detector_cfg.clone();
        let firing = if let Some(det) = self.detectors.get_mut(agent_id) {
            det.touch_event(ts);
            det.on_command_ended(&argv, class, exit_code, &cfg)
        } else {
            None
        };
        if let Some(f) = firing {
            self.handle_detector_firings(agent_id, vec![f], ts, out);
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
            let is_turn_open = self
                .runs
                .get(&run_key)
                .map(|r| r.turn_open)
                .unwrap_or(false);
            let (next_st, next_enum) = if is_turn_open {
                ("Thinking", tacho::AgentStateType::Thinking)
            } else {
                ("Idle", tacho::AgentStateType::Idle)
            };
            self.transition_state(&agent_id, next_st, next_enum, "tool ended", ts, out);
        } else {
            let track = format!("agent:{}/run:{}/turns", agent_id, run_n);
            let body = tacho::event::Body::ToolCallEnded(tacho::ToolCallEnded {
                error: Some(error),
                result_bytes,
                abandoned: Some(false),
            });
            self.emit(
                &track,
                Kind::ToolCallEnded,
                ts,
                None,
                None,
                Some(agent_id.clone()),
                Some("UnmatchedToolCallEnded".to_string()),
                Some(body),
                out,
            );
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
