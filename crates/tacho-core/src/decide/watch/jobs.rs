//! Named worktree jobs (`procs`: R49–R53, parity case 82):
//! - Name validation (`valid_job_name`, `job_name_problem`)
//! - Start validation and wrapper planning (`job_start_check`, `job_wrapper_script`)
//! - Bounded `wait_for` with injected clock/step and 60s heartbeat (`wait_for`, `last_log_line`, `job_wait`)
//! - Safe group kill with worktree `owns` + `pgid == pid` verification (`kill_group`, `job_kill`)

use std::path::Path;

use super::procs::owns;

/// Bounded wait limit for a tool call (`9 * 60 = 540` seconds, under 10 minutes) (R51).
pub const WAIT_LIMIT_SECS: i64 = 9 * 60;

/// Heartbeat interval while waiting on a named job (`60` seconds) (R51).
pub const WAIT_BEAT_SECS: i64 = 60;

/// Maximum length of a job name (`40` characters) (R49).
pub const MAX_JOB_NAME_LEN: usize = 40;

/// Whether `name` is a valid job name: 1..=40 characters of `[a-z0-9._-]` (R49).
pub fn valid_job_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_JOB_NAME_LEN
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
        })
}

/// Return the refusal sentence if `name` is not a valid job name, or `None` if valid (R49).
pub fn job_name_problem(name: &str) -> Option<String> {
    if valid_job_name(name) {
        None
    } else {
        Some(format!(
            "a name is lowercase letters, digits, dots, dashes and underscores, up to 40: '{name}'"
        ))
    }
}

/// Validate whether a named job `name` can be started, given an optional existing `(pid, is_alive)` (R49, R50).
/// Returns `Err(reason)` if refused, or `Ok(wrapper_script)` to execute detached.
pub fn job_start_check(
    name: &str,
    existing_pid: Option<(u32, bool)>,
    exit_file_path: &Path,
    tool: &str,
) -> Result<String, String> {
    if let Some(problem) = job_name_problem(name) {
        return Err(problem);
    }
    if let Some((pid, true)) = existing_pid {
        return Err(format!(
            "{name} is already running (pid {pid}). Wait: {tool} wait {name}   Stop it: {tool} kill {name}"
        ));
    }
    Ok(format!("\"$@\"; echo $? > {}", exit_file_path.display()))
}

/// Wait until `is_alive()` returns `false` (returns `0`) or `limit` seconds pass (returns `3`),
/// calling `say(elapsed)` every `beat` seconds and advancing via `step(1)` (R51).
/// Pure and deterministic: uses injected `clock` and `step` callbacks without wall-clock sleeps.
pub fn wait_for<IsAlive, Say, Clock, StepFn>(
    mut is_alive: IsAlive,
    mut say: Say,
    mut clock: Clock,
    mut step: StepFn,
    limit: i64,
    beat: i64,
) -> i32
where
    IsAlive: FnMut() -> bool,
    Say: FnMut(i64),
    Clock: FnMut() -> i64,
    StepFn: FnMut(i64),
{
    let began = clock();
    let mut last = began;
    while is_alive() {
        let now = clock();
        if now - began >= limit {
            return 3;
        }
        if now - last >= beat {
            say(now - began);
            last = now;
        }
        step(1);
    }
    0
}

/// Extract the last non-empty line of a log string, truncated to 160 characters (R51).
pub fn last_log_line(log_text: &str) -> String {
    log_text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.chars().take(160).collect())
        .unwrap_or_default()
}

