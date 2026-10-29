//! Pure status snapshot construction (`VERSION = 1`), section slicing, attention & needs-you
//! prioritization, main/deploy status block, process owner classification, cost summary,
//! and recursive secret redaction.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::selfupdate::{self, SelfUpdatePolicy};
use super::OpsConfig;

pub const VERSION: u32 = 1;

pub const SECTIONS: &[&str] = &[
    "headline",
    "needs_you",
    "attention",
    "main",
    "agents",
    "pipeline",
    "held",
    "flakes",
    "machine",
    "cost",
    "quotas",
    "updated_at",
];

/// Extracts literal secret values (`>= 4` chars) from configured environment/config variables
/// whose names contain `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `KEY`, `WEBHOOK`, `AUTH`,
/// `CREDENTIAL`, or `PRIVATE` (excluding path/port/issue metadata keys unless `WEBHOOK` is present).
pub fn configured_secrets(env_map: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in env_map {
        let trimmed = v.trim();
        if trimmed.chars().count() < 4 {
            continue;
        }
        let up = k.to_ascii_uppercase();
        let has_secret_word = [
            "TOKEN",
            "SECRET",
            "PASSWORD",
            "PASSWD",
            "KEY",
            "WEBHOOK",
            "AUTH",
            "CREDENTIAL",
            "PRIVATE",
        ]
        .iter()
        .any(|w| up.contains(w));
        if !has_secret_word {
            continue;
        }
        let is_non_secret_suffix = [
            "_FILE", "_PATH", "_DIR", "_ISSUE", "_PORT", "_MODE", "_CMD", "_BIN",
        ]
        .iter()
        .any(|suf| up.ends_with(suf))
            || (up.ends_with("_URL") && !up.contains("WEBHOOK"));
        if is_non_secret_suffix {
            continue;
        }
        out.push(trimmed.to_string());
    }
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out.dedup();
    out
}

/// Scrubs secret patterns (`crate::scrub::Scrubber`) and replaces any literal of length >= 4
/// with `[REDACTED]` (R52).
pub fn redact_text(value: &str, literals: &[String]) -> String {
    let mut out = crate::scrub::Scrubber::enabled().scrub(value);
    for lit in literals {
        if lit.chars().count() >= 4 {
            out = out.replace(lit.as_str(), "[REDACTED]");
        }
    }
    out
}

/// Recursively scrubs string values in a `serde_json::Value` while preserving object keys,
/// numbers, booleans, and nulls (R52).
pub fn redact_json(value: serde_json::Value, literals: &[String]) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(redact_text(&s, literals)),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(|v| redact_json(v, literals)).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, redact_json(v, literals)))
                .collect(),
        ),
        other => other,
    }
}

/// Resource owner bucket (R49, SN23–SN29).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Owner {
    CiRunners,
    Batches,
    Agents,
    Burners,
    Orchestrator,
    Other,
    Unknown,
}

impl Owner {
    pub const ALL: [Owner; 7] = [
        Owner::CiRunners,
        Owner::Batches,
        Owner::Agents,
        Owner::Burners,
        Owner::Orchestrator,
        Owner::Other,
        Owner::Unknown,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Owner::CiRunners => "ci_runners",
            Owner::Batches => "batches",
            Owner::Agents => "agents",
            Owner::Burners => "burners",
            Owner::Orchestrator => "orchestrator",
            Owner::Other => "other",
            Owner::Unknown => "unknown",
        }
    }
}

/// Configurable process-owner matching rules (R49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRules {
    pub batch_path_marker: String,
    pub worktree_marker: String,
    pub orchestrator_markers: Vec<String>,
    pub burner_comm: Vec<String>,
    pub agent_comm: Vec<String>,
}

impl Default for OwnerRules {
    fn default() -> Self {
        Self {
            batch_path_marker: "/batches/".into(),
            worktree_marker: "/worktrees/".into(),
            orchestrator_markers: vec!["tacho".into(), "fleet".into()],
            burner_comm: vec![
                "cargo".into(),
                "rustc".into(),
                "cc1".into(),
                "cc1plus".into(),
                "ld".into(),
                "lld".into(),
                "sccache".into(),
                "nextest".into(),
                "node".into(),
                "npm".into(),
                "java".into(),
                "gradle".into(),
                "postgres".into(),
            ],
            agent_comm: vec!["claude".into(), "codex".into()],
        }
    }
}

