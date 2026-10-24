//! Hook invocation handler and PreToolUse guardrails for Claude Code, Gemini CLI, and Jetski.
//!
//! Hard constraints:
//! 1. Wall-clock budget: strictly <= 200 ms total.
//! 2. Output: prints NOTHING to stdout (stdout is intercepted by agent hook runners).
//! 3. Exit code: exits 0 on pass (or passive mode), exits 2 on PreToolUse guardrail refusal.
//! 4. Stdin limit: capped at 16 MiB.
//! 5. Absent daemon: spawns daemon detached without waiting, drops the event,
//!    and increments `$TACHO_HOME/hook-drops`.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tacho_core::decide::watch::config::WatchConfig;
use tacho_core::decide::watch::guard::shell_tokens;
use tacho_core::{Input, ToolInput};
use tacho_ingest::hooks::{claude, gemini, jetski, HookEnv};

use crate::cli::Provider;
use crate::client::DaemonClient;

const HARD_BUDGET_MS: u64 = 200;
const MAX_STDIN_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB

/// CPU burner binary names refused outside `tacho starve`.
const BURNER_NAMES: &[&str] = &["yes", "stress", "stress-ng"];

/// Wrappers that `burner-guard` looks through to inspect the underlying command.
const WRAPPERS: &[&str] = &[
    "timeout", "sudo", "nohup", "setsid", "env", "nice", "ionice", "taskset", "xargs", "time",
    "exec", "command", "chrt", "stdbuf",
];

/// Shell binaries whose `-c <script>` argument is recursively inspected by `burner-guard`.
const SHELL_WRAPPERS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];

fn is_burner_binary(name: &str) -> bool {
    BURNER_NAMES.contains(&name)
}

/// True iff `s` is a numeric duration or adjustment token (`10`, `30s`, `1.5m`, `-5`, `+10`).
fn is_duration_token(s: &str) -> bool {
    let rest = s.strip_prefix(['+', '-']).unwrap_or(s);
    let body = rest.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(rest);
    !body.is_empty()
        && body.bytes().any(|b| b.is_ascii_digit())
        && body.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

/// True iff `s` is a `taskset` CPU mask (`0x3`) or CPU range list (`0-3,8`).
fn is_cpu_mask_token(s: &str) -> bool {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    !s.is_empty()
        && s.bytes().any(|b| b.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b',')
}

/// Word-bounded check for `tacho starve` in `command`.
fn contains_tacho_starve(command: &str) -> bool {
    let tokens = shell_tokens(command);
    for pair in tokens.windows(2) {
        let bin = pair[0].rsplit('/').next().unwrap_or(&pair[0]);
        if bin == "tacho" && pair[1] == "starve" {
            return true;
        }
    }
    false
}

/// Split a shell command into `(stage, separator_after)` pairs on `||`, `&&`, `;`, `\n`, `|`, `&`,
/// and keyword boundaries `do`, `then`, `else`.
fn split_shell_stages(command: &str) -> Vec<(&str, Option<&str>)> {
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut stages = Vec::new();
    let mut last_end = 0;
    let mut i = 0;
    let mut quote: Option<u8> = None;

    let is_word_boundary = |idx: usize| -> bool {
        idx == 0
            || idx >= len
            || bytes[idx].is_ascii_whitespace()
            || matches!(bytes[idx], b';' | b'|' | b'&' | b'(' | b')' | b'{' | b'}')
    };

    while i < len {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' && q == b'"' && i + 1 < len {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' {
            quote = Some(b);
            i += 1;
            continue;
        }
        if b == b'\\' && i + 1 < len {
            i += 2;
            continue;
        }

        if i + 1 < len
            && ((b == b'|' && bytes[i + 1] == b'|') || (b == b'&' && bytes[i + 1] == b'&'))
        {
            let sep = &command[i..i + 2];
            stages.push((&command[last_end..i], Some(sep)));
            i += 2;
            last_end = i;
            continue;
        }
        if matches!(b, b';' | b'\n' | b'|' | b'&') {
            // Avoid splitting `2>&1` or `&>` redirections
            if b == b'&'
                && ((i > 0 && bytes[i - 1] == b'>') || (i + 1 < len && bytes[i + 1] == b'>'))
            {
                i += 1;
                continue;
            }
            let sep = &command[i..i + 1];
            stages.push((&command[last_end..i], Some(sep)));
            i += 1;
            last_end = i;
            continue;
        }

        for kw in ["then", "else", "do"] {
            let kw_len = kw.len();
            if i + kw_len <= len
                && &command[i..i + kw_len] == kw
                && is_word_boundary(i)
                && is_word_boundary(i + kw_len)
            {
                stages.push((&command[last_end..i], Some(kw)));
                i += kw_len;
                last_end = i;
                break;
            }
        }
        i += 1;
    }
    stages.push((&command[last_end..], None));
    stages
}

