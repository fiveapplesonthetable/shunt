//! Machine resource budgets: cgroup v2 CPU quota meter, busy gate, systemd slices/units,
//! Docker container cgroup flags, host readings, and container job CPU/signal rules.
//!
//! Requirements R62–R68, parity cases 83–87.

use super::config::WatchConfig;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

static SELF_HOSTED_CPUS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{\{\s*vars\.CI_RUNS_ON\s*&&\s*'--cpus=(\d+)'\s*\|\|\s*''\s*\}\}").unwrap()
});

/// Nearest cgroup v2 ancestor carrying a finite CPU quota (R65).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub path: String,
    pub cpus: f64,
}

/// Parse the allowed CPUs from a cgroup v2 `cpu.max` line (`"<quota> <period>"` -> `quota / period`),
/// or `None` when unlimited (`"max 100000"`) or malformed (R65).
pub fn parse_cpus(cpu_max: &str) -> Option<f64> {
    let (quota_s, period_s) = cpu_max.trim().split_once(' ')?;
    let quota: i64 = quota_s.trim().parse().ok()?;
    let period: i64 = period_s.trim().parse().ok()?;
    if quota > 0 && period > 0 {
        let cpus = (quota as f64) / (period as f64);
        if cpus > 0.0 {
            return Some(cpus);
        }
    }
    None
}

/// Alias for [`parse_cpus`].
pub fn parse_cpu_max(cpu_max: &str) -> Option<f64> {
    parse_cpus(cpu_max)
}

/// Parse `usage_usec` from a cgroup v2 `cpu.stat` text (R65).
pub fn parse_usage(cpu_stat: &str) -> Option<u64> {
    for line in cpu_stat.lines() {
        let (key, value) = line.trim().split_once(' ')?;
        if key == "usage_usec" {
            return value.trim().parse::<u64>().ok();
        }
    }
    None
}

/// Share of `cpus` that `used_usec` of CPU time over `seconds` represents, as a percentage (R65).
pub fn percent(used_usec: u64, seconds: f64, cpus: f64) -> f64 {
    if seconds <= 0.0 || cpus <= 0.0 {
        return 0.0;
    }
    100.0 * (used_usec as f64) / (seconds * 1_000_000.0 * cpus)
}

/// Find the nearest ancestor of `own_cgroup` (from its v2 `0::` line) that carries a finite `cpu.max` quota (R65).
///
/// `read_cpu_max` is called with the ancestor directory path (or `<dir>/cpu.max`).
pub fn find_budget<F>(root: &str, own_cgroup: &str, mut read_cpu_max: F) -> Option<Budget>
where
    F: FnMut(&str) -> Option<String>,
{
    let line = own_cgroup.lines().find(|l| l.starts_with("0::"))?;
    let mut parts: Vec<&str> = line[3..].split('/').filter(|p| !p.is_empty()).collect();
    let trimmed_root = root.trim_end_matches('/');
    while !parts.is_empty() {
        let dir = format!("{trimmed_root}/{}", parts.join("/"));
        let file = format!("{dir}/cpu.max");
        let content = read_cpu_max(&file).or_else(|| read_cpu_max(&dir));
        if let Some(text) = content {
            if let Some(cpus) = parse_cpus(&text) {
                return Some(Budget { path: dir, cpus });
            }
        }
        parts.pop();
    }
    None
}

/// Stateful pure cgroup v2 CPU usage meter (R65).
///
/// Requires a window of at least `window_secs` (default 30.0 s) between baseline and current reading;
/// readings inside the window repeat the previous percentage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    pub budget: Option<Budget>,
    pub baseline: Option<(f64, u64)>,
    pub last: Option<f64>,
    pub window_secs: f64,
}

impl Meter {
    pub fn new(budget: Option<Budget>, window_secs: f64) -> Self {
        Self {
            budget,
            baseline: None,
            last: None,
            window_secs,
        }
    }

