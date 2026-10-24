//! Legacy CPU burner, duplicate batch, starve, tidy, watchdog, and cost helpers
//! preserved for backward compatibility with existing callers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::decide::dispatch::{base_key, AgentRun};
use crate::decide::verify::BatchRun;

use super::legacy::{
    CostRecord, CostSummary, ProcEntry, StarvePlan, TidyDecision, TidySnapshot, BURNERS_PER_CPU,
    BURNER_MIN_AGE_SECS, STARVE_CPU_DIVISOR,
};

fn basename(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

/// Extract a CPU burner binary name (`yes`, `stress`, `stress-ng`) from `comm` or `args`,
/// looking through `timeout` wrappers (e.g. `timeout -k 5 30 yes`).
pub fn burner_command(comm: &str, args: &str) -> Option<String> {
    let base_comm = basename(comm);
    if matches!(base_comm, "yes" | "stress" | "stress-ng") {
        return Some(base_comm.to_string());
    }
    let parts: Vec<&str> = args.split_whitespace().collect();
    let first = parts.first().copied().map(basename).unwrap_or(base_comm);
    if matches!(first, "yes" | "stress" | "stress-ng") {
        return Some(first.to_string());
    }
    if first != "timeout" {
        return None;
    }
    let mut i = 1;
    while i < parts.len() {
        let arg = parts[i];
        if matches!(arg, "-k" | "-s" | "--kill-after" | "--signal") {
            i += 2;
        } else if arg.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let cmd = basename(parts.get(i + 1).copied()?);
    if matches!(cmd, "yes" | "stress" | "stress-ng") {
        Some(cmd.to_string())
    } else {
        None
    }
}

/// Find runaway CPU burner processes (`yes`, `stress`, `stress-ng`, including under `timeout`)
/// running `>= BURNER_MIN_AGE_SECS` that were NOT launched by `tacho starve` (`env_starve == false`).
pub fn burners(procs: &[ProcEntry]) -> Vec<ProcEntry> {
    procs
        .iter()
        .filter(|p| {
            !p.env_starve
                && p.elapsed_secs >= BURNER_MIN_AGE_SECS
                && burner_command(&p.comm, &p.args).is_some()
        })
        .cloned()
        .collect()
}

/// Find duplicate running batches covering the same `group` or identical PR set so all but the oldest can be terminated.
pub fn duplicate_batches(batches: &[BatchRun]) -> Vec<String> {
    let mut by_key: BTreeMap<String, Vec<&BatchRun>> = BTreeMap::new();
    for b in batches {
        if b.status == "running" || b.status == "bisecting" {
            let mut sorted_prs = b.prs.clone();
            sorted_prs.sort_unstable();
            let key = if !b.group.is_empty() {
                format!("g:{}:prs:{:?}", b.group, sorted_prs)
            } else {
                format!("prs:{:?}", sorted_prs)
            };
            by_key.entry(key).or_default().push(b);
        }
    }
    let mut kill_ids = Vec::new();
    for (_, mut list) in by_key {
        if list.len() > 1 {
            list.sort_by_key(|b| (b.started_at, b.id.as_str()));
            for b in &list[1..] {
                kill_ids.push(b.id.clone());
            }
        }
    }
    kill_ids
}

/// Identify runs blocked only because they exited 0 without stating `PR: none` when their worktree
/// actually has 0 commits ahead and 0 dirty files (`idle_chains`), so supervision can auto-mark `pr_none = true`.
pub fn idle_chains(
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    live_keys: &BTreeSet<String>,
    worktree_clean_zero_commits: &HashMap<String, bool>,
) -> Vec<String> {
    let mut out = Vec::new();
    for a in agents.values() {
        if retired.contains(&a.key) || live_keys.contains(&a.key) {
            continue;
        }
        if a.finished_at.is_some()
            && a.exit_code == Some(0)
            && a.pr.is_none()
            && !a.pr_none
            && worktree_clean_zero_commits.get(&a.key) == Some(&true)
        {
            out.push(a.key.clone());
        }
    }
    out
}

/// Format a sorted slice of CPU indices into compact `taskset -c` range notation
/// (e.g. `&[0, 1, 2, 3, 8]` -> `"0-3,8"`).
pub fn format_cpu_set(cpus: &[usize]) -> String {
    if cpus.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<usize> = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    super::starve::format_cpu_ids(&sorted)
}

/// Plan a bounded CPU starvation slice for `tacho starve`.
pub fn plan_starve(
    total_cpus: usize,
    requested_slice: Option<usize>,
    requested_cpu_set: Option<&str>,
    batch_running: bool,
    timeout_secs: u64,
) -> Result<StarvePlan, String> {
    if batch_running {
        return Err("cannot run `tacho starve` while a verification batch is running".to_string());
    }
    if total_cpus == 0 {
        return Err("total_cpus must be >= 1".to_string());
    }
    let max_slice = (total_cpus / STARVE_CPU_DIVISOR).max(1);

    let cpus: Vec<usize> = if let Some(spec) = requested_cpu_set {
        let mut parsed = BTreeSet::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(format!("empty element in CPU set '{spec}'"));
            }
            if let Some((a, b)) = part.split_once('-') {
                let start: usize = a
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid CPU range '{part}'"))?;
                let end: usize = b
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid CPU range '{part}'"))?;
                if start > end || end >= total_cpus {
                    return Err(format!(
                        "CPU range '{part}' out of bounds for {total_cpus} CPUs"
                    ));
                }
                for c in start..=end {
                    parsed.insert(c);
                }
            } else {
                let c: usize = part
                    .parse()
                    .map_err(|_| format!("invalid CPU id '{part}'"))?;
                if c >= total_cpus {
                    return Err(format!("CPU {c} out of bounds for {total_cpus} CPUs"));
                }
                parsed.insert(c);
            }
        }
        if parsed.is_empty() {
            return Err("empty CPU set".to_string());
        }
        if parsed.len() > max_slice {
            return Err(format!(
                "requested {} CPUs exceeds max starve slice of {} (1/{STARVE_CPU_DIVISOR} of {total_cpus} CPUs)",
                parsed.len(),
                max_slice
            ));
        }
        parsed.into_iter().collect()
    } else {
        let count = requested_slice.unwrap_or(max_slice).clamp(1, max_slice);
        if requested_slice.is_some_and(|req| req > max_slice) {
            let req = requested_slice.unwrap_or(0);
            return Err(format!(
                "requested slice {req} exceeds max starve slice of {max_slice} (1/{STARVE_CPU_DIVISOR} of {total_cpus} CPUs)"
            ));
        }
        ((total_cpus - count)..total_cpus).collect()
    };

    let cpu_list = format_cpu_set(&cpus);
    let burners = cpus.len() * BURNERS_PER_CPU;
    Ok(StarvePlan {
        cpus,
        cpu_list,
        burners,
        timeout_secs: timeout_secs.clamp(1, 300),
    })
}