fn wrapper_flag_takes_value(wrapper: &str, flag: &str) -> bool {
    match wrapper {
        "timeout" => matches!(flag, "-k" | "--kill-after" | "-s" | "--signal"),
        "sudo" => matches!(
            flag,
            "-u" | "--user"
                | "-g"
                | "--group"
                | "-C"
                | "--close-from"
                | "-D"
                | "--chdir"
                | "-R"
                | "--chroot"
                | "-T"
                | "--command-timeout"
                | "-h"
                | "--host"
                | "-p"
                | "--prompt"
                | "-r"
                | "--role"
                | "-t"
                | "--type"
        ),
        "nice" => matches!(flag, "-n" | "--adjustment"),
        "ionice" => matches!(
            flag,
            "-c" | "--class"
                | "-n"
                | "--classdata"
                | "-p"
                | "--pid"
                | "-P"
                | "--pgid"
                | "-u"
                | "--uid"
        ),
        "taskset" => matches!(flag, "-c" | "--cpu-list"),
        "env" => matches!(flag, "-u" | "--unset" | "-C" | "--chdir"),
        "xargs" => matches!(
            flag,
            "-n" | "--max-args"
                | "-P"
                | "--max-procs"
                | "-I"
                | "-i"
                | "--replace"
                | "-L"
                | "--max-lines"
                | "-s"
                | "--max-chars"
                | "-d"
                | "--delimiter"
                | "-a"
                | "--arg-file"
                | "-E"
                | "-e"
        ),
        "chrt" => matches!(flag, "-p" | "--pid"),
        "stdbuf" => matches!(
            flag,
            "-i" | "--input" | "-o" | "--output" | "-e" | "--error"
        ),
        _ => false,
    }
}

fn is_shell_c_flag(flag: &str) -> bool {
    if flag == "-c" || flag == "--command" {
        return true;
    }
    if let Some(short) = flag.strip_prefix('-') {
        return !short.starts_with('-')
            && short.ends_with('c')
            && short.chars().all(|c| c.is_ascii_alphabetic());
    }
    false
}

/// Inspect a single pipeline stage's tokens, looking through wrappers (`timeout`, `env`, `sudo`,
/// `nice`, `ionice`, `nohup`, `setsid`, `taskset`, `xargs`, `time`, `exec`, `command`, `sh -c`, etc.).
pub fn stage_burner_name(stage: &str) -> Option<(String, bool)> {
    let tokens = shell_tokens(stage);
    inspect_tokens(&tokens)
}