    /// Record a new `cpu.stat` sample at `now` (in seconds) and return the latest quota percentage.
    pub fn sample(&mut self, now: f64, cpu_stat: &str) -> Option<f64> {
        let budget = self.budget.as_ref()?;
        let used = parse_usage(cpu_stat)?;
        match self.baseline {
            None => {
                self.baseline = Some((now, used));
            }
            Some((base_at, base_used)) if now - base_at >= self.window_secs => {
                let delta_usec = used.saturating_sub(base_used);
                self.last = Some(percent(delta_usec, now - base_at, budget.cpus));
                self.baseline = Some((now, used));
            }
            Some(_) => {}
        }
        self.last
    }
}

/// Resolve the configured CPU percentage limit from `<prefix>MAX_CPU_PERCENT` or legacy `<prefix>MAX_LOAD`.
pub fn cpu_limit_setting(
    percent_env: Option<&str>,
    load_env: Option<&str>,
    usable_cpus: usize,
    fallback: Option<f64>,
    default_percent: f64,
    env_prefix: &str,
) -> (f64, Option<String>) {
    let pct_name = format!("{env_prefix}MAX_CPU_PERCENT");
    let load_name = format!("{env_prefix}MAX_LOAD");
    let (name, raw, legacy) = if let Some(v) = percent_env {
        (pct_name, v, false)
    } else if let Some(v) = load_env {
        (load_name, v, true)
    } else {
        return (default_percent, None);
    };
    let parsed = raw.parse::<f64>().unwrap_or(f64::NAN);
    let cpus = usable_cpus.max(1) as f64;
    let maximum = if legacy { cpus * 2.0 } else { 100.0 };
    if parsed.is_finite() && (0.0..=maximum).contains(&parsed) {
        let limit = if legacy {
            (parsed / cpus * 100.0).min(100.0)
        } else {
            parsed
        };
        return (limit, None);
    }
    let kind_word = if legacy { "load" } else { "percentage" };
    let used_fallback = if fallback.is_some() {
        "the previous limit"
    } else {
        "the default"
    };
    let problem = format!(
        "{name}={raw} must be a {kind_word} from 0 to {}; using {used_fallback}",
        maximum as i64
    );
    (fallback.unwrap_or(default_percent), Some(problem))
}

/// Machine busy gate (R64):
/// - Busy if any watched mount has `< c.low_disk_gb` (20 GB) free.
/// - Otherwise busy iff `cpu_percent > c.max_cpu_percent`.
/// - When `cpu_percent` is `None`, falls back to `max(load1, load5) > usable_cpus * c.max_cpu_percent / 100.0`.
pub fn busy(
    free_gbs: &[f64],
    cpu_percent: Option<f64>,
    load1: f64,
    load5: f64,
    usable_cpus: usize,
    c: &WatchConfig,
) -> bool {
    if free_gbs.iter().any(|&free| free < c.low_disk_gb) {
        return true;
    }
    if let Some(used) = cpu_percent {
        return used > c.max_cpu_percent;
    }
    load1.max(load5) > (usable_cpus as f64) * c.max_cpu_percent / 100.0
}

/// Systemd slice kind for wrapping agent or batch processes (R62).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SliceKind {
    Agent,
    Batch,
}

impl SliceKind {
    pub fn slice_name(&self, c: &WatchConfig) -> String {
        match self {
            SliceKind::Agent => format!("{}-agent.slice", c.slice_prefix),
            SliceKind::Batch => format!("{}-batch.slice", c.slice_prefix),
        }
    }

    pub fn scope_limits(&self) -> &'static [&'static str] {
        match self {
            SliceKind::Agent => &["MemoryHigh=12G", "MemoryMax=16G"],
            SliceKind::Batch => &["MemoryMax=48G"],
        }
    }

    pub const fn cpu_quota_percent(&self) -> u32 {
        2400
    }

    pub const fn cpu_weight(&self) -> u32 {
        match self {
            SliceKind::Agent => 20,
            SliceKind::Batch => 100,
        }
    }
}