/// A single observed process from `/proc` (R49).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProcObs {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub cmdline: String,
    pub cwd: String,
    pub cgroup: String,
    pub rss_bytes: u64,
    pub cpu_seconds: f64,
}

/// Aggregated resource usage for one [`Owner`] bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OwnerBucket {
    pub count: usize,
    pub rss_gb: f64,
    pub cpu_seconds: f64,
    pub cpu_cores: Option<f64>,
}

/// Classifies a single process given its ancestor chain (SN23–SN27).
pub fn classify_proc(proc: &ProcObs, ancestors: &[ProcObs], rules: &OwnerRules) -> Owner {
    let chain = std::iter::once(proc).chain(ancestors.iter());
    for p in chain.clone() {
        if p.cgroup.contains("actions.runner.")
            || p.cmdline.contains("Runner.Listener")
            || p.cmdline.contains("Runner.Worker")
            || p.cwd.contains("actions-runner")
        {
            return Owner::CiRunners;
        }
    }
    for p in chain.clone() {
        if p.cwd.contains(&rules.batch_path_marker)
            || p.cmdline.contains(&rules.batch_path_marker)
            || p.cgroup.contains("tacho-b-")
        {
            return Owner::Batches;
        }
    }
    let in_agent_tree = chain
        .clone()
        .any(|p| rules.agent_comm.contains(&p.comm) || p.cwd.contains(&rules.worktree_marker));
    let is_burner = rules
        .burner_comm
        .iter()
        .any(|c| proc.comm == *c || proc.comm.starts_with(c.as_str()));
    if is_burner && !in_agent_tree {
        return Owner::Burners;
    }
    if in_agent_tree {
        return Owner::Agents;
    }
    for p in chain {
        if rules
            .orchestrator_markers
            .iter()
            .any(|m| p.cmdline.contains(m.as_str()) || p.comm == *m)
        {
            return Owner::Orchestrator;
        }
    }
    if proc.comm.is_empty() && proc.cmdline.is_empty() {
        Owner::Unknown
    } else {
        Owner::Other
    }
}

/// Classifies all processes, aggregates RSS and CPU seconds per bucket, and merges any residual
/// `mem_used_gb - sum(rss_gb)` into `unknown` (SN23–SN29).
pub fn classify_owners(
    procs: &[ProcObs],
    mem_used_gb: f64,
    prev_sample: Option<&(i64, BTreeMap<String, f64>)>,
    now: i64,
    nproc: usize,
    rules: &OwnerRules,
) -> (BTreeMap<String, OwnerBucket>, BTreeMap<String, f64>) {
    let by_pid: BTreeMap<u32, &ProcObs> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut out: BTreeMap<String, OwnerBucket> = Owner::ALL
        .into_iter()
        .map(|o| (o.key().to_string(), OwnerBucket::default()))
        .collect();

    for p in procs {
        let mut ancestors = Vec::new();
        let (mut cur, mut guard) = (p.ppid, 0);
        while cur > 1 && guard < 32 {
            guard += 1;
            if let Some(&parent) = by_pid.get(&cur) {
                ancestors.push(parent.clone());
                cur = parent.ppid;
            } else {
                break;
            }
        }
        let owner = classify_proc(p, &ancestors, rules);
        let entry = out.entry(owner.key().to_string()).or_default();
        entry.count += 1;
        entry.rss_gb += (p.rss_bytes as f64) / 1_073_741_824.0;
        entry.cpu_seconds += p.cpu_seconds;
    }

    let sum_rss: f64 = out.values().map(|b| b.rss_gb).sum();
    if mem_used_gb > sum_rss {
        if let Some(unk) = out.get_mut(Owner::Unknown.key()) {
            unk.rss_gb += mem_used_gb - sum_rss;
        }
    }

    let cur_secs: BTreeMap<String, f64> = out
        .iter()
        .map(|(k, b)| (k.clone(), b.cpu_seconds))
        .collect();
    if let Some(core_rates) = rates(prev_sample, now, &cur_secs, nproc) {
        for (k, b) in &mut out {
            b.cpu_cores = core_rates.get(k).copied();
        }
    }
    for b in out.values_mut() {
        b.rss_gb = (b.rss_gb * 100.0).round() / 100.0;
    }
    (out, cur_secs)
}

