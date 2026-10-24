//! PreToolUse Bash guard hooks: CPU burner refusal (R60) and background execution refusal (R61).

use super::config::WatchConfig;
use regex::Regex;
use std::sync::LazyLock;

static SEPARATOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\|\||&&|;|\n|\||&|\bdo\b|\bthen\b|\belse\b").unwrap());
static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\d.]+[smhd]?$").unwrap());

/// Split a shell stage into tokens using POSIX-style quoting (`shlex.split`),
/// falling back to `split_whitespace` when quotes are unbalanced.
pub fn shell_tokens(stage: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = stage.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    cur.push(ch);
                }
            }
            Some('"') => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' {
                    if let Some(&next) = chars.peek() {
                        if matches!(next, '"' | '\\' | '$' | '`') {
                            cur.push(chars.next().unwrap());
                        } else {
                            cur.push(ch);
                        }
                    } else {
                        return stage.split_whitespace().map(str::to_string).collect();
                    }
                } else {
                    cur.push(ch);
                }
            }
            Some(_) => unreachable!(),
            None => {
                if ch.is_whitespace() {
                    if in_token {
                        tokens.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                } else if ch == '\'' || ch == '"' {
                    in_token = true;
                    quote = Some(ch);
                } else if ch == '\\' {
                    in_token = true;
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    } else {
                        return stage.split_whitespace().map(str::to_string).collect();
                    }
                } else {
                    in_token = true;
                    cur.push(ch);
                }
            }
        }
    }

    if quote.is_some() {
        return stage.split_whitespace().map(str::to_string).collect();
    }
    if in_token {
        tokens.push(cur);
    }
    tokens
}

/// Extract the burner program name a single pipeline stage runs, looking through wrappers,
/// environment assignments (`X=Y`), flags (`-o`), and durations (`60`, `10m`, `1.5s`).
pub fn stage_burner_word(stage: &str, c: &WatchConfig) -> Option<String> {
    for token in shell_tokens(stage) {
        let stripped = token.trim_start_matches(['(', '{']);
        let name = stripped.rsplit('/').next().unwrap_or(stripped);
        if c.is_burner_name(name) {
            return Some(name.to_string());
        }
        if token.contains('=')
            || token.starts_with('-')
            || DURATION_RE.is_match(&token)
            || c.wrappers.iter().any(|w| w == name)
        {
            continue;
        }
        return None;
    }
    None
}

fn contains_tool_starve(command: &str, tool: &str) -> bool {
    let pattern = format!(r"\b{}\s+starve\b", regex::escape(tool));
    Regex::new(&pattern)
        .map(|re| re.is_match(command))
        .unwrap_or(false)
}

/// Inspect a shell `command` and return a refusal message if any stage launches a CPU burner
/// outside `<tool> starve` (R60).
///
/// `yes` directly feeding a pipe (`yes | ...`) answers an interactive prompt and is allowed.
pub fn burner_refusal(command: &str, c: &WatchConfig) -> Option<String> {
    if contains_tool_starve(command, &c.tool) {
        return None;
    }
    let mut stages: Vec<(&str, Option<&str>)> = Vec::new();
    let mut last_end = 0;
    for m in SEPARATOR_RE.find_iter(command) {
        stages.push((&command[last_end..m.start()], Some(m.as_str())));
        last_end = m.end();
    }
    stages.push((&command[last_end..], None));

    for (stage, sep_after) in stages {
        if let Some(word) = stage_burner_word(stage, c) {
            let feeds_pipe = sep_after == Some("|");
            if !(word == "yes" && feeds_pipe) {
                return Some(format!(
                    "Refused: `{word}` is a CPU burner. Load generation runs only as `{tool} starve -- <command>`, \
                     which gives it a CPU slice of its own and stops the burners when the command ends. \
                     {tool} kills any other burner after 10 minutes.",
                    tool = c.tool
                ));
            }
        }
    }
    None
}

/// Refuse `run_in_background: true` on a Bash tool invocation (R61).
pub fn background_refusal(run_in_background: bool, c: &WatchConfig) -> Option<String> {
    if run_in_background {
        Some(format!(
            "Denied: this session exits when you write your final message, and a background command dies with it. \
             Run it in the foreground with a timeout, or start it with `{tool} run` and wait on it with `{tool} wait`.",
            tool = c.tool
        ))
    } else {
        None
    }
}