/// Wrap `argv` in a transient `systemd-run --user --scope` unit inside `kind`'s slice (R62).
/// Leaves `argv` unchanged when `systemd-run` or `XDG_RUNTIME_DIR` is unavailable.
pub fn scoped_argv(
    kind: SliceKind,
    argv: &[String],
    unit: &str,
    have_systemd_run: bool,
    have_xdg_runtime_dir: bool,
    c: &WatchConfig,
) -> Vec<String> {
    if !have_systemd_run || !have_xdg_runtime_dir {
        return argv.to_vec();
    }
    let mut out = vec![
        "systemd-run".to_string(),
        "--user".to_string(),
        "--scope".to_string(),
        "--quiet".to_string(),
        "--collect".to_string(),
        format!("--slice={}", kind.slice_name(c)),
    ];
    if !unit.is_empty() {
        out.push(format!("--unit={unit}"));
    }
    for prop in kind.scope_limits() {
        out.push("-p".to_string());
        out.push((*prop).to_string());
    }
    out.extend(argv.iter().cloned());
    out
}

/// Scope unit name for one start of `key`: `<prefix>-agent-<sanitized_key>-<started>` (R63).
pub fn agent_unit_name(key: &str, started: i64, prefix: &str) -> String {
    let sanitized: String = key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    format!("{prefix}-agent-{sanitized}-{started}")
}

/// Command argv to stop a transient user scope unit (R63).
pub fn stop_unit_argv(unit: &str) -> Vec<String> {
    vec![
        "systemctl".to_string(),
        "--user".to_string(),
        "stop".to_string(),
        format!("{unit}.scope"),
    ]
}

fn normalize_socket_path(p: &str) -> String {
    let trimmed = p.trim();
    if let Some(rest) = trimmed.strip_prefix("/var/run/") {
        format!("/run/{rest}")
    } else {
        trimmed.to_string()
    }
}

/// Whether the selected Docker endpoint is the local rootful daemon (R66).
pub fn local_root_daemon(endpoint: &str, security: &[String], root_socket: &str) -> bool {
    let Some(sock) = endpoint.strip_prefix("unix://") else {
        return false;
    };
    if security.iter().any(|s| s == "name=rootless") {
        return false;
    }
    normalize_socket_path(sock) == normalize_socket_path(root_socket)
}

/// Compute Docker CPU and `--cgroup-parent` flags for the selected daemon (R66).
#[allow(clippy::too_many_arguments)]
pub fn docker_flags(
    endpoint: &str,
    driver: &str,
    security: &[String],
    cpu_max: &str,
    limit: i32,
    owner_quota: u32,
    strict: bool,
    c: &WatchConfig,
) -> Result<Vec<String>, String> {
    if limit <= 0 {
        return Err("Docker CPU limit must be positive".to_string());
    }
    if driver.is_empty() {
        return Err("Docker did not report a valid cgroup driver or owner quota".to_string());
    }
    if !local_root_daemon(endpoint, security, &c.docker_socket) || !strict {
        return Ok(vec![format!("--cpus={limit}")]);
    }
    if driver != "systemd" {
        return Err(format!(
            "local root Docker needs the systemd cgroup driver for {}; configure Docker or use rootless Docker",
            c.owner_slice
        ));
    }
    let parts: Vec<&str> = cpu_max.split_whitespace().collect();
    if parts.len() != 2
        || !parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!(
            "{} needs a finite CPU quota before local root Docker jobs start",
            c.owner_slice
        ));
    }
    let quota: u64 = parts[0].parse().unwrap_or(0);
    let period: u64 = parts[1].parse().unwrap_or(0);
    if period == 0 {
        return Err(format!(
            "{} needs a finite CPU quota before local root Docker jobs start",
            c.owner_slice
        ));
    }
    if quota > (owner_quota as u64) * period {
        return Err(format!(
            "{} quota exceeds the configured {owner_quota} CPUs; apply the partition before starting Docker jobs",
            c.owner_slice
        ));
    }
    Ok(vec![
        format!("--cgroup-parent={}", c.owner_slice),
        format!("--cpus={limit}"),
    ])
}