/// Computes CPU core rates from two `(timestamp, cpu_seconds)` samples if `1 <= dt <= 600` (SN28).
pub fn rates(
    prev: Option<&(i64, BTreeMap<String, f64>)>,
    now: i64,
    cur_secs: &BTreeMap<String, f64>,
    nproc: usize,
) -> Option<BTreeMap<String, f64>> {
    let (prev_ts, prev_map) = prev?;
    let dt = now - *prev_ts;
    if !(1..=600).contains(&dt) {
        return None;
    }
    let cap = (nproc.max(1)) as f64;
    let mut out = BTreeMap::new();
    for (k, &cur) in cur_secs {
        let old = prev_map.get(k).copied().unwrap_or(0.0);
        let delta = (cur - old).max(0.0) / (dt as f64);
        out.insert(k.clone(), (delta.min(cap) * 100.0).round() / 100.0);
    }
    Some(out)
}

/// Token counts for a single agent run (R50).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

/// Prices a run in USD from its model name and token counts (R50).
pub fn price(model: &str, u: &TokenUsage) -> f64 {
    let m = model.to_ascii_lowercase();
    let (inp, cw, cr, out) = if m.contains("opus") {
        (15.0, 18.75, 1.50, 75.0)
    } else if m.contains("sonnet") {
        (3.0, 3.75, 0.30, 15.0)
    } else if m.contains("haiku") {
        (0.80, 1.0, 0.08, 4.0)
    } else if m.contains("gpt-5") || m.contains("codex") {
        (1.75, 0.0, 0.175, 14.0)
    } else {
        (3.0, 3.75, 0.30, 15.0)
    };
    let raw = (u.input as f64 * inp
        + u.cache_write as f64 * cw
        + u.cache_read as f64 * cr
        + u.output as f64 * out)
        / 1_000_000.0;
    (raw * 10000.0).round() / 10000.0
}

/// A recorded agent cost entry (R50).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CostRecord {
    pub ts: i64,
    pub kind: String,
    pub key: String,
    pub model: String,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub turns: u32,
    pub usage: TokenUsage,
    pub session_id: String,
}

/// Deduplicates cost records by `(key, ts, cost_usd_bits)` (R50).
pub fn once(records: &[CostRecord]) -> Vec<CostRecord> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for r in records {
        if seen.insert((r.key.clone(), r.ts, (r.cost_usd * 10000.0).round() as i64)) {
            out.push(r.clone());
        }
    }
    out
}

/// Aggregated cost block for the snapshot (R50).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CostSummary {
    pub today_usd: f64,
    pub today_runs: usize,
    pub last_24h_usd: f64,
    pub last_24h_runs: usize,
    pub by_kind: BTreeMap<String, f64>,
    pub by_issue: BTreeMap<String, f64>,
}

/// Summarizes deduplicated cost records for today and the last 24 hours (R50).
pub fn cost_summary(records: &[CostRecord], now: i64, start_of_day_ts: i64) -> CostSummary {
    let dedup = once(records);
    let cutoff_24h = now - 86400;
    let mut out = CostSummary::default();
    for r in &dedup {
        if r.ts >= start_of_day_ts {
            out.today_usd += r.cost_usd;
            out.today_runs += 1;
            *out.by_kind.entry(r.kind.clone()).or_default() += r.cost_usd;
            *out.by_issue.entry(r.key.clone()).or_default() += r.cost_usd;
        }
        if r.ts >= cutoff_24h {
            out.last_24h_usd += r.cost_usd;
            out.last_24h_runs += 1;
        }
    }
    out.today_usd = (out.today_usd * 100.0).round() / 100.0;
    out.last_24h_usd = (out.last_24h_usd * 100.0).round() / 100.0;
    out
}