fn inspect_tokens(tokens: &[String]) -> Option<(String, bool)> {
    let mut i = 0;
    let mut current_wrapper: Option<&str> = None;
    let mut timeout_needs_duration = false;
    let mut taskset_needs_mask = false;

    while i < tokens.len() {
        let raw_tok = &tokens[i];
        let stripped = raw_tok
            .trim_start_matches(['(', '{'])
            .trim_end_matches([')', '}']);
        if stripped.is_empty() {
            i += 1;
            continue;
        }

        if stripped.starts_with('>')
            || stripped.starts_with('<')
            || stripped.starts_with("1>")
            || stripped.starts_with("2>")
            || stripped.starts_with("&>")
        {
            i += 1;
            continue;
        }

        if !stripped.starts_with('-') && stripped.contains('=') {
            i += 1;
            continue;
        }

        if stripped.starts_with('-') {
            if let Some(w) = current_wrapper {
                if w == "env" && (stripped == "-S" || stripped == "--split-string") {
                    if let Some(script) = tokens.get(i + 1) {
                        if let Some(b) = check_nested_command_for_burner(script) {
                            return Some((b, false));
                        }
                        i += 2;
                        continue;
                    }
                }
                if w == "taskset" && (stripped == "-c" || stripped == "--cpu-list") {
                    taskset_needs_mask = false;
                    i += 2;
                    continue;
                }
                if wrapper_flag_takes_value(w, stripped) {
                    i += 2;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        let basename = stripped.rsplit('/').next().unwrap_or(stripped);
        if is_burner_binary(basename) {
            return Some((basename.to_string(), true));
        }

        if timeout_needs_duration && is_duration_token(stripped) {
            timeout_needs_duration = false;
            i += 1;
            continue;
        }

        if taskset_needs_mask && is_cpu_mask_token(stripped) && i + 1 < tokens.len() {
            taskset_needs_mask = false;
            i += 1;
            continue;
        }

        if is_duration_token(stripped) && current_wrapper.is_some() {
            i += 1;
            continue;
        }

        if SHELL_WRAPPERS.contains(&basename) {
            let mut j = i + 1;
            while j < tokens.len() {
                let arg = &tokens[j];
                if is_shell_c_flag(arg) {
                    if let Some(script) = tokens.get(j + 1) {
                        if let Some(b) = check_nested_command_for_burner(script) {
                            return Some((b, false));
                        }
                    }
                    return None;
                }
                if arg.starts_with('-') {
                    j += 1;
                } else {
                    break;
                }
            }
            return None;
        }

        if WRAPPERS.contains(&basename) {
            current_wrapper = Some(WRAPPERS.iter().copied().find(|&w| w == basename).unwrap());
            if basename == "timeout" {
                timeout_needs_duration = true;
            } else if basename == "taskset" {
                taskset_needs_mask = true;
            }
            i += 1;
            continue;
        }

        return None;
    }
    None
}

fn check_nested_command_for_burner(command: &str) -> Option<String> {
    if contains_tacho_starve(command) {
        return None;
    }
    for (stage, sep_after) in split_shell_stages(command) {
        if let Some((word, can_pipe)) = stage_burner_name(stage) {
            let feeds_pipe = can_pipe && sep_after == Some("|");
            if !(word == "yes" && feeds_pipe) {
                return Some(word);
            }
        }
    }
    None
}

/// Inspect a shell command and return a refusal message if any stage launches a CPU burner
/// (`yes`, `stress`, `stress-ng`) outside `tacho starve` (including through wrappers like
/// `timeout`, `env`, `sudo`, `nice`, `ionice`, `nohup`, `setsid`, `taskset`, `xargs`, `time`,
/// `exec`, `command`, and `sh -c` / `bash -c`).
///
/// `yes` directly feeding a pipe (`yes | ...`) is allowed because it answers an interactive prompt.
pub fn burner_guard_check(command: &str, under_starve: bool) -> Result<(), String> {
    if under_starve || contains_tacho_starve(command) {
        return Ok(());
    }
    if let Some(word) = check_nested_command_for_burner(command) {
        return Err(format!(
            "Refused: `{word}` is a CPU burner. Load generation runs only as `tacho starve -- <command>`, \
             which gives it a CPU slice of its own and stops the burners when the command ends. \
             tacho kills any other burner after 10 minutes."
        ));
    }
    let wc = WatchConfig::default();
    if let Some(refusal) = tacho_core::decide::watch::guard::burner_refusal(command, &wc) {
        return Err(refusal);
    }
    Ok(())
}

/// Check `no-background-bash`: refuse when `run_in_background` (or `background` / `async`) is truthy in `tool_input`.
pub fn no_background_bash_check(tool_input: &serde_json::Value) -> Result<(), String> {
    let flag_val = tool_input
        .get("run_in_background")
        .or_else(|| tool_input.get("background"))
        .or_else(|| tool_input.get("async"));
    let run_in_bg = match flag_val {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(serde_json::Value::String(s)) => {
            s == "1" || s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
        }
        _ => false,
    };
    if run_in_bg {
        let wc = WatchConfig::default();
        if let Some(msg) = tacho_core::decide::watch::guard::background_refusal(true, &wc) {
            return Err(msg);
        }
    }
    Ok(())
}

/// Validate a PreToolUse / BeforeTool JSON payload against all active worker guardrails:
/// 1. `no-background-bash` (`run_in_background` refusal)
/// 2. `burner-guard` (`yes`, `stress`, `stress-ng` refusal including wrappers & `sh -c`)
/// 3. Git & workflow hygiene (`git stash`, `git commit/push --no-verify`, `Co-Authored-By:` trailers)
pub fn check_pre_tool_use_guardrails(
    val: &serde_json::Value,
    under_starve: bool,
) -> Result<(), String> {
    let event_name = val
        .get("hook_event_name")
        .or_else(|| val.get("event"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !event_name.is_empty()
        && event_name != "PreToolUse"
        && event_name != "BeforeTool"
        && event_name != "tool_begin"
    {
        return Ok(());
    }

    let tool_name = val
        .get("tool_name")
        .or_else(|| val.get("tool"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let is_shell = matches!(
        tool_name,
        "Bash" | "bash" | "run_shell_command" | "run_command" | "shell"
    );
    if !is_shell {
        return Ok(());
    }

    let Some(tool_input) = val.get("tool_input") else {
        return Ok(());
    };

    no_background_bash_check(tool_input)?;

    let cmd = tool_input
        .get("command")
        .or_else(|| tool_input.get("cmd"))
        .or_else(|| tool_input.get("CommandLine"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    burner_guard_check(cmd, under_starve)?;

    let cmd_stays_starved = under_starve || contains_tacho_starve(cmd);
    let sanitized_cmd = cmd.replace("yes |", "true |").replace("yes|", "true|");
    tacho_core::decide::dispatch::guard_bash_tool(false, &sanitized_cmd, cmd_stays_starved)?;

    Ok(())
}

/// Handles the `tacho hook <provider>` execution.
pub fn execute_hook(provider: Provider) {
    let start_time = Instant::now();
    let client = DaemonClient::new();
    let home = crate::paths::resolve_home();

    let mut raw_bytes = Vec::new();
    if io::stdin()
        .take(MAX_STDIN_BYTES)
        .read_to_end(&mut raw_bytes)
        .is_err()
    {
        record_hook_drop(&home);
        return;
    }
    if raw_bytes.is_empty() {
        return;
    }

    let val: serde_json::Value = match serde_json::from_slice(&raw_bytes) {
        Ok(v) => v,
        Err(_) => {
            record_hook_drop(&home);
            return;
        }
    };

    if let Ok(agent_key) = std::env::var("TACHO_AGENT") {
        if !agent_key.is_empty() {
            let inbox_file = home.join("inbox").join(format!("{agent_key}.txt"));
            if inbox_file.is_file() {
                if let Ok(msg) = std::fs::read_to_string(&inbox_file) {
                    let _ = std::fs::remove_file(&inbox_file);
                    if !msg.trim().is_empty() {
                        eprintln!("[FLEET MESSAGE for {agent_key}]: {}", msg.trim());
                    }
                }
            }
        }
    }

    let guardrails_enabled = std::env::var("TACHO_GUARDRAILS").ok().as_deref() == Some("1")
        || std::env::var("TACHO_MANAGED_WORKER").ok().as_deref() == Some("1");
    if guardrails_enabled {
        let under_starve = std::env::var("TACHO_STARVE").ok().as_deref() == Some("1")
            || std::env::var("STARVE").ok().as_deref() == Some("1");
        if let Err(refusal) = check_pre_tool_use_guardrails(&val, under_starve) {
            eprintln!("{refusal}");
            std::process::exit(2);
        }
    }

    let mut inputs = map_hook_payload(provider, &val);
    if inputs.is_empty() {
        return;
    }

    let stream = match client.try_connect_api_fast() {
        Ok(s) => s,
        Err(_) => {
            if std::env::var("TACHO_NO_AUTOSPAWN").ok().as_deref() != Some("1") {
                let _ = client.spawn_daemon_detached();
            }
            record_hook_drop(&home);
            return;
        }
    };

    if start_time.elapsed() < Duration::from_millis(HARD_BUDGET_MS / 2) {
        enrich_git_trees(&mut inputs, &val);
    }

    let timeout_ms = HARD_BUDGET_MS
        .saturating_sub(start_time.elapsed().as_millis() as u64)
        .max(50);
    let _ = send_inputs_with_timeout(stream, &inputs, Duration::from_millis(timeout_ms));
}

/// Atomically increments the drop counter in `$TACHO_HOME/hook-drops` under `flock(LOCK_EX)`.
pub fn record_hook_drop(home_dir: &Path) {
    let _ = crate::paths::ensure_dir_0700(home_dir);
    let drop_file = home_dir.join("hook-drops");
    if let Ok(mut file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&drop_file)
    {
        unsafe {
            let _ = libc::flock(file.as_raw_fd(), libc::LOCK_EX);
        }
        let mut s = String::new();
        let _ = file.read_to_string(&mut s);
        let count = s.trim().parse::<u64>().unwrap_or(0);
        if file.seek(SeekFrom::Start(0)).is_ok() && file.set_len(0).is_ok() {
            let _ = file.write_all((count + 1).to_string().as_bytes());
            let _ = file.flush();
        }
        unsafe {
            let _ = libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn resolve_git_tree(cwd: Option<&Path>) -> Option<String> {
    let dir = cwd?;
    if !dir.is_dir() {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD^{tree}"])
        .current_dir(dir)
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    } else {
        None
    }
}

fn map_hook_payload(provider: Provider, val: &serde_json::Value) -> Vec<Input> {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let agent = std::env::var("TACHO_AGENT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| val.get("agent").and_then(|v| v.as_str()).map(String::from));
    let task = std::env::var("TACHO_TASK")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| val.get("task").and_then(|v| v.as_str()).map(String::from));
    let ppid = val
        .get("pid")
        .and_then(|v| v.as_u64())
        .map(|p| p as u32)
        .or_else(|| {
            let p = unsafe { libc::getppid() };
            (p > 0).then_some(p as u32)
        })
        .filter(|&p| p > 0);

    let env = HookEnv {
        agent,
        task,
        invoked_at_ns: now_ns,
        ppid,
    };

    match provider {
        Provider::Claude => claude::map(val, &env),
        Provider::Gemini => gemini::map(val, &env),
        Provider::Jetski => jetski::map(val, &env),
    }
}

fn enrich_git_trees(inputs: &mut [Input], val: &serde_json::Value) {
    let top_cwd = val.get("cwd").and_then(|v| v.as_str());
    for input in inputs {
        match input {
            Input::RunStart { cwd, git_tree, .. } => {
                if git_tree.is_none() {
                    *git_tree = resolve_git_tree(cwd.as_deref().or(top_cwd).map(Path::new));
                }
            }
            Input::ToolBegin {
                input: ToolInput::Shell { cwd, git_tree, .. },
                ..
            } => {
                if cwd.is_none() {
                    *cwd = top_cwd.map(String::from);
                }
                if git_tree.is_none() {
                    *git_tree = resolve_git_tree(cwd.as_deref().map(Path::new));
                }
            }
            _ => {}
        }
    }
}

fn send_inputs_with_timeout(
    mut stream: UnixStream,
    inputs: &[Input],
    timeout: Duration,
) -> io::Result<()> {
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout))?;

    for input in inputs {
        let msg = serde_json::json!({ "input": input });
        let mut line = serde_json::to_vec(&msg)?;
        line.push(b'\n');
        stream.write_all(&line)?;
    }
    stream.flush()?;

    let mut reader = io::BufReader::new(stream);
    let mut ack = String::new();
    for _ in inputs {
        ack.clear();
        if io::BufRead::read_line(&mut reader, &mut ack).unwrap_or(0) == 0 {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_payload(cmd: &str, run_in_background: Option<bool>) -> serde_json::Value {
        let mut input = serde_json::json!({ "command": cmd });
        if let Some(bg) = run_in_background {
            input["run_in_background"] = serde_json::Value::Bool(bg);
        }
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": input,
        })
    }

    #[test]
    fn test_burner_guard_refuses_direct_burners() {
        for cmd in [
            "yes",
            "yes >/dev/null",
            "yes > /dev/null 2>&1 &",
            "/usr/bin/yes >/dev/null",
            "stress --cpu 4",
            "/bin/stress -c 2",
            "stress-ng --cpu 4 --timeout 60s",
            "/usr/local/bin/stress-ng --matrix 0",
        ] {
            let res = burner_guard_check(cmd, false);
            assert!(res.is_err(), "expected burner refusal for `{cmd}`");
            let msg = res.unwrap_err();
            assert!(msg.contains("is a CPU burner"), "unexpected message: {msg}");
            assert!(msg.contains("tacho starve --"), "unexpected message: {msg}");
        }
    }

    #[test]
    fn test_burner_guard_refuses_all_wrappers_and_nested_sh_c() {
        for cmd in [
            "timeout 30 yes >/dev/null",
            "timeout -k 5 30s stress --cpu 2",
            "timeout --signal=KILL 1.5m stress-ng --cpu 1",
            "env FOO=1 BAR=baz yes >/dev/null",
            "env -i PATH=/usr/bin stress --cpu 2",
            "sudo yes >/dev/null",
            "sudo -u root stress-ng --cpu 4",
            "nice yes >/dev/null",
            "nice -n 10 stress --cpu 2",
            "nice -n -5 stress-ng --cpu 2",
            "ionice -c 3 yes >/dev/null",
            "ionice -c 2 -n 7 stress --cpu 4",
            "nohup yes >/dev/null &",
            "setsid stress --cpu 2",
            "taskset -c 0-3 yes >/dev/null",
            "taskset 0x3 stress-ng --cpu 2",
            "time yes >/dev/null",
            "exec yes >/dev/null",
            "command yes >/dev/null",
            "xargs -n 1 stress",
            "sh -c 'yes >/dev/null'",
            "bash -c \"stress --cpu 4\"",
            "/bin/sh -lc 'timeout 10 nice -n 5 ionice -c 3 yes >/dev/null'",
            "env FOO=1 sudo -u nobody nice -n 10 ionice -c 3 nohup timeout 60 sh -c 'stress-ng --cpu 2'",
            "(yes >/dev/null)",
            "{ stress --cpu 2; }",
            "echo setup && timeout 10 yes >/dev/null",
            "true || stress --cpu 1",
            "for i in 1 2; do yes >/dev/null; done",
        ] {
            let res = burner_guard_check(cmd, false);
            assert!(res.is_err(), "expected wrapper burner refusal for `{cmd}`");
        }
    }

    #[test]
    fn test_burner_guard_allows_yes_feeding_pipe_and_tacho_starve() {
        for cmd in [
            "yes | cargo install foo",
            "yes n | ./configure",
            "sh -c 'yes | head -n 5'",
            "tacho starve -- cargo test",
            "tacho starve -- stress-ng --cpu 4",
            "tacho starve -- yes >/dev/null",
            "cargo test --workspace",
            "git status",
            "echo yes",
            "grep stress Cargo.toml",
        ] {
            let res = burner_guard_check(cmd, false);
            assert!(res.is_ok(), "expected `{cmd}` to be allowed, got: {res:?}");
        }

        assert!(burner_guard_check("yes >/dev/null", true).is_ok());
        assert!(burner_guard_check("stress-ng --cpu 4", true).is_ok());
        assert!(burner_guard_check("stress --cpu 2 | cat", false).is_err());
        assert!(burner_guard_check("stress-ng --cpu 2 | cat", false).is_err());
        assert!(burner_guard_check("yes | yes", false).is_err());
    }

    #[test]
    fn test_no_background_bash_guardrail() {
        let blocked = bash_payload("cargo test", Some(true));
        let err = check_pre_tool_use_guardrails(&blocked, false).unwrap_err();
        assert!(err.contains("Denied: this session exits when you write your final message"));
        assert!(err.contains("tacho wait"));

        let allowed_false = bash_payload("cargo test", Some(false));
        assert!(check_pre_tool_use_guardrails(&allowed_false, false).is_ok());

        let allowed_none = bash_payload("cargo test", None);
        assert!(check_pre_tool_use_guardrails(&allowed_none, false).is_ok());
    }

    #[test]
    fn test_git_and_trailer_guardrails_in_pre_tool_use() {
        for bad_cmd in [
            "git stash",
            "git stash push -m wip",
            "git commit --no-verify -m 'wip'",
            "git push --no-verify",
            "git commit -m 'Fix bug\n\nCo-Authored-By: Bot <bot@example.com>'",
        ] {
            let payload = bash_payload(bad_cmd, None);
            assert!(
                check_pre_tool_use_guardrails(&payload, false).is_err(),
                "expected refusal for `{bad_cmd}`"
            );
        }
    }

    #[test]
    fn test_hostile_hook_inputs_and_string_numeric_background_flags() {
        // String and numeric variants of run_in_background must also be refused
        for hostile_bg in [
            serde_json::json!({"command": "cargo test", "run_in_background": "true"}),
            serde_json::json!({"command": "cargo test", "run_in_background": "1"}),
            serde_json::json!({"command": "cargo test", "run_in_background": 1}),
            serde_json::json!({"command": "cargo test", "background": true}),
            serde_json::json!({"command": "cargo test", "async": "true"}),
        ] {
            assert!(
                no_background_bash_check(&hostile_bg).is_err(),
                "expected background refusal for {hostile_bg}"
            );
        }

        // Deeply nested wrapper CPU burner (`sudo env timeout 10 sh -c "yes > /dev/null"`)
        let nested = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {
                "command": "sudo env timeout 10 sh -c \"yes > /dev/null\""
            }
        });
        assert!(check_pre_tool_use_guardrails(&nested, false).is_err());

        // Malformed / non-object payloads must not panic
        for weird in [
            serde_json::json!(null),
            serde_json::json!(42),
            serde_json::json!("not an object"),
            serde_json::json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": "string-not-object"}),
            serde_json::json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": 12345}}),
        ] {
            let _ = check_pre_tool_use_guardrails(&weird, false);
        }
    }
}
