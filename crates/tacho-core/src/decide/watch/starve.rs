//! CPU starvation planner and runner (`<tool> starve -- <cmd>`).
//!
//! Requirements R54–R59, parity cases 60–81.

use super::config::{StarveConfig, WatchConfig};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

static CPU_LIST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+(-\d+)?(,\d+(-\d+)?)*$").unwrap());

/// A planned CPU starvation slice (`plan_starve`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarvePlan {
    pub cpus: String,
    pub burners: usize,
    pub wrap: Option<Vec<String>>,
    pub problem: String,
}

impl StarvePlan {
    /// Convert the plan into a `Result`, returning `Err(self.problem)` if `wrap` is `None`.
    pub fn into_result(self) -> Result<Self, String> {
        if self.wrap.is_some() {
            Ok(self)
        } else {
            Err(self.problem)
        }
    }
}

/// Format an ordered slice of CPU IDs into compact range notation (e.g. `[0, 1, 2, 3, 8]` -> `"0-3,8"`).
pub fn format_cpu_ids(cpu_ids: &[usize]) -> String {
    let mut iter = cpu_ids.iter().copied();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut parts = Vec::new();
    let mut start = first;
    let mut prev = first;
    for cpu in iter {
        if cpu != prev + 1 {
            if start != prev {
                parts.push(format!("{start}-{prev}"));
            } else {
                parts.push(start.to_string());
            }
            start = cpu;
        }
        prev = cpu;
    }
    if start != prev {
        parts.push(format!("{start}-{prev}"));
    } else {
        parts.push(start.to_string());
    }
    parts.join(",")
}

/// Whether `cpu_set` matches `^\d+(-\d+)?(,\d+(-\d+)?)*$` with `low <= high` in every range.
pub fn valid_cpu_list(cpu_set: &str) -> bool {
    if !CPU_LIST_RE.is_match(cpu_set) {
        return false;
    }
    for part in cpu_set.split(',') {
        let (low_s, high_s) = part.split_once('-').unwrap_or((part, part));
        let (Ok(low), Ok(high)) = (low_s.parse::<usize>(), high_s.parse::<usize>()) else {
            return false;
        };
        if low > high {
            return false;
        }
    }
    true
}

/// Count the total CPUs named in a validated `cpu_set`.
pub fn count_cpus(cpu_set: &str) -> usize {
    let mut total = 0;
    for part in cpu_set.split(',') {
        let (low_s, high_s) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(low), Ok(high)) = (low_s.parse::<usize>(), high_s.parse::<usize>()) {
            total += high.saturating_sub(low) + 1;
        }
    }
    total
}

/// Whether every CPU in `cpu_set` is present in `available`.
pub fn within_affinity(cpu_set: &str, available: &[usize]) -> bool {
    for part in cpu_set.split(',') {
        let (low_s, high_s) = part.split_once('-').unwrap_or((part, part));
        let (Ok(low), Ok(high)) = (low_s.parse::<usize>(), high_s.parse::<usize>()) else {
            return false;
        };
        let found = available
            .iter()
            .filter(|&&cpu| low <= cpu && cpu <= high)
            .count();
        if found != high - low + 1 {
            return false;
        }
    }
    true
}

/// Plan the CPU set, burner count, and pinning wrapper for `<tool> starve` (R54–R56).
///
/// - Default slice is `size` CPUs (default 4), capped at `max(1, total_cpus / c.max_share)`.
/// - Default set uses the highest available CPU IDs so the lowest stay with the machine.
/// - Wrapper prefers `taskset -c <set>`, falling back to `systemd-run --user --scope --quiet -p AllowedCPUs=<set>`.
pub fn plan_starve(
    total_cpus: usize,
    size: usize,
    cpu_set: Option<&str>,
    taskset: bool,
    scope: bool,
    available_cpus: Option<&[usize]>,
    c: &StarveConfig,
) -> StarvePlan {
    let available: Vec<usize> = match available_cpus {
        Some(a) => a.to_vec(),
        None => (0..total_cpus).collect(),
    };
    let limit = (total_cpus / c.max_share.max(1)).max(1);
    if size < 1 {
        return StarvePlan {
            cpus: cpu_set.unwrap_or("").to_string(),
            burners: 0,
            wrap: None,
            problem: "the CPU slice must contain at least one CPU".to_string(),
        };
    }
    let chosen = match cpu_set {
        Some(s) => s.to_string(),
        None => {
            let take_n = size.min(limit).min(available.len());
            format_cpu_ids(&available[available.len().saturating_sub(take_n)..])
        }
    };
    if !valid_cpu_list(&chosen) {
        return StarvePlan {
            cpus: chosen.clone(),
            burners: 0,
            wrap: None,
            problem: format!("'{chosen}' is not a CPU list; write e.g. 0-3,8"),
        };
    }
    if !within_affinity(&chosen, &available) {
        return StarvePlan {
            cpus: chosen.clone(),
            burners: 0,
            wrap: None,
            problem: format!("{chosen} names CPUs outside this process's available affinity"),
        };
    }
    let count = count_cpus(&chosen);
    if count > limit {
        return StarvePlan {
            cpus: chosen.clone(),
            burners: 0,
            wrap: None,
            problem: format!(
                "{chosen} exceeds the starve limit of {limit} CPUs (one eighth of this {total_cpus}-CPU fleet)"
            ),
        };
    }
    let burners = c.burners_per_cpu * count;
    if taskset {
        return StarvePlan {
            cpus: chosen.clone(),
            burners,
            wrap: Some(vec!["taskset".to_string(), "-c".to_string(), chosen]),
            problem: String::new(),
        };
    }
    if scope {
        return StarvePlan {
            cpus: chosen.clone(),
            burners,
            wrap: Some(vec![
                "systemd-run".to_string(),
                "--user".to_string(),
                "--scope".to_string(),
                "--quiet".to_string(),
                "-p".to_string(),
                format!("AllowedCPUs={chosen}"),
            ]),
            problem: String::new(),
        };
    }
    StarvePlan {
        cpus: chosen,
        burners,
        wrap: None,
        problem:
            "neither taskset nor a systemd user scope is available to pin the CPUs; install util-linux"
                .to_string(),
    }
}

