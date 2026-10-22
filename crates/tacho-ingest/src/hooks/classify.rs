use tacho_core::{FileEditKind, ToolInput};

/// Counts the number of lines in a string.
pub fn count_lines(s: &str) -> u32 {
    if s.is_empty() {
        0
    } else {
        s.lines().count() as u32
    }
}

/// Produces deterministic, canonical JSON bytes for a serde_json Value.
/// Keys in objects are sorted alphabetically, with no whitespace between tokens.
pub fn canonical_json(value: &serde_json::Value) -> Vec<u8> {
    match value {
        serde_json::Value::Null => b"null".to_vec(),
        serde_json::Value::Bool(b) => {
            if *b {
                b"true".to_vec()
            } else {
                b"false".to_vec()
            }
        }
        serde_json::Value::Number(n) => n.to_string().into_bytes(),
        serde_json::Value::String(s) => serde_json::to_vec(s).unwrap_or_default(),
        serde_json::Value::Array(arr) => {
            let mut out = Vec::new();
            out.push(b'[');
            for (i, elem) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend(canonical_json(elem));
            }
            out.push(b']');
            out
        }
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Vec::new();
            out.push(b'{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(k).unwrap_or_default());
                out.push(b':');
                out.extend(canonical_json(v));
            }
            out.push(b'}');
            out
        }
    }
}

/// Computes an 8-byte (16 hex chars) BLAKE3 digest of the given bytes.
pub fn blake3_digest_8_bytes(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    let b = hash.as_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
    )
}

/// Computes an 8-byte (16 hex chars) BLAKE3 digest of a string.
pub fn blake3_digest_8(text: &str) -> String {
    blake3_digest_8_bytes(text.as_bytes())
}

/// Classifies a tool invocation given its name and JSON input payload into a `ToolInput`.
pub fn parse_exit_status(s: &str) -> Option<i32> {
    let lower = s.to_lowercase();
    for prefix in &[
        "exit status ",
        "exit code ",
        "exit status: ",
        "exit code: ",
        "status: ",
    ] {
        if let Some(idx) = lower.find(prefix) {
            let after = &lower[idx + prefix.len()..];
            let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(code) = num_str.parse::<i32>() {
                return Some(code);
            }
        }
    }
    None
}

