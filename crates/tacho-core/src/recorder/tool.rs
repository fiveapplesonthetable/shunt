//! Tool execution details and end-slice body builders.

use crate::classify::{classify, CommandClass};
use crate::detect::blake3_digest_8;
use crate::input::{FileEditKind, ToolInput};
use crate::kind::Kind;
use crate::scrub::{relativize_cwd, relativize_path, truncate_utf8};
use tacho_proto::tacho;

use super::tracks::{command_class_name, to_proto_cmd_class};
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
    pub(crate) triggered_stall: bool,
}

pub(crate) struct ToolDetails {
    pub(crate) kind: Kind,
    pub(crate) custom_name: Option<String>,
    pub(crate) body: tacho::event::Body,
    pub(crate) argv: Option<String>,
    pub(crate) command_class: Option<CommandClass>,
    pub(crate) file_path: Option<String>,
    pub(crate) file_edit: Option<(Option<FileEditKind>, Option<u32>, Option<u32>)>,
    pub(crate) digest: String,
}

impl Default for ToolDetails {
    fn default() -> Self {
        Self {
            kind: Kind::ToolCall,
            custom_name: None,
            body: tacho::event::Body::ToolCall(tacho::ToolCall::default()),
            argv: None,
            command_class: None,
            file_path: None,
            file_edit: None,
            digest: String::new(),
        }
    }
}

pub(crate) fn make_tool_end_body_full(
    end_kind: Kind,
    exit: Option<i32>,
    bytes: Option<u64>,
    error: bool,
    abandoned: bool,
) -> tacho::event::Body {
    match end_kind {
        Kind::CommandEnded => tacho::event::Body::CommandEnded(tacho::CommandEnded {
            exit,
            timed_out: Some(false),
            stdout_bytes: bytes,
            stderr_bytes: None,
            tests_run: None,
            tests_failed: None,
            abandoned: Some(abandoned),
        }),
        Kind::FileAccessEnded => tacho::event::Body::FileAccessEnded(tacho::FileAccessEnded {
            bytes_read: bytes,
            items_found: None,
            error: Some(error),
            abandoned: Some(abandoned),
        }),
        Kind::TurnEnded => tacho::event::Body::TurnEnded(tacho::TurnEnded::default()),
        Kind::WaitEnded => tacho::event::Body::WaitEnded(tacho::WaitEnded {
            timed_out: Some(false),
            abandoned: Some(abandoned),
        }),
        Kind::ReviewEnded => tacho::event::Body::ReviewEnded(tacho::ReviewEnded {
            findings_count: None,
            approved: None,
            abandoned: Some(abandoned),
        }),
        Kind::CustomEnded => tacho::event::Body::CustomEnded(tacho::CustomEnded {
            id: None,
            args: Vec::new(),
            abandoned: Some(abandoned),
        }),
        _ => tacho::event::Body::ToolCallEnded(tacho::ToolCallEnded {
            error: Some(error),
            result_bytes: bytes,
            abandoned: Some(abandoned),
        }),
    }
}

pub(crate) fn make_tool_end_body(
    end_kind: Kind,
    exit: Option<i32>,
    bytes: Option<u64>,
    error: bool,
) -> tacho::event::Body {
    make_tool_end_body_full(end_kind, exit, bytes, error, false)
}

