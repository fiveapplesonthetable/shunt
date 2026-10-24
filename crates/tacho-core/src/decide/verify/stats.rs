//! CPU slicing & budgets (R35), gate-first wave & sibling stop tracking (R36),
//! per-area stats & summary formatting (R39), cgroup v2 CPU meter (R40),
//! sccache / rustc cache environment & network failure check (R41),
//! LRU target directory shrink plan (R42), warm-cache provisioning & maintenance
//! validators (R43), and resource budget validators (R44).

use super::config::{CiRunnerConfig, StatsPatterns};
use super::result::{AreaResult, RC_STOPPED};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// R35: format a list of CPU indices as a comma-separated string (`"0,1,2"`).
pub fn cpu_list(cpus: &[usize]) -> String {
    cpus.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// R35: partition `total` CPUs across `n` concurrent areas so every area gets a non-empty
/// disjoint slice when `n <= total`, or wraps around `0..total` when `n > total`.
pub fn cpu_sets(total: usize, n: usize) -> Vec<Vec<usize>> {
    if n == 0 || total == 0 {
        return Vec::new();
    }
    let share = (total / n).max(1);
    (0..n)
        .map(|i| {
            let start = i * share;
            if start >= total {
                vec![i % total]
            } else {
                let end = if i == n - 1 {
                    total
                } else {
                    ((i + 1) * share).min(total)
                };
                (start..end).collect()
            }
        })
        .collect()
}

/// R35: compute `(cpus, jobs)` for one area (`gate` runs alone on all CPUs; parallel areas
/// split `total_cpus` evenly with `jobs = cpus.len().min(max_jobs)`).
pub fn area_budget(
    idx: usize,
    parallel_count: usize,
    is_gate: bool,
    total_cpus: usize,
    max_jobs: usize,
) -> (Vec<usize>, usize) {
    let cpus = if is_gate || parallel_count <= 1 {
        (0..total_cpus.max(1)).collect()
    } else {
        let sets = cpu_sets(total_cpus.max(1), parallel_count);
        sets.get(idx).cloned().unwrap_or_else(|| vec![0])
    };
    let jobs = cpus.len().min(max_jobs.max(1)).max(1);
    (cpus, jobs)
}

/// R36: split areas into execution waves (`[gate]` alone first if present, then the rest in parallel).
pub fn execution_waves(areas: &[String], gate: Option<&str>) -> Vec<Vec<String>> {
    if areas.is_empty() {
        return Vec::new();
    }
    if let Some(g) = gate {
        if areas.iter().any(|a| a == g) {
            let rest: Vec<String> = areas.iter().filter(|a| a.as_str() != g).cloned().collect();
            if rest.is_empty() {
                return vec![vec![g.to_string()]];
            }
            return vec![vec![g.to_string()], rest];
        }
    }
    vec![areas.to_vec()]
}

/// R36: pure sibling-stop state tracker for parallel areas (`_Stop`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StopTracker {
    pub triggered: bool,
    pub failed_first: Option<String>,
    pub running: BTreeSet<String>,
    pub killed: Vec<String>,
}

impl StopTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `area` as running. If another area already failed, immediately marks `area` to be
    /// killed and returns `true` (kill immediately).
    pub fn start(&mut self, area: &str) -> bool {
        if self.triggered {
            self.killed.push(area.to_string());
            return true;
        }
        self.running.insert(area.to_string());
        false
    }

    /// Mark `area` finished so a subsequent failure never kills an already-exited area.
    pub fn finish(&mut self, area: &str) {
        self.running.remove(area);
    }

    /// Record that `area` failed. On the first failure, returns the list of currently running
    /// sibling areas to kill (each killed at most once). Subsequent calls return an empty list.
    pub fn fail(&mut self, area: &str) -> Vec<String> {
        self.running.remove(area);
        if self.triggered {
            return Vec::new();
        }
        self.triggered = true;
        self.failed_first = Some(area.to_string());
        let to_kill: Vec<String> = self.running.iter().cloned().collect();
        self.running.clear();
        self.killed.extend(to_kill.iter().cloned());
        to_kill
    }

    /// Effective return code for `area`: `RC_STOPPED` (98) if `area` was stopped by a sibling
    /// failure and exited non-zero, otherwise `raw_rc`.
    pub fn effective_rc(&self, area: &str, raw_rc: i32) -> i32 {
        if raw_rc != 0 && self.killed.iter().any(|a| a == area) {
            RC_STOPPED
        } else {
            raw_rc
        }
    }
}

/// R39: parse `(hits, misses)` from `sccache --show-stats` output.
pub fn sccache_counts(text: &str, pats: &StatsPatterns) -> (u64, u64) {
    let hit_re = Regex::new(&pats.sccache_hits).ok();
    let miss_re = Regex::new(&pats.sccache_misses).ok();
    let hits = hit_re
        .and_then(|re| re.captures(text))
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .unwrap_or(0);
    let misses = miss_re
        .and_then(|re| re.captures(text))
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .unwrap_or(0);
    (hits, misses)
}

