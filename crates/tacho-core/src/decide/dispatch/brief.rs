//! Brief assembly, provider argv expansion, session/settings flags, and worktree slug/port helpers
//! (R51, R53–R56, R80).

use regex::Regex;
use std::collections::BTreeSet;
use std::sync::LazyLock;

use super::config::DispatchConfig;
use super::model::RunKey;

/// Assemble the full prompt handed to a fresh run (R51).
pub fn brief(task: &str, key: &str, cfg: &DispatchConfig) -> String {
    let first = match cfg.brief.preamble.as_deref().filter(|s| !s.is_empty()) {
        Some(pre) => format!("Read {pre} and follow it exactly. Then do"),
        None => "Do".to_string(),
    };
    let base = RunKey::parse(key).base;
    let mut rules = String::new();
    for (prefix, rule) in &cfg.brief.key_rules {
        if base.starts_with(prefix.as_str()) {
            rules.push_str("\n\n");
            rules.push_str(rule);
            break;
        }
    }
    let self_update = match cfg
        .brief
        .self_update_rule
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(su) => format!("\n\n{su}"),
        None => String::new(),
    };
    let note = cfg
        .brief
        .note_instruction
        .replace("{tool}", &cfg.tool)
        .replace("{key}", &base);
    let note_section = if note.is_empty() {
        String::new()
    } else {
        format!("\n\n{note}")
    };
    format!(
        "{first} this task in the repository at {}, and finish with at most {} lines saying what \
         you did and the PR number.\n\n{task}{rules}{self_update}{note_section}",
        cfg.repo, cfg.brief.report_lines
    )
}

/// Split a shell-like command template into words (handling single and double quotes).
pub fn split_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                quote = None;
            }
            Some('"') if c == '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            Some(_) => {
                current.push(c);
            }
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c == '\\' => {
                in_word = true;
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                in_word = true;
                current.push(c);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// Expand a provider command template without splitting `brief` into separate arguments (R55).
pub fn command_argv(command: &str, model: &str, brief_text: &str) -> Vec<String> {
    split_words(command)
        .into_iter()
        .map(|part| {
            part.replace("{model}", model)
                .replace("{brief}", brief_text)
        })
        .collect()
}

fn executable_basename(command_or_prog: &str) -> &str {
    let first = command_or_prog
        .split_whitespace()
        .next()
        .unwrap_or(command_or_prog);
    first.rsplit('/').next().unwrap_or(first)
}

/// True when the command's executable supports named/resumable sessions and hook settings (R54).
pub fn has_sessions(command_or_prog: &str, cfg: &DispatchConfig) -> bool {
    let base = executable_basename(command_or_prog);
    cfg.providers.session_capable.iter().any(|s| s == base)
}

/// Add `--session-id <session>` or `--resume <session>` right after `argv[0]` when `session` is non-empty (R54).
pub fn with_session(mut argv: Vec<String>, session: &str, resume: bool) -> Vec<String> {
    if session.is_empty() || argv.is_empty() {
        return argv;
    }
    let flag = if resume { "--resume" } else { "--session-id" };
    argv.splice(1..1, [flag.to_string(), session.to_string()]);
    argv
}

/// The Claude settings JSON that denies a background Bash call via the hook script (R53).
pub fn settings_json(hook_script: &str) -> String {
    serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 -I {hook_script}")
                }]
            }]
        }
    })
    .to_string()
}

/// Add `--settings <path>` to a session-capable command; other commands are unchanged (R53).
pub fn with_settings(mut argv: Vec<String>, path: &str, cfg: &DispatchConfig) -> Vec<String> {
    if argv.is_empty() || !has_sessions(&argv[0], cfg) {
        return argv;
    }
    argv.splice(1..1, ["--settings".to_string(), path.to_string()]);
    argv
}

/// Names the executable actually selected when `selected == "configured"` (R56).
pub fn provider_name(command: &str, selected: &str) -> String {
    if selected != "configured" {
        return selected.to_string();
    }
    let words = split_words(command);
    match words.first() {
        Some(w) if !w.is_empty() => executable_basename(w).to_string(),
        _ => "unknown".to_string(),
    }
}

static SLUG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+(-[a-z0-9]+)*$").unwrap());

/// Why `slug` cannot name a worktree branch and directory, or `None` (R80).
pub fn slug_problem(slug: &str) -> Option<String> {
    if !(3..=80).contains(&slug.len()) || !SLUG_RE.is_match(slug) {
        Some(
            "a slug is 3 to 80 characters of lowercase letters, digits and single dashes, e.g. a-held-rate-can-be-discarded"
                .to_string(),
        )
    } else {
        None
    }
}

pub const PORT_LOW: u16 = 20_000;
pub const PORT_SIZE: u16 = 20;
pub const PORT_BLOCKS: u16 = 600;

/// The first port of a block of 20 from 20000 (600 blocks) that no live worktree holds and nothing
/// listens on, or `None` (R80).
pub fn allocate_ports(taken: &BTreeSet<u16>, is_busy: impl Fn(u16) -> bool) -> Option<u16> {
    for i in 0..PORT_BLOCKS {
        let base = PORT_LOW + i * PORT_SIZE;
        if !taken.contains(&base) && !(0..PORT_SIZE).any(|k| is_busy(base + k)) {
            return Some(base);
        }
    }
    None
}