pub(crate) fn make_abandoned_body(end_kind: Kind) -> tacho::event::Body {
    make_tool_end_body_full(end_kind, None, None, true, true)
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
    pub(crate) fn resolve_tool_details(
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
                git_tree,
            } => {
                let s_argv = truncate_utf8(&self.scrubber.scrub(&argv), 512);
                let eff_cwd = tool_cwd.as_deref().or(cwd);
                let s_cwd = eff_cwd
                    .map(|c| truncate_utf8(&self.scrubber.scrub(&relativize_cwd(c, None)), 512));
                let words: Vec<&str> = argv.split_whitespace().collect();
                let class = classify(&words);
                let proto_class = to_proto_cmd_class(class);
                let class_str = command_class_name(class);
                let display_argv = match eff_cwd {
                    Some(c) if !c.is_empty() && c != "." => {
                        let s_c = self.scrubber.scrub(c);
                        s_argv
                            .replace(&format!(" -C {s_c} "), " ")
                            .replace(&format!("{s_c}/"), "")
                    }
                    _ => s_argv.clone(),
                };
                let short_argv = truncate_utf8(&display_argv, 64);
                let slice_label = if short_argv.is_empty() {
                    class_str.to_string()
                } else {
                    format!("{}: {}", class_str, short_argv)
                };
                ToolDetails {
                    kind: Kind::Command,
                    custom_name: Some(slice_label),
                    body: tacho::event::Body::Command(tacho::Command {
                        argv: Some(s_argv.clone()),
                        cwd: s_cwd,
                        class: Some(proto_class as i32),
                        check_name: None,
                        via_wrap: Some(false),
                        program: words.first().map(|w| truncate_utf8(w, 64)),
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                        git_tree: git_tree.as_deref().map(|g| truncate_utf8(g, 64)),
                    }),
                    argv: Some(s_argv),
                    command_class: Some(class),
                    digest: blake3_digest_8(&argv),
                    ..Default::default()
                }
            }
            ToolInput::FileRead { path } => {
                let s_path = truncate_utf8(
                    &self
                        .scrubber
                        .scrub(&relativize_path(&path, cwd.unwrap_or("."))),
                    512,
                );
                let slice_label = if s_path.is_empty() {
                    "Read".to_string()
                } else {
                    format!("Read: {}", truncate_utf8(&s_path, 48))
                };
                ToolDetails {
                    kind: Kind::FileAccess,
                    custom_name: Some(slice_label),
                    body: tacho::event::Body::FileAccess(tacho::FileAccess {
                        path: Some(s_path.clone()),
                        kind: Some(tacho::FileAccessKind::Read as i32),
                        query: None,
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                    }),
                    digest: blake3_digest_8(&path),
                    file_path: Some(s_path),
                    ..Default::default()
                }
            }
            ToolInput::FileSearch { pattern, path } => {
                let rel = path
                    .as_deref()
                    .map(|p| relativize_path(p, cwd.unwrap_or(".")));
                let s_path = rel
                    .as_deref()
                    .map(|p| truncate_utf8(&self.scrubber.scrub(p), 512));
                let s_query = truncate_utf8(&self.scrubber.scrub(&pattern), 256);
                let slice_label = if s_query.is_empty() {
                    "Search".to_string()
                } else {
                    format!("Search: {}", truncate_utf8(&s_query, 48))
                };
                ToolDetails {
                    kind: Kind::FileAccess,
                    custom_name: Some(slice_label),
                    body: tacho::event::Body::FileAccess(tacho::FileAccess {
                        path: s_path.clone(),
                        kind: Some(tacho::FileAccessKind::Search as i32),
                        query: Some(s_query),
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                    }),
                    digest: blake3_digest_8(&format!("{}:{:?}", pattern, path)),
                    file_path: s_path,
                    ..Default::default()
                }
            }
            ToolInput::FileEdit {
                path,
                kind,
                lines_added,
                lines_removed,
            } => {
                let rel = truncate_utf8(
                    &self
                        .scrubber
                        .scrub(&relativize_path(&path, cwd.unwrap_or("."))),
                    512,
                );
                let digest = blake3_digest_8(&path);
                let slice_label = match (lines_added, lines_removed) {
                    (Some(a), Some(r)) => {
                        format!("Edit: {} (+{}/-{})", truncate_utf8(&rel, 48), a, r)
                    }
                    _ if !rel.is_empty() => format!("Edit: {}", truncate_utf8(&rel, 48)),
                    _ => "Edit".to_string(),
                };
                ToolDetails {
                    custom_name: Some(slice_label),
                    body: tacho::event::Body::ToolCall(tacho::ToolCall {
                        name: Some("Edit".to_string()),
                        input_digest: Some(digest.clone()),
                        input_bytes: None,
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                    }),
                    file_path: Some(rel),
                    file_edit: Some((kind, lines_added, lines_removed)),
                    digest,
                    ..Default::default()
                }
            }
            ToolInput::Subagent { description_len } => ToolDetails {
                custom_name: Some("Subagent".to_string()),
                body: make_simple_tool_call(
                    "Subagent",
                    &format!("subagent:{}", tool_use_id),
                    description_len,
                    tool_use_id,
                ),
                digest: blake3_digest_8(&format!("subagent:{}", tool_use_id)),
                ..Default::default()
            },
            ToolInput::Other {
                input_digest,
                input_bytes,
            } => {
                let digest = input_digest.unwrap_or_else(|| blake3_digest_8(tool_name));
                ToolDetails {
                    custom_name: Some(tool_name.to_string()),
                    body: make_simple_tool_call(tool_name, &digest, input_bytes, tool_use_id),
                    digest,
                    ..Default::default()
                }
            }
        }
    }
}