/// R39: sum passing test counts across Rust (`test result: ok. N passed`), Vitest (`Tests ... (N)`),
/// and Pytest (`N passed`) output lines in `log`.
pub fn tests_passed(log: &str, pats: &StatsPatterns) -> u64 {
    let mut total = 0u64;
    if let Ok(re) = Regex::new(&pats.rust_passed) {
        for c in re.captures_iter(log) {
            if let Some(n) = c.get(1).and_then(|m| m.as_str().parse::<u64>().ok()) {
                total += n;
            }
        }
    }
    if let Ok(re) = Regex::new(&pats.vitest_passed) {
        for c in re.captures_iter(log) {
            let group = c.get(2).or_else(|| c.get(1));
            if let Some(n) = group.and_then(|m| m.as_str().parse::<u64>().ok()) {
                total += n;
            }
        }
    }
    if let Ok(re) = Regex::new(&pats.pytest_passed) {
        for l in log.lines() {
            if l.contains("test result:") || l.contains("Tests ") {
                continue;
            }
            for c in re.captures_iter(l) {
                if let Some(n) = c.get(1).and_then(|m| m.as_str().parse::<u64>().ok()) {
                    total += n;
                }
            }
        }
    }
    total
}

/// R39: format one area's stats line (`area: ok wall=12s cpu=34.5s sccache=10/12 (83%) tests=42`).
pub fn stats_line(area: &str, s: &AreaResult) -> String {
    let status = if s.rc == 0 {
        "ok".to_string()
    } else if s.rc == RC_STOPPED {
        "stopped".to_string()
    } else {
        format!("FAIL(rc={})", s.rc)
    };
    let total_cache = s.cache_hits + s.cache_misses;
    let cache_part = match (s.cache_hits * 100).checked_div(total_cache) {
        Some(pct) => format!(" sccache={}/{} ({pct}%)", s.cache_hits, total_cache),
        None => String::new(),
    };
    let tests_part = if s.tests_passed > 0 {
        format!(" tests={}", s.tests_passed)
    } else {
        String::new()
    };
    format!(
        "{area}: {status} wall={}s cpu={:.1}s{cache_part}{tests_part}",
        s.wall_s, s.cpu_s
    )
}

/// R39: build the structured summary line (`passed: ...` or `failed: ...` plus per-area stats).
pub fn summary(areas: &[String], results: &BTreeMap<String, AreaResult>) -> String {
    let failed: Vec<String> = areas
        .iter()
        .filter(|a| {
            results
                .get(*a)
                .map(|r| r.rc != 0 && r.rc != RC_STOPPED)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if failed.is_empty() {
        format!("passed: {}", areas.join(" "))
    } else {
        format!("failed: {}", failed.join(" "))
    }
}

/// R40: pure cgroup v2 CPU meter state (`_CpuMeter`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CpuMeterState {
    /// Last reading in microseconds per cgroup path (`cpu.stat` `usage_usec`).
    pub peak_usec: BTreeMap<String, u64>,
}

impl CpuMeterState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse `usage_usec <N>` from cgroup v2 `cpu.stat` text.
    pub fn parse_usage_usec(cpu_stat: &str) -> Option<u64> {
        for line in cpu_stat.lines() {
            if let Some(rest) = line.trim().strip_prefix("usage_usec") {
                if let Ok(v) = rest.trim().parse::<u64>() {
                    return Some(v);
                }
            }
        }
        None
    }

    /// Record a `cpu.stat` text sample for `cgroup_id` (e.g. scope path or container id).
    pub fn record_sample(&mut self, cgroup_id: &str, cpu_stat: &str) {
        if let Some(usec) = Self::parse_usage_usec(cpu_stat) {
            let entry = self.peak_usec.entry(cgroup_id.to_string()).or_insert(0);
            if usec > *entry {
                *entry = usec;
            }
        }
    }

    /// Total CPU time in seconds across all tracked cgroups (`sum(usec) / 1_000_000.0`, rounded to 0.1s).
    pub fn total_seconds(&self) -> f64 {
        let sum_usec: u64 = self.peak_usec.values().sum();
        ((sum_usec as f64) / 100_000.0).round() / 10.0
    }

    /// Parity 164: `docker inspect` command used to resolve container PIDs; always passes
    /// `--type container` before the container name so an image of the same name cannot shadow it.
    pub fn docker_inspect_argv(container_name: &str) -> Vec<String> {
        vec![
            "docker".to_string(),
            "inspect".to_string(),
            "--type".to_string(),
            "container".to_string(),
            "--format".to_string(),
            "{{.State.Pid}}".to_string(),
            container_name.to_string(),
        ]
    }
}