/// Current and last completed CI + deploy runs on `main` (R48, SN16–SN20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MainCiRun {
    pub id: u64,
    pub status: String,
    pub conclusion: String,
    pub sha: String,
    pub title: String,
    pub age_seconds: Option<i64>,
    /// When the run completed (forge timestamp, e.g. RFC 3339); empty when unknown.
    #[serde(default)]
    pub completed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MainStatusBlock {
    pub status: String,
    pub current: Option<MainCiRun>,
    pub last_completed: Option<MainCiRun>,
    pub deploy: Option<MainCiRun>,
}

/// Builds the `main` status block (R48, SN16–SN20).
pub fn build_main_block(
    current: Option<MainCiRun>,
    last_completed: Option<MainCiRun>,
    deploy: Option<MainCiRun>,
) -> MainStatusBlock {
    let status = match (&current, &last_completed) {
        (Some(_), None) => "running".into(),
        (_, Some(done)) if done.conclusion == "success" => "passing".into(),
        (_, Some(done)) if !done.conclusion.is_empty() => done.conclusion.clone(),
        _ => "unknown".into(),
    };
    MainStatusBlock {
        status,
        current,
        last_completed,
        deploy,
    }
}

/// Configurable thresholds for `needs_you` and `attention` (R45–R47, SN1–SN15, RT1–RT2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRules {
    pub stuck_agent_secs: i64,
    pub quiet_tick_secs: i64,
    pub ready_unmerged_secs: i64,
    pub max_retries: u32,
    pub attention_cap: usize,
    pub selfupdate: SelfUpdatePolicy,
}

impl Default for AttentionRules {
    fn default() -> Self {
        Self {
            stuck_agent_secs: 2700,
            quiet_tick_secs: 600,
            ready_unmerged_secs: 1800,
            max_retries: 2,
            attention_cap: 5,
            selfupdate: SelfUpdatePolicy::default(),
        }
    }
}

/// Inputs used to compute `needs_you`, `attention`, and the full [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SnapshotObs {
    pub now: i64,
    pub last_tick_age: Option<i64>,
    pub paused: bool,
    pub pause_reason: String,
    pub main_block: MainStatusBlock,
    pub pending_fixes: BTreeMap<String, String>,
    pub ready_prs: Vec<(u64, String, i64)>,
    pub held_prs: BTreeMap<u64, String>,
    pub held_replays: BTreeMap<String, String>,
    pub blocked_issues: BTreeMap<u64, String>,
    pub needs_human_issues: Vec<(u64, String)>,
    pub retried: BTreeMap<String, u32>,
    pub open_prs: Vec<u64>,
    pub open_issues: Vec<u64>,
    pub running_agents: Vec<(String, String, i64)>,
    pub running_batches: Vec<(String, String, i64, Option<i64>)>,
    pub flakes: Vec<String>,
    pub selfupdate_behind: Vec<selfupdate::Behind>,
    pub selfupdate_failed: Option<String>,
    pub machine: serde_json::Value,
    pub cost: CostSummary,
    pub quotas: Option<serde_json::Value>,
    pub redact_literals: Vec<String>,
}

/// Items requiring human decision (`needs_you`) (R45, R46, SN1, SN2).
pub fn needs_you(obs: &SnapshotObs) -> Vec<String> {
    let mut out = Vec::new();
    for (pr, title, _) in &obs.ready_prs {
        out.push(format!("PR #{pr} ready to merge: {title}"));
    }
    for (iss, title) in &obs.needs_human_issues {
        if title.is_empty() {
            out.push(format!("issue #{iss} labeled needs-human"));
        } else {
            out.push(format!("issue #{iss} needs human: {title}"));
        }
    }
    out
}