/// Parse `CPUQuota=<k*100>%` from a slice `.conf` file into whole CPUs (`k`) (R66).
pub fn configured_owner_quota(conf_text: &str) -> Result<u32, String> {
    for line in conf_text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("CPUQuota=") {
            if let Some(pct_str) = rest.strip_suffix('%') {
                let pct: u32 = pct_str
                    .parse()
                    .map_err(|_| "invalid owner CPU quota".to_string())?;
                if pct > 0 && pct % 100 == 0 {
                    return Ok(pct / 100);
                }
            }
            return Err("invalid owner CPU quota".to_string());
        }
    }
    Err("invalid owner CPU quota".to_string())
}

/// High-level Docker container flag resolver supporting `strict` (`--managed`) and `prefer_owner` (`--prefer-owner`) modes (R66).
#[allow(clippy::too_many_arguments)]
pub fn container_flags(
    limit: i32,
    driver: &str,
    security: &[String],
    endpoint: &str,
    cgroup_cpu_max: Option<&str>,
    owner_quota_res: Result<u32, String>,
    strict: bool,
    prefer_owner: bool,
    c: &WatchConfig,
) -> Result<Vec<String>, String> {
    let managed =
        (strict || prefer_owner) && local_root_daemon(endpoint, security, &c.docker_socket);
    if !managed {
        return docker_flags(endpoint, driver, security, "", limit, 0, false, c);
    }
    let cpu_max = cgroup_cpu_max.unwrap_or("").trim();
    match owner_quota_res
        .and_then(|oq| docker_flags(endpoint, driver, security, cpu_max, limit, oq, true, c))
    {
        Ok(flags) => Ok(flags),
        Err(e) => {
            if prefer_owner && !strict {
                docker_flags(endpoint, driver, security, "", limit, 0, false, c)
            } else {
                Err(e)
            }
        }
    }
}

/// Free disk reading for one mount point (R67).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskReading {
    pub mount: String,
    pub free_gb: u64,
}

/// Host telemetry reading (load, cores, memory, disk, thermal sensor) (R67).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostReading {
    pub load: [f64; 3],
    pub cores: usize,
    pub mem_total_gb: u64,
    pub mem_free_gb: u64,
    pub temp_c: Option<f64>,
    pub disk: Vec<DiskReading>,
}

/// Parse `/proc/meminfo`, disk usage, and `/sys/class/thermal/thermal_zone*/temp` readings into [`HostReading`] (R67).
pub fn parse_reading(
    load: [f64; 3],
    cores: usize,
    meminfo: &str,
    disk_mounts: &[(String, u64)],
    thermal_zones_millideg: &[&str],
) -> HostReading {
    let mut mem_kb: BTreeMap<&str, u64> = BTreeMap::new();
    for line in meminfo.lines() {
        if let Some((k, rest)) = line.split_once(':') {
            if let Some(num_s) = rest.split_whitespace().next() {
                if let Ok(kb) = num_s.parse::<u64>() {
                    mem_kb.insert(k.trim(), kb);
                }
            }
        }
    }
    let mem_total_gb = mem_kb.get("MemTotal").copied().unwrap_or(0) / (1024 * 1024);
    let mem_free_gb = mem_kb.get("MemAvailable").copied().unwrap_or(0) / (1024 * 1024);

    let temp_c = thermal_zones_millideg
        .iter()
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .map(|milli| (milli as f64) / 1000.0)
        .reduce(f64::max);

    let disk = disk_mounts
        .iter()
        .map(|(m, free)| DiskReading {
            mount: m.clone(),
            free_gb: *free,
        })
        .collect();

    HostReading {
        load,
        cores,
        mem_total_gb,
        mem_free_gb,
        temp_c,
        disk,
    }
}