/// Outcome of the foreground command call inside [`run_starve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StarveCallError {
    TimedOut,
    CannotStart(String),
}

/// Execute a planned starvation run via injected callbacks (`spawn`, `call`, `stop`, `say`),
/// guaranteeing every spawned burner is stopped in all exit/error paths (R57–R58).
#[allow(clippy::too_many_arguments)]
pub fn run_starve<FSpawn, FCall, FStop, FSay>(
    command: &[String],
    plan: &StarvePlan,
    batch_running: bool,
    base_env: &BTreeMap<String, String>,
    c: &WatchConfig,
    mut spawn: FSpawn,
    mut call: FCall,
    mut stop: FStop,
    mut say: FSay,
) -> i32
where
    FSpawn: FnMut(&[String], &BTreeMap<String, String>) -> u32,
    FCall: FnMut(&[String], &BTreeMap<String, String>, u64) -> Result<i32, StarveCallError>,
    FStop: FnMut(u32),
    FSay: FnMut(&str),
{
    let Some(ref wrap) = plan.wrap else {
        say(&plan.problem);
        return 1;
    };
    let mut env = base_env.clone();
    env.insert(c.starve_env(), "1".to_string());
    env.insert("STARVE".to_string(), "1".to_string());

    let max_secs = c.starve.max_call_secs;
    say(&format!(
        "running on CPUs {} with {} burners for at most {max_secs} seconds",
        plan.cpus, plan.burners
    ));

    if batch_running {
        say("refusing to starve while a merge-check batch is running");
        return 1;
    }

    let mut started = Vec::with_capacity(plan.burners);
    let mut burner_argv = wrap.clone();
    burner_argv.push("timeout".to_string());
    burner_argv.push(max_secs.to_string());
    burner_argv.push("yes".to_string());

    for _ in 0..plan.burners {
        started.push(spawn(&burner_argv, &env));
    }

    let mut cmd_argv = wrap.clone();
    cmd_argv.extend(command.iter().cloned());

    let rc = match call(&cmd_argv, &env, max_secs) {
        Ok(code) => code,
        Err(StarveCallError::TimedOut) => {
            say(&format!(
                "command exceeded the {max_secs}-second starve limit"
            ));
            124
        }
        Err(StarveCallError::CannotStart(err)) => {
            let cmd_name = command.first().map(String::as_str).unwrap_or("");
            say(&format!("could not run {cmd_name}: {err}"));
            127
        }
    };

    for pid in started {
        stop(pid);
    }
    rc
}

/// Bounded foreground call helper: communicates up to `timeout`; on timeout, calls
/// `kill_group(pid, true)` (`SIGKILL` with `now=true`) once and `wait(pid)` once (R58, parity case 80).
pub fn bounded_call<FComm, FKillGroup, FWait>(
    pid: u32,
    timeout: u64,
    mut communicate: FComm,
    mut kill_group: FKillGroup,
    mut wait: FWait,
) -> Result<i32, StarveCallError>
where
    FComm: FnMut(u64) -> Result<i32, StarveCallError>,
    FKillGroup: FnMut(u32, bool),
    FWait: FnMut(u32),
{
    match communicate(timeout) {
        Ok(rc) => Ok(rc),
        Err(StarveCallError::TimedOut) => {
            kill_group(pid, true);
            wait(pid);
            Err(StarveCallError::TimedOut)
        }
        Err(e) => Err(e),
    }
}

/// Whether a registered starve burner pid should be stopped when a batch preempts starve (R59):
/// true iff `pgid == pid`, its environment carries `<TOOL>_STARVE=1` (or `STARVE=1`), and its cmdline contains `"yes"`.
pub fn should_stop_burner(
    pid: u32,
    pgid: u32,
    environ: &[String],
    cmdline: &[String],
    c: &WatchConfig,
) -> bool {
    if pgid != pid {
        return false;
    }
    let marker = format!("{}=1", c.starve_env());
    let has_env = environ.iter().any(|e| e == &marker || e == "STARVE=1");
    let has_yes = cmdline.iter().any(|arg| arg == "yes");
    has_env && has_yes
}