/// Ranked operational warnings (`attention`) before 5-item snapshot truncation (R47, SN3–SN15, RT1–RT2).
pub fn needs_attention(obs: &SnapshotObs, rules: &AttentionRules, tool: &str) -> Vec<String> {
    let mut items = Vec::new();
    let has_main_fix = obs.pending_fixes.keys().any(|k| k.starts_with("main"));
    let has_deploy_fix = obs.pending_fixes.keys().any(|k| k.starts_with("deploy"));

    if obs.main_block.status == "failure" && !has_main_fix {
        let title = obs
            .main_block
            .last_completed
            .as_ref()
            .map(|r| r.title.as_str())
            .unwrap_or("CI failed");
        items.push(format!("main is broken ({title}) and no fix is running"));
    }
    if let Some(dep) = &obs.main_block.deploy {
        if dep.conclusion == "failure" && !has_deploy_fix {
            items.push(format!(
                "deploy #{} failed ({}) and no fix is running",
                dep.id, dep.title
            ));
        }
    }
    for (id, phase, _, stale_secs) in &obs.running_batches {
        if phase == "dead" {
            items.push(format!("batch {id} died unexpectedly"));
        } else if let Some(s) = stale_secs.filter(|&s| s >= 1800) {
            items.push(format!("batch {id} stale in phase {phase} for {}m", s / 60));
        }
    }
    for (key, kind, elapsed) in &obs.running_agents {
        if *elapsed >= rules.stuck_agent_secs {
            items.push(format!(
                "agent {key} ({kind}) running for {}m",
                elapsed / 60
            ));
        }
    }
    if let Some(err) = &obs.selfupdate_failed {
        items.push(format!(
            "{tool} auto-update failed ({err}) — run `{tool} update`"
        ));
    } else if let Some(line) =
        selfupdate::attention(&obs.selfupdate_behind, obs.now, tool, &rules.selfupdate)
    {
        items.push(line);
    }
    if let Some(age) = obs.last_tick_age.filter(|&a| a > rules.quiet_tick_secs) {
        items.push(format!("tick loop quiet for {}m", age / 60));
    }
    let open_pr_set: BTreeSet<u64> = obs.open_prs.iter().copied().collect();
    let open_issue_set: BTreeSet<u64> = obs.open_issues.iter().copied().collect();
    let active_keys: BTreeSet<&str> = obs
        .running_agents
        .iter()
        .map(|(k, _, _)| k.as_str())
        .collect();

    for (&pr, reason) in &obs.held_prs {
        if open_pr_set.is_empty() || open_pr_set.contains(&pr) {
            items.push(format!("PR #{pr} held: {reason}"));
        }
    }
    for (key, reason) in &obs.held_replays {
        items.push(format!("replay {key} held: {reason}"));
    }
    for (&iss, reason) in &obs.blocked_issues {
        if open_issue_set.is_empty() || open_issue_set.contains(&iss) {
            items.push(format!("issue #{iss} blocked: {reason}"));
        }
    }
    for (key, &count) in &obs.retried {
        if count >= rules.max_retries && !active_keys.contains(key.as_str()) {
            let num_opt = key
                .trim_start_matches("pr-")
                .trim_start_matches("issue-")
                .parse::<u64>()
                .ok();
            let still_open = match num_opt {
                Some(n) if key.starts_with("pr-") => {
                    open_pr_set.is_empty() || open_pr_set.contains(&n)
                }
                Some(n) => open_issue_set.is_empty() || open_issue_set.contains(&n),
                None => true,
            };
            if still_open {
                items.push(format!("{key} exhausted retries ({count})"));
            }
        }
    }
    for (pr, title, ready_age) in &obs.ready_prs {
        if *ready_age >= rules.ready_unmerged_secs {
            items.push(format!(
                "PR #{pr} ready for {}m without merging: {title}",
                ready_age / 60
            ));
        }
    }
    let mut dedup = Vec::new();
    for item in items {
        if !dedup.contains(&item) {
            dedup.push(item);
        }
    }
    dedup
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Headline {
    pub state: String,
    pub main: String,
    pub deploy: String,
    pub agents_running: usize,
    pub batches_running: usize,
    pub needs_you: usize,
    pub attention: usize,
    pub last_tick_age: Option<i64>,
}

/// Complete `v1` status snapshot (R43, R44, SN21).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub updated_at: i64,
    pub headline: Headline,
    pub needs_you: Vec<String>,
    pub attention: Vec<String>,
    pub main: MainStatusBlock,
    pub agents: serde_json::Value,
    pub pipeline: serde_json::Value,
    pub held: serde_json::Value,
    pub flakes: Vec<String>,
    pub machine: serde_json::Value,
    pub cost: CostSummary,
    pub quotas: Option<serde_json::Value>,
}