/// Parse a CI container CPU option string `${{ vars.CI_RUNS_ON && '--cpus=N' || '' }}`,
/// failing closed on any unsupported CPU expression (R68, parity cases 83–84).
pub fn parse_ci_cpus(area: &str, options: &str) -> Result<u32, String> {
    if options.matches("${{").count() != 1 {
        return Err(format!(
            "{area}: unsupported self-hosted CPU options {options:?}"
        ));
    }
    let Some(caps) = SELF_HOSTED_CPUS_RE.captures(options) else {
        return Err(format!(
            "{area}: unsupported self-hosted CPU options {options:?}"
        ));
    };
    caps[1]
        .parse::<u32>()
        .map_err(|_| format!("{area}: unsupported self-hosted CPU options {options:?}"))
}

/// Validate and extract Android host processor and Gradle worker caps from CI env (R68, parity case 85).
pub fn parse_android_caps(
    env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let required = [
        ("JAVA_TOOL_OPTIONS", "-XX:ActiveProcessorCount="),
        ("GRADLE_OPTS", "-Dorg.gradle.workers.max="),
    ];
    let mut out = BTreeMap::new();
    for (k, v) in env {
        if let Some((_, prefix)) = required.iter().find(|(rk, _)| rk == k) {
            let pattern = format!(
                r"^\$\{{\{{\s*vars\.CI_RUNS_ON\s*&&\s*'{}(\d+)'\s*\|\|\s*''\s*\}}\}}$",
                regex::escape(prefix)
            );
            let re = Regex::new(&pattern).unwrap();
            let Some(caps) = re.captures(v.trim()) else {
                return Err(format!("android: unsupported {k} resource expression"));
            };
            out.insert(k.clone(), format!("{prefix}{}", &caps[1]));
        } else if !v.contains("${{") {
            out.insert(k.clone(), v.clone());
        }
    }
    for (rk, _) in &required {
        if !out.contains_key(*rk) {
            return Err("android: missing processor or Gradle worker cap".to_string());
        }
    }
    Ok(out)
}

/// Build the container `docker run` argv with `--init` and the signal-trapping driver script
/// that forwards `TERM` (143) and `INT` (130) to child process groups before handback (R68, parity cases 86–87).
#[allow(clippy::too_many_arguments)]
pub fn container_job_argv(
    image: &str,
    name: &str,
    network: &str,
    workdir: &str,
    owner: &str,
    handback_paths: &[String],
    steps: &[String],
    tool: &str,
) -> Vec<String> {
    let handback = handback_paths
        .iter()
        .map(|p| format!("'{}'", p.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let driver = format!(
        "child=\"\"; runner_files=\"\"; finish() {{ status=$?; trap - EXIT; \
         if [ -n \"$runner_files\" ]; then rm -rf -- \"$runner_files\"; fi; \
         chown -R {owner} {handback} || handback=$?; \
         if [ \"$status\" -eq 0 ] && [ \"${{handback:-0}}\" -ne 0 ]; then status=$handback; fi; exit \"$status\"; }}; \
         set -m; interrupt() {{ status=$1; trap - TERM INT; if [ -n \"$child\" ]; then \
         kill -TERM -- \"-$child\" 2>/dev/null || :; wait \"$child\" 2>/dev/null || :; fi; exit \"$status\"; }}; \
         trap finish EXIT; trap 'interrupt 143' TERM; trap 'interrupt 130' INT; \
         for s in \"$@\"; do bash -e -c \"$s\" & child=$!; wait \"$child\"; status=$?; child=\"\"; [ \"$status\" -eq 0 ] || exit \"$status\"; done"
    );
    let mut argv = vec![
        "docker".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "--init".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--network".to_string(),
        network.to_string(),
        "--user".to_string(),
        "root".to_string(),
        "-w".to_string(),
        workdir.to_string(),
        image.to_string(),
        "bash".to_string(),
        "-c".to_string(),
        driver,
        tool.to_string(),
    ];
    argv.extend(steps.iter().cloned());
    argv
}