/// R41: compute the `sccache` + `CARGO_INCREMENTAL=0` environment variables for a warm build.
/// Never sets `CARGO_BUILD_JOBS` so Cargo's jobserver is not overridden per sub-invocation.
pub fn rust_cache_env(
    cache_root: &str,
    area: &str,
    sccache_available: bool,
    cfg: &CiRunnerConfig,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let root = if cache_root.is_empty() {
        &cfg.cache_root
    } else {
        cache_root
    };
    env.insert("CARGO_HOME".to_string(), format!("{root}/cargo"));
    env.insert(
        "CARGO_TARGET_DIR".to_string(),
        format!("{root}/target/{area}"),
    );
    env.insert("CARGO_INCREMENTAL".to_string(), "0".to_string());
    if sccache_available {
        env.insert("RUSTC_WRAPPER".to_string(), "sccache".to_string());
        env.insert("SCCACHE_DIR".to_string(), format!("{root}/sccache"));
        env.insert("SCCACHE_CACHE_SIZE".to_string(), cfg.sccache_size.clone());
    }
    env
}

/// R41: detect whether a failed step log is a transient network/registry outage eligible for 1 retry.
pub fn is_network_failure(log: &str, pats: &super::config::LogPatterns) -> bool {
    pats.network.iter().any(|p| match Regex::new(p) {
        Ok(re) => re.is_match(log),
        Err(_) => log.contains(p.as_str()),
    })
}

/// R42: target subdirectory entry for LRU shrinking (`_shrink`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaDirEntry {
    pub name: String,
    pub bytes: u64,
    pub mtime: i64,
}

/// R42: given target subdirectories (`target/<area>`) and `max_bytes`, return the ordered list
/// of directory names to delete (oldest `mtime` first, skipping any currently `active` areas)
/// until total bytes `<= max_bytes`.
pub fn shrink_plan(
    entries: &[AreaDirEntry],
    active: &BTreeSet<String>,
    max_bytes: u64,
) -> Vec<String> {
    let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
    if total <= max_bytes {
        return Vec::new();
    }
    let mut candidates: Vec<&AreaDirEntry> = entries
        .iter()
        .filter(|e| !active.contains(&e.name))
        .collect();
    candidates.sort_by_key(|e| (e.mtime, e.name.clone()));
    let mut to_remove = Vec::new();
    for c in candidates {
        if total <= max_bytes {
            break;
        }
        to_remove.push(c.name.clone());
        total = total.saturating_sub(c.bytes);
    }
    to_remove
}

/// R43: validate `REPO_ID` against `^[A-Za-z0-9._-]+$` (rejecting empty, `/`, `..`, spaces).
pub fn validate_repo_id(repo_id: &str) -> Result<&str, String> {
    if repo_id.is_empty() || repo_id == "." || repo_id == ".." {
        return Err(format!("invalid REPO_ID '{repo_id}': must be non-empty"));
    }
    let ok = repo_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Err(format!(
            "invalid REPO_ID '{repo_id}': must match ^[A-Za-z0-9._-]+$"
        ));
    }
    Ok(repo_id)
}

/// R43: verify that the warm cache root has a writable sentinel and all required subdirectories
/// (`cargo`, `sccache`, `npm`, `pip`, `playwright`, `gradle`, `android`, `target`).
pub fn verify_cache_root(
    sentinel_ok: bool,
    present_subdirs: &BTreeSet<String>,
    cfg: &CiRunnerConfig,
) -> Result<(), String> {
    if !sentinel_ok {
        return Err("warm cache sentinel check failed (root not writable)".to_string());
    }
    for req in &cfg.cache_subdirs {
        if !present_subdirs.contains(req) {
            return Err(format!("missing warm cache subdirectory '{req}'"));
        }
    }
    Ok(())
}

/// R44: parse a `cpuset.cpus` / `AllowedCPUs` specification (`"0-3,6"` -> `{0, 1, 2, 3, 6}`).
pub fn parse_cpu_spec(spec: &str) -> Result<BTreeSet<usize>, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("empty CPU specification".to_string());
    }
    let mut out = BTreeSet::new();
    for part in trimmed.split(',') {
        let p = part.trim();
        if let Some((a, b)) = p.split_once('-') {
            let start = a
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("invalid CPU range start '{a}'"))?;
            let end = b
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("invalid CPU range end '{b}'"))?;
            if start > end {
                return Err(format!("inverted CPU range '{start}-{end}'"));
            }
            for c in start..=end {
                out.insert(c);
            }
        } else {
            let c = p
                .parse::<usize>()
                .map_err(|_| format!("invalid CPU index '{p}'"))?;
            out.insert(c);
        }
    }
    Ok(out)
}

/// R44: validate a container or systemd slice resource budget against host capacity (`nproc`).
pub fn validate_resource_budget(
    cpu_spec: &str,
    memory_max: &str,
    pids_max: u64,
    host_cpus: usize,
) -> Result<BTreeSet<usize>, String> {
    let cpus = parse_cpu_spec(cpu_spec)?;
    if let Some(&max_cpu) = cpus.iter().max() {
        if max_cpu >= host_cpus {
            return Err(format!(
                "CPU index {max_cpu} in '{cpu_spec}' exceeds host CPU count {host_cpus}"
            ));
        }
    }
    if memory_max.trim().is_empty() || memory_max.trim() == "0" {
        return Err(format!("invalid memory limit '{memory_max}'"));
    }
    if pids_max < 64 {
        return Err(format!("pids.max {pids_max} is too small (minimum 64)"));
    }
    Ok(cpus)
}