/// Classifies a tool invocation given its name and JSON input payload into a `ToolInput`.
pub fn classify_tool(tool_name: &str, tool_input: &serde_json::Value) -> ToolInput {
    let lower_name = tool_name.to_lowercase();

    // 1. Shell / Command execution
    if matches!(
        lower_name.as_str(),
        "bash"
            | "run_command"
            | "shell"
            | "exec_command"
            | "execute_command"
            | "terminal"
            | "sh"
            | "run_shell_command"
            | "command_execution"
            | "exec"
    ) {
        let argv = tool_input
            .get("command")
            .or_else(|| tool_input.get("CommandLine"))
            .or_else(|| tool_input.get("cmd"))
            .or_else(|| tool_input.get("script"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                tool_input.get("argv").and_then(|v| {
                    if let Some(s) = v.as_str() {
                        Some(s.to_string())
                    } else if let Some(arr) = v.as_array() {
                        let parts: Vec<&str> = arr.iter().filter_map(|x| x.as_str()).collect();
                        Some(parts.join(" "))
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_default();

        let cwd = tool_input
            .get("cwd")
            .or_else(|| tool_input.get("Cwd"))
            .or_else(|| tool_input.get("working_dir"))
            .or_else(|| tool_input.get("directory"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let git_tree = tool_input
            .get("git_tree")
            .or_else(|| tool_input.get("gitTree"))
            .or_else(|| tool_input.get("tree"))
            .and_then(|v| v.as_str())
            .map(String::from);

        return ToolInput::Shell {
            argv,
            cwd,
            git_tree,
        };
    }

    // 2. FileRead
    if matches!(
        lower_name.as_str(),
        "read" | "view_file" | "read_file" | "view" | "readfile" | "cat" | "fetch_file"
    ) {
        let path = tool_input
            .get("file_path")
            .or_else(|| tool_input.get("path"))
            .or_else(|| tool_input.get("Path"))
            .or_else(|| tool_input.get("file"))
            .or_else(|| tool_input.get("filepath"))
            .or_else(|| tool_input.get("target_file"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        return ToolInput::FileRead { path };
    }

    // 3. FileSearch
    if matches!(
        lower_name.as_str(),
        "grep"
            | "glob"
            | "search"
            | "code_search"
            | "file_search"
            | "grep_search"
            | "moma_search"
            | "find"
            | "find_files"
            | "ripgrep"
            | "search_files"
            | "ls"
            | "list_dir"
            | "dir"
    ) {
        let pattern = tool_input
            .get("pattern")
            .or_else(|| tool_input.get("query"))
            .or_else(|| tool_input.get("Query"))
            .or_else(|| tool_input.get("search_term"))
            .or_else(|| tool_input.get("regex"))
            .or_else(|| tool_input.get("term"))
            .or_else(|| tool_input.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let path = tool_input
            .get("path")
            .or_else(|| tool_input.get("Path"))
            .or_else(|| tool_input.get("dir"))
            .or_else(|| tool_input.get("dir_path"))
            .or_else(|| tool_input.get("directory"))
            .or_else(|| tool_input.get("path_pattern"))
            .or_else(|| tool_input.get("file_pattern"))
            .and_then(|v| v.as_str())
            .map(String::from);

        return ToolInput::FileSearch { pattern, path };
    }

    // 4. FileEdit
    if matches!(
        lower_name.as_str(),
        "edit"
            | "write"
            | "multiedit"
            | "replace"
            | "replace_file_content"
            | "write_to_file"
            | "write_file"
            | "edit_file"
            | "notebook_edit"
            | "notebookedit"
            | "create_file"
            | "modify_file"
            | "delete_file"
            | "apply_patch"
            | "str_replace"
    ) {
        let path = tool_input
            .get("file_path")
            .or_else(|| tool_input.get("path"))
            .or_else(|| tool_input.get("Path"))
            .or_else(|| tool_input.get("file"))
            .or_else(|| tool_input.get("notebook_path"))
            .or_else(|| tool_input.get("notebookpath"))
            .or_else(|| tool_input.get("NotebookPath"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let mut lines_added = None;
        let mut lines_removed = None;

        if let Some(edits) = tool_input.get("edits").and_then(|v| v.as_array()) {
            let mut added = 0;
            let mut removed = 0;
            for edit in edits {
                if let Some(old) = edit
                    .get("old_string")
                    .or_else(|| edit.get("old_text"))
                    .or_else(|| edit.get("old_str"))
                    .or_else(|| edit.get("Target"))
                    .and_then(|v| v.as_str())
                {
                    removed += count_lines(old);
                }
                if let Some(new) = edit
                    .get("new_string")
                    .or_else(|| edit.get("new_text"))
                    .or_else(|| edit.get("new_str"))
                    .or_else(|| edit.get("Replacement"))
                    .and_then(|v| v.as_str())
                {
                    added += count_lines(new);
                }
            }
            lines_added = Some(added);
            lines_removed = Some(removed);
        } else {
            if let Some(old) = tool_input
                .get("old_string")
                .or_else(|| tool_input.get("Target"))
                .or_else(|| tool_input.get("target"))
                .or_else(|| tool_input.get("old_str"))
                .or_else(|| tool_input.get("old_text"))
                .and_then(|v| v.as_str())
            {
                lines_removed = Some(count_lines(old));
            }

            if let Some(new) = tool_input
                .get("new_string")
                .or_else(|| tool_input.get("Replacement"))
                .or_else(|| tool_input.get("replacement"))
                .or_else(|| tool_input.get("new_str"))
                .or_else(|| tool_input.get("new_text"))
                .and_then(|v| v.as_str())
            {
                lines_added = Some(count_lines(new));
            }

            // Write counts content lines as added
            if let Some(content) = tool_input
                .get("content")
                .or_else(|| tool_input.get("Content"))
                .and_then(|v| v.as_str())
            {
                lines_added = Some(count_lines(content));
                if lines_removed.is_none() {
                    lines_removed = Some(0);
                }
            }
        }

        let kind = if lower_name.contains("write") || lower_name.contains("create") {
            FileEditKind::Create
        } else if lower_name.contains("delete") || lower_name.contains("remove") {
            FileEditKind::Delete
        } else if lower_name.contains("rename") || lower_name.contains("move") {
            FileEditKind::Rename
        } else {
            FileEditKind::Edit
        };

        return ToolInput::FileEdit {
            path,
            kind: Some(kind),
            lines_added,
            lines_removed,
        };
    }

    // 5. Subagent
    if matches!(
        lower_name.as_str(),
        "task"
            | "subagent"
            | "invoke_subagent"
            | "agent"
            | "swarm"
            | "spawn_agent"
            | "launch_subagent"
    ) {
        let description_len = tool_input
            .get("prompt")
            .or_else(|| tool_input.get("description"))
            .or_else(|| tool_input.get("task"))
            .or_else(|| tool_input.get("brief"))
            .or_else(|| tool_input.get("instruction"))
            .and_then(|v| v.as_str())
            .map(|s| s.len() as u64)
            .or_else(|| {
                tool_input.get("args").map(|a| {
                    if let Some(s) = a.as_str() {
                        s.len() as u64
                    } else {
                        canonical_json(a).len() as u64
                    }
                })
            });

        return ToolInput::Subagent { description_len };
    }

    // 6. Other: BLAKE3 8-byte hex digest + byte size of canonical JSON
    let bytes = canonical_json(tool_input);
    let input_bytes = Some(bytes.len() as u64);
    let input_digest = Some(blake3_digest_8_bytes(&bytes));

    ToolInput::Other {
        input_digest,
        input_bytes,
    }
}
