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
        path: None,
    })
}

/// Splits a shell command string into words, handling single quotes, double quotes,
/// and backslash escapes according to standard POSIX shell semantics.
fn split_shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut in_word = false;

    for c in s.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            in_word = true;
        } else if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                current.push(c);
            }
        } else if in_double {
            if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_double = false;
            } else {
                current.push(c);
            }
        } else {
            match c {
                '\\' => {
                    escaped = true;
                    in_word = true;
                }
                '\'' => {
                    in_single = true;
                    in_word = true;
                }
                '"' => {
                    in_double = true;
                    in_word = true;
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                _ => {
                    current.push(c);
                    in_word = true;
                }
            }
        }
    }
    if escaped {
        current.push('\\');
    }
    if in_word {
        words.push(current);
    }
    words
}

/// Formats a shell command argv for display by stripping redundant cwd references:
/// - drops a `-C <tok>` pair only when it directly follows `git` and `<tok>` equals `cwd`
/// - strips a token prefix equal to `cwd + "/"`
/// - rejoins tokens with single spaces, quoting tokens that contain spaces.
///
/// Returns the original `s_argv` untouched if `s_cwd` is missing, empty, ".", or "/".
fn format_display_argv(s_argv: &str, s_cwd: Option<&str>) -> String {
    let Some(s_c) = s_cwd else {
        return s_argv.to_string();
    };
    if s_c.is_empty() || s_c == "." || s_c == "/" {
        return s_argv.to_string();
    }
    let s_c_trimmed = s_c.trim_end_matches('/');
    if s_c_trimmed.is_empty() {
        return s_argv.to_string();
    }
    let prefix = format!("{s_c_trimmed}/");
    let tokens = split_shell_words(s_argv);
    let mut new_tokens = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let tok = &tokens[i];
        if tok == "-C"
            && i > 0
            && (new_tokens
                .last()
                .map(|s: &String| s.as_str() == "git" || s.ends_with("/git"))
                .unwrap_or(false))
            && i + 1 < tokens.len()
            && (tokens[i + 1] == s_c || tokens[i + 1].trim_end_matches('/') == s_c_trimmed)
        {
            i += 2;
            continue;
        }
        if let Some(stripped) = tok.strip_prefix(&prefix) {
            new_tokens.push(stripped.to_string());
        } else {
            new_tokens.push(tok.clone());
        }
        i += 1;
    }
    new_tokens
        .into_iter()
        .map(|t| {
            if t.contains(' ') {
                format!("\"{}\"", t.replace('"', "\\\""))
            } else {
                t
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
                let s_c = eff_cwd.and_then(|c| {
                    if c.is_empty() || c == "." || c == "/" {
                        None
                    } else {
                        Some(self.scrubber.scrub(c))
                    }
                });
                let display_argv = format_display_argv(&s_argv, s_c.as_deref());
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
                let rel = relativize_path(&path, cwd.unwrap_or("."));
                let s_path = truncate_utf8(&self.scrubber.scrub(&rel), 512);
                let slice_label = if s_path.is_empty() {
                    "Read".to_string()
                } else {
                    format!("Read: {}", truncate_utf8(&s_path, 48))
                };
                ToolDetails {
                    kind: Kind::FileAccess,
                    custom_name: Some(slice_label),
                    body: tacho::event::Body::FileAccess(tacho::FileAccess {
                        path: Some(s_path),
                        kind: Some(tacho::FileAccessKind::Read as i32),
                        query: None,
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                    }),
                    digest: blake3_digest_8(&path),
                    file_path: Some(rel),
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
                        path: s_path,
                        kind: Some(tacho::FileAccessKind::Search as i32),
                        query: Some(s_query),
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                    }),
                    digest: blake3_digest_8(&format!("{}:{:?}", pattern, path)),
                    file_path: rel,
                    ..Default::default()
                }
            }
            ToolInput::FileEdit {
                path,
                kind,
                lines_added,
                lines_removed,
            } => {
                let rel = relativize_path(&path, cwd.unwrap_or("."));
                let s_rel = truncate_utf8(&self.scrubber.scrub(&rel), 512);
                let digest = blake3_digest_8(&path);
                let slice_label = match (lines_added, lines_removed) {
                    (Some(a), Some(r)) => {
                        format!("Edit: {} (+{}/-{})", truncate_utf8(&s_rel, 48), a, r)
                    }
                    _ if !s_rel.is_empty() => format!("Edit: {}", truncate_utf8(&s_rel, 48)),
                    _ => "Edit".to_string(),
                };
                ToolDetails {
                    custom_name: Some(slice_label),
                    body: tacho::event::Body::ToolCall(tacho::ToolCall {
                        name: Some("Edit".to_string()),
                        input_digest: Some(digest.clone()),
                        input_bytes: None,
                        tool_use_id: Some(truncate_utf8(tool_use_id, 128)),
                        path: Some(s_rel),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_shell_words_basic() {
        assert_eq!(split_shell_words(""), Vec::<String>::new());
        assert_eq!(split_shell_words("   "), Vec::<String>::new());
        assert_eq!(split_shell_words("git status"), vec!["git", "status"]);
        assert_eq!(split_shell_words("git   status"), vec!["git", "status"]);
    }

    #[test]
    fn test_split_shell_words_quotes_and_escapes() {
        assert_eq!(
            split_shell_words("echo 'hello world'"),
            vec!["echo", "hello world"]
        );
        assert_eq!(
            split_shell_words("echo \"hello world\""),
            vec!["echo", "hello world"]
        );
        assert_eq!(
            split_shell_words("echo hello\\ world"),
            vec!["echo", "hello world"]
        );
        assert_eq!(
            split_shell_words("echo \"foo\\\"bar\""),
            vec!["echo", "foo\"bar"]
        );
        assert_eq!(
            split_shell_words("git commit -m ''"),
            vec!["git", "commit", "-m", ""]
        );
        assert_eq!(
            split_shell_words("git commit -m \"\""),
            vec!["git", "commit", "-m", ""]
        );
        assert_eq!(split_shell_words("echo 'a\\b'"), vec!["echo", "a\\b"]);
    }

    #[test]
    fn test_format_display_argv_git_c_dropping() {
        // -C <cwd> directly following git is dropped
        assert_eq!(
            format_display_argv("git -C /repo status", Some("/repo")),
            "git status"
        );
        assert_eq!(
            format_display_argv("git -C /repo/ status", Some("/repo")),
            "git status"
        );
        assert_eq!(
            format_display_argv("/usr/bin/git -C /repo status", Some("/repo")),
            "/usr/bin/git status"
        );

        // -C to other directory is kept
        assert_eq!(
            format_display_argv("git -C /other status", Some("/repo")),
            "git -C /other status"
        );

        // -C not following git is kept
        assert_eq!(
            format_display_argv("other -C /repo status", Some("/repo")),
            "other -C /repo status"
        );
    }

    #[test]
    fn test_format_display_argv_prefix_stripping_and_corruption() {
        // Prefix stripping of cwd + "/"
        assert_eq!(
            format_display_argv("cat /repo/src/main.rs", Some("/repo")),
            "cat src/main.rs"
        );

        // Crucial corruption case: /backup/repo/b must NOT be stripped or corrupted
        assert_eq!(
            format_display_argv("git /backup/repo/b", Some("/repo")),
            "git /backup/repo/b"
        );
        assert_eq!(
            format_display_argv("cat /backup/repo/b/file.txt", Some("/repo")),
            "cat /backup/repo/b/file.txt"
        );

        // cwd alone without slash is not stripped
        assert_eq!(
            format_display_argv("echo /repo", Some("/repo")),
            "echo /repo"
        );
    }

    #[test]
    fn test_format_display_argv_no_stripping_cases() {
        // None, empty, "/", "." -> no stripping
        assert_eq!(
            format_display_argv("git -C /repo /repo/b", None),
            "git -C /repo /repo/b"
        );
        assert_eq!(format_display_argv("git -C / /b", Some("/")), "git -C / /b");
        assert_eq!(
            format_display_argv("git -C . ./b", Some(".")),
            "git -C . ./b"
        );
        assert_eq!(
            format_display_argv("git -C /repo /repo/b", Some("")),
            "git -C /repo /repo/b"
        );
    }

    #[test]
    fn test_format_display_argv_quoting_spaces() {
        assert_eq!(
            format_display_argv("git commit -m \"my commit msg\"", Some("/repo")),
            "git commit -m \"my commit msg\""
        );
    }

    #[test]
    fn test_resolve_tool_details_unscrubbed_file_paths() {
        let rec = Recorder::new();
        // FileRead
        let read_details = rec.resolve_tool_details(
            ToolInput::FileRead {
                path: "/workspace/secret_token_123/src/lib.rs".to_string(),
            },
            "call_1",
            "read",
            Some("/workspace"),
        );
        // file_path must be raw relativized (unscrubbed)
        assert_eq!(
            read_details.file_path.as_deref(),
            Some("secret_token_123/src/lib.rs")
        );

        // FileSearch
        let search_details = rec.resolve_tool_details(
            ToolInput::FileSearch {
                pattern: "fn test".to_string(),
                path: Some("/workspace/secret_token_123/src".to_string()),
            },
            "call_2",
            "search",
            Some("/workspace"),
        );
        assert_eq!(
            search_details.file_path.as_deref(),
            Some("secret_token_123/src")
        );

        // FileEdit
        let edit_details = rec.resolve_tool_details(
            ToolInput::FileEdit {
                path: "/workspace/secret_token_123/src/lib.rs".to_string(),
                kind: None,
                lines_added: Some(5),
                lines_removed: Some(2),
            },
            "call_3",
            "edit",
            Some("/workspace"),
        );
        assert_eq!(
            edit_details.file_path.as_deref(),
            Some("secret_token_123/src/lib.rs")
        );
    }

    #[test]
    fn test_touch_cpu_activity_clears_silent_emits_idle_transition() {
        use crate::input::Input;

        let mut rec = Recorder::new();
        let mut out = Vec::new();
        let mut ts = 1_000_000_000u64;

        rec.apply(
            Input::RunStart {
                provider: "test".into(),
                session_id: "s1".into(),
                agent: Some("a1".into()),
                task: None,
                pid: None,
                cwd: Some("/repo".into()),
                model: None,
                parent_session: None,
                git_tree: None,
                source_ts_ns: Some(ts),
                role: None,
            },
            ts,
            &mut out,
        );

        // Advance 305s to trigger SILENT stall
        ts += 305 * 1_000_000_000;
        rec.tick(ts, &mut out);

        assert_eq!(rec.live_state().agents["a1"].state.as_str(), "Stalled");
        assert!(rec.detectors["a1"].is_stalled);

        // 1st CPU sample: sets baseline, must NOT clear stall
        ts += 1_000_000_000;
        rec.apply(
            Input::CounterSample {
                track_key: "agent:a1/run:1/cpu".into(),
                value: 10_000,
                unit: Some("ms".into()),
                source_ts_ns: Some(ts),
            },
            ts,
            &mut out,
        );
        assert_eq!(rec.live_state().agents["a1"].state.as_str(), "Stalled");

        // 2nd CPU sample with active rate (>= 50ms/s): clears stall and transitions to Idle
        ts += 1_000_000_000; // +1s
        rec.apply(
            Input::CounterSample {
                track_key: "agent:a1/run:1/cpu".into(),
                value: 10_200, // +200ms in 1s = 200ms/s
                unit: Some("ms".into()),
                source_ts_ns: Some(ts),
            },
            ts,
            &mut out,
        );
        assert_eq!(rec.live_state().agents["a1"].state.as_str(), "Idle");
        assert!(!rec.detectors["a1"].is_stalled);

        // Verify the AgentState event transitioning to Idle with reason "cpu activity" was emitted
        let idle_event = out.iter().rev().find(|e| {
            if e.kind == Kind::AgentState {
                if let Some(tacho::event::Body::AgentState(ref st)) = e.body.body {
                    return st.state == Some(tacho::AgentStateType::Idle as i32)
                        && st.because.as_deref() == Some("cpu activity");
                }
            }
            false
        });
        assert!(
            idle_event.is_some(),
            "Expected AgentState::Idle transition event with reason 'cpu activity'"
        );
    }
}