/// Extract the last `n` non-empty lines of `text` joined by `\n` (R51).
pub fn tail_non_empty(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Pure simulation of `wait(target, wt)` with injected state callbacks (R51).
/// Returns `(exit_code, output_lines)`:
/// - `0` when the process ended
/// - `3` when it is still running at `WAIT_LIMIT_SECS`
/// - `1` when there is no such named process
#[allow(clippy::too_many_arguments)]
pub fn job_wait<IsAlive, HasExit, ReadLog, ReadExit, Clock, StepFn>(
    target: &str,
    named_pid: Option<u32>,
    mut is_alive: IsAlive,
    mut has_exit_file: HasExit,
    mut read_log: ReadLog,
    mut read_exit: ReadExit,
    clock: Clock,
    step: StepFn,
    tool: &str,
) -> (i32, Vec<String>)
where
    IsAlive: FnMut(u32) -> bool,
    HasExit: FnMut() -> bool,
    ReadLog: FnMut() -> Option<String>,
    ReadExit: FnMut() -> Option<String>,
    Clock: FnMut() -> i64,
    StepFn: FnMut(i64),
{
    let mut out = Vec::new();
    let is_numeric = !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit());
    let pid = if is_numeric {
        target.parse::<u32>().unwrap_or(0)
    } else {
        let Some(p) = named_pid else {
            out.push(format!(
                "nothing named {target} was started here. Start it with: {tool} run {target} -- <command>"
            ));
            return (1, out);
        };
        p
    };

    let rc = wait_for(
        || is_alive(pid) && (is_numeric || !has_exit_file()),
        |t| {
            let mins = t / 60;
            if is_numeric {
                out.push(format!("{target} still running after {mins} min"));
            } else {
                let last = read_log().map(|l| last_log_line(&l)).unwrap_or_default();
                out.push(format!(
                    "{target} still running after {mins} min; last line: {last}"
                ));
            }
        },
        clock,
        step,
        WAIT_LIMIT_SECS,
        WAIT_BEAT_SECS,
    );

    if rc == 3 {
        out.push(format!(
            "{target} is still running. Call again: {tool} wait {target}"
        ));
        return (3, out);
    }

    if is_numeric {
        out.push(format!("process {pid} ended"));
        return (0, out);
    }

    let code = read_exit()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown (it was killed)".to_string());
    let suffix = if code == "0" { "" } else { "  <- it FAILED" };
    out.push(format!("{target} ended with exit code {code}{suffix}"));
    if let Some(log_text) = read_log() {
        out.push(format!(
            "--- last lines of the log\n{}",
            tail_non_empty(&log_text, 25)
        ));
    }
    (0, out)
}

/// Signal a process group's leader and everything it started (`SIGTERM`, or `SIGKILL` when `now == true`),
/// returning whether the leader has exited without waiting for it (R52).
/// If `killpg` returns `ESRCH` (`Err(())`), the group is already gone (`true`).
pub fn kill_group<KillPg, Alive>(pid: u32, now: bool, mut killpg: KillPg, mut alive: Alive) -> bool
where
    KillPg: FnMut(u32, i32) -> Result<(), ()>,
    Alive: FnMut(u32) -> bool,
{
    let sig = if now { 9 } else { 15 };
    if killpg(pid, sig).is_err() {
        return true;
    }
    !alive(pid)
}

/// Stop a named process and everything it started if and only if its `cwd` is inside `wt`
/// and `pgid == pid` (R52). Returns `""` on success, or the refusal/timeout sentence.
#[allow(clippy::too_many_arguments)]
pub fn job_kill<Alive, GroupAlive, KillPg, Clock, StepFn>(
    name: &str,
    wt: &Path,
    now: bool,
    named_pid: Option<u32>,
    proc_cwd_and_pgid: Option<(&Path, u32)>,
    mut alive: Alive,
    mut group_alive: GroupAlive,
    killpg: KillPg,
    clock: Clock,
    step: StepFn,
    tool: &str,
) -> String
where
    Alive: FnMut(u32) -> bool,
    GroupAlive: FnMut(u32) -> bool,
    KillPg: FnMut(u32, i32) -> Result<(), ()>,
    Clock: FnMut() -> i64,
    StepFn: FnMut(i64),
{
    let Some(pid) = named_pid else {
        return format!("nothing named {name} was started here");
    };
    if !alive(pid) {
        return String::new();
    }
    let Some((cwd, pgid)) = proc_cwd_and_pgid else {
        return String::new();
    };
    if !owns(&cwd.to_string_lossy(), &wt.to_string_lossy()) || pgid != pid {
        return format!(
            "pid {pid} is not a process this worktree started (it runs in {}); not touching it",
            cwd.display()
        );
    }
    kill_group(pid, now, killpg, &mut alive);
    let rc = wait_for(
        || group_alive(pid),
        |_t| {},
        clock,
        step,
        WAIT_LIMIT_SECS,
        WAIT_BEAT_SECS,
    );
    if rc == 3 {
        return format!(
            "pid {pid} was sent SIGTERM and is still running; if it ignores it: {tool} kill {name} --now"
        );
    }
    String::new()
}