/// Decide which stale worktrees, checkouts, containers, and networks to clean up (`tacho tidy`).
pub fn decide_tidy(snap: &TidySnapshot, min_age_secs: i64) -> Vec<TidyDecision> {
    let mut out = Vec::new();

    for wt in &snap.worktrees {
        if snap.live_agent_worktrees.contains(&wt.path)
            || snap.live_batch_worktrees.contains(&wt.path)
            || snap.held_paths.contains(&wt.path)
        {
            continue;
        }
        if wt.age_secs < min_age_secs && !wt.pr_merged_or_closed {
            continue;
        }
        let rescue = wt.dirty || wt.unpushed_commits > 0;
        let because = if wt.pr_merged_or_closed {
            format!("PR for branch '{}' is merged or closed", wt.branch)
        } else {
            format!("worktree idle for {}s >= {}s", wt.age_secs, min_age_secs)
        };
        out.push(TidyDecision::RemoveTree {
            path: wt.path.clone(),
            rescue,
            because,
        });
    }

    for co in &snap.checkouts {
        if snap.live_batch_worktrees.contains(&co.path) || snap.held_paths.contains(&co.path) {
            continue;
        }
        if co.age_secs < min_age_secs {
            continue;
        }
        let rescue = co.dirty || co.unpushed_commits > 0;
        out.push(TidyDecision::RemoveCheckout {
            path: co.path.clone(),
            rescue,
            because: format!(
                "batch checkout idle for {}s >= {}s",
                co.age_secs, min_age_secs
            ),
        });
    }

    for c in &snap.containers {
        if c.age_secs < min_age_secs {
            continue;
        }
        if c.ports.iter().any(|p| snap.held_ports.contains(p)) {
            continue;
        }
        out.push(TidyDecision::RemoveContainer {
            id: c.id.clone(),
            because: format!(
                "container '{}' age {}s >= {}s with no held ports",
                c.name, c.age_secs, min_age_secs
            ),
        });
    }

    for net in &snap.networks {
        out.push(TidyDecision::RemoveNetwork {
            name: net.clone(),
            because: "unused ephemeral network".to_string(),
        });
    }

    out
}