/// Builds a redacted [`Snapshot`] from [`SnapshotObs`] (R43–R52, SN1–SN29).
pub fn snapshot(obs: &SnapshotObs, rules: &AttentionRules, cfg: &OpsConfig) -> Snapshot {
    let ny = needs_you(obs);
    let full_att = needs_attention(obs, rules, &cfg.tool);
    let state = if obs.paused {
        "paused".into()
    } else if obs.main_block.status == "failure" {
        "degraded".into()
    } else if !obs.running_agents.is_empty() || !obs.running_batches.is_empty() {
        "working".into()
    } else {
        "idle".into()
    };
    let deploy_str = obs
        .main_block
        .deploy
        .as_ref()
        .map(|d| {
            if d.status != "completed" {
                "running".into()
            } else {
                d.conclusion.clone()
            }
        })
        .unwrap_or_else(|| "unknown".into());

    let headline = Headline {
        state,
        main: obs.main_block.status.clone(),
        deploy: deploy_str,
        agents_running: obs.running_agents.len(),
        batches_running: obs.running_batches.len(),
        needs_you: ny.len(),
        attention: full_att.len(),
        last_tick_age: obs.last_tick_age,
    };
    let capped_att: Vec<String> = full_att.into_iter().take(rules.attention_cap).collect();
    let raw = Snapshot {
        version: VERSION,
        updated_at: obs.now,
        headline,
        needs_you: ny,
        attention: capped_att,
        main: obs.main_block.clone(),
        agents: serde_json::json!({
            "running": obs.running_agents.iter().map(|(k, kind, el)| {
                serde_json::json!({"key": k, "kind": kind, "elapsed": el})
            }).collect::<Vec<_>>()
        }),
        pipeline: serde_json::json!({
            "open_prs": obs.open_prs.len(),
            "open_issues": obs.open_issues.len(),
            "ready_prs": obs.ready_prs.len(),
            "batches": obs.running_batches.len(),
        }),
        held: serde_json::json!({
            "prs": obs.held_prs,
            "replays": obs.held_replays,
            "blocked_issues": obs.blocked_issues,
        }),
        flakes: obs.flakes.clone(),
        machine: obs.machine.clone(),
        cost: obs.cost.clone(),
        quotas: obs.quotas.clone(),
    };
    let val = serde_json::to_value(&raw).unwrap_or(serde_json::Value::Null);
    let scrubbed = redact_json(val, &obs.redact_literals);
    serde_json::from_value(scrubbed).unwrap_or(raw)
}

/// Extracts one or more named sections (supporting `needs` and `progress` aliases) (SN22).
pub fn section(shot: &Snapshot, names: &[&str]) -> Result<serde_json::Value, String> {
    let full = serde_json::to_value(shot).map_err(|e| e.to_string())?;
    let obj = full.as_object().ok_or("snapshot is not an object")?;
    if names.is_empty() {
        return Ok(full);
    }
    let mut expanded = Vec::new();
    for &n in names {
        for part in n.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match part {
                "needs" => expanded.extend(["needs_you", "attention"]),
                "progress" => expanded.extend(["agents", "pipeline"]),
                other if SECTIONS.contains(&other) => expanded.push(other),
                other => return Err(format!("unknown section '{other}'")),
            }
        }
    }
    if expanded.len() == 1 {
        return Ok(obj
            .get(expanded[0])
            .cloned()
            .unwrap_or(serde_json::Value::Null));
    }
    let mut out = serde_json::Map::new();
    for k in expanded {
        if let Some(v) = obj.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    Ok(serde_json::Value::Object(out))
}