/// Find finished runs whose PID is no longer alive but whose `finished_at` was never stamped.
pub fn stale_runs(agents: &BTreeMap<String, AgentRun>, live_pids: &BTreeSet<u32>) -> Vec<String> {
    let mut out = Vec::new();
    for a in agents.values() {
        if a.finished_at.is_none() {
            match a.pid {
                Some(pid) if !live_pids.contains(&pid) => out.push(a.key.clone()),
                None => out.push(a.key.clone()),
                _ => {}
            }
        }
    }
    out
}

/// Format a summary of zombie (`Z`) processes grouped by parent PID.
pub fn zombie_lines(procs: &[ProcEntry]) -> Vec<String> {
    let mut by_ppid: BTreeMap<u32, usize> = BTreeMap::new();
    for p in procs {
        if p.state.starts_with('Z') {
            *by_ppid.entry(p.ppid).or_default() += 1;
        }
    }
    by_ppid
        .into_iter()
        .filter(|&(_, count)| count >= 5)
        .map(|(ppid, count)| format!("parent PID {ppid} has {count} zombie children"))
        .collect()
}

/// Check whether the orchestrator tick loop appears down (`last_tick_ts` older than `max_stale_secs`
/// while work is active or queued).
pub fn down_reason(
    last_tick_ts: Option<i64>,
    now: i64,
    max_stale_secs: i64,
    has_active_work: bool,
) -> Option<String> {
    if !has_active_work {
        return None;
    }
    let Some(ts) = last_tick_ts else {
        return Some("orchestrator has never recorded a tick while work is active".to_string());
    };
    let age = (now - ts).max(0);
    if age > max_stale_secs {
        Some(format!(
            "orchestrator tick is stale by {age}s (threshold {max_stale_secs}s)"
        ))
    } else {
        None
    }
}

/// Decide whether the watchdog should send an alert (rate-limited by `cooldown_secs`).
pub fn down_alert(
    reason: Option<&str>,
    last_alert_ts: Option<i64>,
    now: i64,
    cooldown_secs: i64,
) -> Option<String> {
    let r = reason?;
    if last_alert_ts.is_some_and(|prev| (now - prev) < cooldown_secs) {
        return None;
    }
    Some(r.to_string())
}

/// Estimate USD cost for token usage of a model (per 1M tokens).
pub fn price(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> f64 {
    let m = model.to_ascii_lowercase();
    let (in_rate, out_rate) = if m.contains("opus") {
        (15.0, 75.0)
    } else if m.contains("sonnet") || m.contains("gemini-3.1-pro") || m.contains("gemini-pro") {
        (3.0, 15.0)
    } else if m.contains("haiku") || m.contains("mini") || m.contains("flash") {
        (0.8, 4.0)
    } else if m.contains("gpt-5") || m.contains("codex") || m.contains("o1") || m.contains("o3") {
        (2.5, 10.0)
    } else {
        return 0.0;
    };
    let cache_read_rate = in_rate * 0.1;
    let cache_write_rate = in_rate * 2.0;
    (input_tokens as f64 * in_rate
        + output_tokens as f64 * out_rate
        + cache_read_tokens as f64 * cache_read_rate
        + cache_write_tokens as f64 * cache_write_rate)
        / 1_000_000.0
}

/// Summarize token and USD costs across a set of [`CostRecord`]s.
pub fn summarize_costs(records: &[CostRecord], agents: &BTreeMap<String, AgentRun>) -> CostSummary {
    let mut out = CostSummary::default();
    let mut seen_keys = HashSet::new();
    for r in records {
        if !seen_keys.insert(r.key.clone()) {
            continue;
        }
        out.runs += 1;
        out.total_usd += r.usd;
        out.total_input_tokens += r.input_tokens;
        out.total_output_tokens += r.output_tokens;
        *out.by_provider.entry(r.provider.clone()).or_default() += r.usd;
        let area = agents
            .get(&r.key)
            .or_else(|| agents.get(base_key(&r.key)))
            .map(|a| {
                if a.area.is_empty() {
                    "unscoped".to_string()
                } else {
                    a.area.clone()
                }
            })
            .unwrap_or_else(|| "unscoped".to_string());
        *out.by_area.entry(area).or_default() += r.usd;
    }
    out
}
