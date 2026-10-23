//! Work queue selection, area serialization, prerequisites, holds, retirement,
//! run lineage/resumption, PR attachment, rebase handoffs, and `--compare` expansion.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

/// Resume backoff in seconds after a failed/exited attempt before automatic retry.
pub const RESUME_BACKOFF_SECS: i64 = 120;

/// Maximum automatic retries per base task key (`1` retry = `2` total attempts).
pub const MAX_RETRIES: u32 = 1;

/// Default hold age warning threshold in seconds (24 hours).
pub const STALE_HOLD_SECS: i64 = 86_400;

/// Built-in model shorthands mapped to `(provider, canonical_model)`.
pub const MODEL_ALIASES: &[(&str, (&str, &str))] = &[
    ("opus", ("claude", "opus")),
    ("sonnet", ("claude", "sonnet")),
    ("haiku", ("claude", "haiku")),
    ("codex", ("codex", "gpt-5.4")),
    ("gpt-5.4", ("codex", "gpt-5.4")),
    ("gpt-5.4-mini", ("codex", "gpt-5.4-mini")),
    ("gemini", ("gemini", "gemini-3.1-pro-preview")),
    ("gemini-pro", ("gemini", "gemini-3.1-pro-preview")),
    ("gemini-flash", ("gemini", "gemini-3-flash-preview")),
];

/// Supported agent providers.
pub const PROVIDERS: &[&str] = &["claude", "codex", "gemini"];

/// A queued work item on the todo queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TodoItem {
    pub key: String,
    pub brief: String,
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub compare: Option<String>,
}

/// A recorded agent run in fleet state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentRun {
    pub key: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub brief: String,
    #[serde(default)]
    pub worktree: String,
    #[serde(default)]
    pub log: String,
    #[serde(default)]
    pub status_note: String,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub pr: Option<u64>,
    #[serde(default)]
    pub pr_none: bool,
    #[serde(default)]
    pub pr_source: Option<String>,
    #[serde(default)]
    pub base_pr: Option<u64>,
    #[serde(default)]
    pub resume_at: Option<i64>,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub compare: Option<String>,
}

/// An explicit hold on a task key, PR (`pr:<n>`), or area (`area:<name>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Hold {
    pub reason: String,
    pub since: i64,
}

/// High-level lifecycle state of a base task across its attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Queued,
    Running,
    Done,
    Resuming,
    Stuck,
}

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Queued => "queued",
            RunState::Running => "running",
            RunState::Done => "done",
            RunState::Resuming => "resuming",
            RunState::Stuck => "stuck",
        }
    }
}

/// Gate verdict for an area chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaGate {
    pub ok: bool,
    pub base_pr: Option<u64>,
    pub reason: String,
}

/// Summary of an area chain's current status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaReport {
    pub area: String,
    pub state: String,
    pub base_pr: Option<u64>,
    pub detail: String,
    pub last_key: Option<String>,
}

/// Result of resolving provider, model, and effort for an agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Facts about a GitHub pull request used for dispatch, rebase, and stack checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PullFacts {
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub head: String,
    #[serde(default = "default_open_state")]
    pub state: String,
    #[serde(default)]
    pub mergeable: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub tree: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
}

fn default_open_state() -> String {
    "OPEN".to_string()
}

/// Strip a `-r<n>` retry suffix from a run key to recover its base task key.
pub fn base_key(key: &str) -> &str {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+)-r(\d+)$").unwrap());
    if let Some(caps) = RE.captures(key) {
        caps.get(1).map(|m| m.as_str()).unwrap_or(key)
    } else {
        key
    }
}

/// Return the retry attempt number (`0` for initial run, `1` for `-r1`, etc.).
pub fn attempt(key: &str) -> u32 {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+)-r(\d+)$").unwrap());
    if let Some(caps) = RE.captures(key) {
        caps.get(2)
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(0)
    } else {
        0
    }
}

/// Format a run key for a base key and retry count (`0 -> base`, `n -> base-rn`).
pub fn run_key(base: &str, retry: u32) -> String {
    let b = base_key(base);
    if retry == 0 {
        b.to_string()
    } else {
        format!("{b}-r{retry}")
    }
}

/// Return all runs belonging to `key`'s lineage, ordered by `(attempt, started_at, key)`.
pub fn lineage<'a>(
    agents: &'a BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    key: &str,
) -> Vec<&'a AgentRun> {
    let root = base_key(key);
    if retired.contains(root) {
        return Vec::new();
    }
    let mut chain: Vec<&'a AgentRun> = agents
        .values()
        .filter(|a| !retired.contains(&a.key) && base_key(&a.key) == root)
        .collect();
    chain.sort_by(|a, b| {
        attempt(&a.key)
            .cmp(&attempt(&b.key))
            .then_with(|| a.started_at.cmp(&b.started_at))
            .then_with(|| a.key.cmp(&b.key))
    });
    chain
}

/// Return the latest run in `key`'s lineage.
pub fn latest<'a>(
    agents: &'a BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    key: &str,
) -> Option<&'a AgentRun> {
    lineage(agents, retired, key).into_iter().last()
}

/// Return all unique base keys present in `agents` (excluding retired).
pub fn base_keys(agents: &BTreeMap<String, AgentRun>, retired: &BTreeSet<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    for k in agents.keys() {
        if !retired.contains(k) {
            let b = base_key(k);
            if !retired.contains(b) {
                seen.insert(b.to_string());
            }
        }
    }
    seen.into_iter().collect()
}

/// Parse an explicit `PR: #<n>` or `PR: none` marker from an agent's final output tail.
/// Returns `Some(Some(n))` for `PR: #<n>`, `Some(None)` for `PR: none`, and `None` if unstated.
pub fn stated_pr(tail: &str) -> Option<Option<u64>> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*PR:\s*(?:#(\d+)|(none))\s*$").unwrap());
    let mut found = None;
    for caps in RE.captures_iter(tail) {
        if let Some(num) = caps.get(1) {
            if let Ok(n) = num.as_str().parse::<u64>() {
                found = Some(Some(n));
            }
        } else if caps.get(2).is_some() {
            found = Some(None);
        }
    }
    found
}

fn branch_mentioned_in_log(branch: &str, tail: &str) -> bool {
    if branch.is_empty() {
        return false;
    }
    let escaped = regex::escape(branch);
    for line in tail.lines() {
        let re = match Regex::new(&format!(r"(?:^|[^\w.\-]){escaped}(?:$|[^\w.\-])")) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if re.is_match(line) {
            return true;
        }
    }
    false
}

/// Resolve the PR number and attribution source (`stated`, `none`, `branch`, `guessed`)
/// from open PRs and the agent's log tail.
///
/// Precedence:
/// 1. Explicit `PR: #<n>` or `PR: none` line in `tail` (`stated` / `none`).
/// 2. Exact branch candidate match (`<key>`, `<root>`, `work/<key>`, `feat/<key>`, `fix/<key>`)
///    or unambiguous word-bounded branch mention in `tail` (`branch`).
/// 3. `#<n>` or `pull/<n>` mention in `tail` matching an open PR (`guessed` — untrusted until `attach`).
pub fn find_pr(
    key: &str,
    started_at: i64,
    open_prs: &[PullFacts],
    tail: &str,
) -> (Option<u64>, Option<String>) {
    if let Some(stated) = stated_pr(tail) {
        return match stated {
            Some(n) => (Some(n), Some("stated".to_string())),
            None => (None, Some("none".to_string())),
        };
    }

    let root = base_key(key);
    let candidates = [
        key.to_string(),
        root.to_string(),
        format!("work/{key}"),
        format!("work/{root}"),
        format!("feat/{key}"),
        format!("feat/{root}"),
        format!("fix/{key}"),
        format!("fix/{root}"),
    ];
    for p in open_prs {
        if candidates.iter().any(|c| c == &p.branch) {
            return (Some(p.number), Some("branch".to_string()));
        }
    }

    let mentioned_branches: Vec<&PullFacts> = open_prs
        .iter()
        .filter(|p| branch_mentioned_in_log(&p.branch, tail))
        .collect();
    if mentioned_branches.len() == 1 {
        return (
            Some(mentioned_branches[0].number),
            Some("branch".to_string()),
        );
    }

    let _ = started_at;
    static GUESS_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:pull/|PR\s+#|#)(\d+)\b").unwrap());
    let open_numbers: HashSet<u64> = open_prs.iter().map(|p| p.number).collect();
    let mut guessed_nums: BTreeSet<u64> = BTreeSet::new();
    for caps in GUESS_RE.captures_iter(tail) {
        if let Some(m) = caps.get(1) {
            if let Ok(n) = m.as_str().parse::<u64>() {
                if open_numbers.contains(&n) {
                    guessed_nums.insert(n);
                }
            }
        }
    }
    if mentioned_branches.len() > 1 {
        let disambiguated: Vec<&&PullFacts> = mentioned_branches
            .iter()
            .filter(|p| guessed_nums.contains(&p.number))
            .collect();
        if disambiguated.len() == 1 {
            return (Some(disambiguated[0].number), Some("branch".to_string()));
        }
    }
    if let Some(&n) = guessed_nums.iter().next_back() {
        return (Some(n), Some("guessed".to_string()));
    }
    (None, None)
}

/// Extract the recorded PR number from an `AgentRun` only when trustworthy (`branch`, `stated`, `attached`, or legacy `None`).
/// A `guessed` PR is never treated as trusted until confirmed via `attach`.
pub fn recorded_pr(agent: &AgentRun) -> Option<u64> {
    let pr = agent.pr?;
    match agent.pr_source.as_deref() {
        None | Some("branch") | Some("stated") | Some("attached") => Some(pr),
        _ => None,
    }
}

/// Return a human-readable problem string if a finished run's PR attribution is untrusted or contradictory.
pub fn pr_problem(agent: &AgentRun, commits_ahead: Option<u32>) -> Option<String> {
    if agent.pr_source.as_deref() == Some("guessed") {
        if let Some(pr) = agent.pr {
            return Some(format!(
                "{} mentions PR #{} in output without a matching branch or 'PR: #{}' line (run 'tacho attach {} {}' to confirm)",
                agent.key, pr, pr, agent.key, pr
            ));
        }
    }
    if agent.pr_none && commits_ahead.unwrap_or(0) > 0 {
        return Some(format!(
            "{} reported 'PR: none' despite {} commit(s) in its worktree",
            agent.key,
            commits_ahead.unwrap_or(0)
        ));
    }
    if agent.finished_at.is_some()
        && agent.exit_code == Some(0)
        && agent.pr.is_none()
        && !agent.pr_none
    {
        return Some(format!(
            "{} exited 0 without opening a PR or stating 'PR: none' (run 'tacho attach {} <pr>|none')",
            agent.key, agent.key
        ));
    }
    None
}

/// Compute the lifecycle state of `key` across its attempts and the todo queue.
pub fn run_state(
    agents: &BTreeMap<String, AgentRun>,
    todo: &[TodoItem],
    retired: &BTreeSet<String>,
    live_keys: &BTreeSet<String>,
    key: &str,
) -> RunState {
    let root = base_key(key);
    let chain = lineage(agents, retired, root);
    if chain.iter().any(|a| live_keys.contains(&a.key)) {
        return RunState::Running;
    }
    let queued = todo
        .iter()
        .any(|t| !retired.contains(&t.key) && base_key(&t.key) == root);
    let Some(last) = chain.last() else {
        return if queued {
            RunState::Queued
        } else {
            RunState::Stuck
        };
    };
    if queued {
        return RunState::Resuming;
    }
    if recorded_pr(last).is_some() {
        return RunState::Done;
    }
    if last.pr_none && last.exit_code == Some(0) {
        return RunState::Done;
    }
    if attempt(&last.key) < MAX_RETRIES {
        return RunState::Resuming;
    }
    RunState::Stuck
}

/// Decide whether a finished run should automatically schedule a retry attempt.
pub fn needs_resume(
    agents: &BTreeMap<String, AgentRun>,
    todo: &[TodoItem],
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    key: &str,
    now: i64,
) -> bool {
    let root = base_key(key);
    if retired.contains(root) || holds.contains_key(root) {
        return false;
    }
    let chain = lineage(agents, retired, root);
    let Some(last) = chain.last() else {
        return false;
    };
    if !last.area.is_empty() && holds.contains_key(&format!("area:{}", last.area)) {
        return false;
    }
    if run_state(agents, todo, retired, live_keys, root) != RunState::Resuming {
        return false;
    }
    if todo.iter().any(|t| base_key(&t.key) == root) {
        return false;
    }
    let due = last
        .resume_at
        .unwrap_or_else(|| last.finished_at.unwrap_or(now) + RESUME_BACKOFF_SECS);
    now >= due
}

/// Build a resume `TodoItem` that continues work in the previous attempt's worktree.
pub fn resume_task(last: &AgentRun, commits: &str, status: &str, now: i64) -> TodoItem {
    let root = base_key(&last.key);
    let next_key = run_key(root, attempt(&last.key) + 1);
    let base_brief = last
        .brief
        .split("\n\nStopped:")
        .next()
        .unwrap_or(&last.brief)
        .trim();
    let mut parts = vec![
        base_brief.to_string(),
        format!(
            "Stopped: previous run ({}) exited (code {:?}) before opening a PR. Continue from worktree {} and finish.",
            last.key, last.exit_code, last.worktree
        ),
    ];
    if !commits.trim().is_empty() {
        parts.push(format!("Commits in worktree:\n{}", commits.trim()));
    }
    if !status.trim().is_empty() {
        parts.push(format!("Uncommitted changes:\n{}", status.trim()));
    }
    TodoItem {
        key: next_key,
        brief: parts.join("\n\n"),
        area: last.area.clone(),
        base: last.base_pr.map(|p| p.to_string()),
        after: Vec::new(),
        priority: 1,
        created_at: now,
        provider: if last.provider.is_empty() {
            None
        } else {
            Some(last.provider.clone())
        },
        model: last.model.clone(),
        effort: last.effort.clone(),
        compare: None,
    }
}

/// Return all runs currently in `RunState::Stuck`.
pub fn stuck_runs<'a>(
    agents: &'a BTreeMap<String, AgentRun>,
    todo: &[TodoItem],
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
) -> Vec<&'a AgentRun> {
    let mut out = Vec::new();
    for root in base_keys(agents, retired) {
        if holds.contains_key(&root) {
            continue;
        }
        if run_state(agents, todo, retired, live_keys, &root) == RunState::Stuck {
            if let Some(last) = latest(agents, retired, &root) {
                if !last.area.is_empty() && holds.contains_key(&format!("area:{}", last.area)) {
                    continue;
                }
                out.push(last);
            }
        }
    }
    out
}

/// Return a set of base keys that have completed (`recorded_pr` or `pr_none` with exit 0).
pub fn done_keys(
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for root in base_keys(agents, retired) {
        if let Some(last) = latest(agents, retired, &root) {
            if recorded_pr(last).is_some() || (last.pr_none && last.exit_code == Some(0)) {
                out.insert(root);
            }
        }
    }
    out
}

/// Extract all `pr:<n>` prerequisites referenced across the todo queue.
pub fn pending_prs(todo: &[TodoItem]) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    for item in todo {
        for dep in &item.after {
            if let Some(rest) = dep.strip_prefix("pr:") {
                if let Ok(n) = rest.parse::<u64>() {
                    out.insert(n);
                }
            }
        }
    }
    out
}

/// Check whether all `after` prerequisites of `item` are satisfied.
pub fn after_met(
    item: &TodoItem,
    done: &BTreeSet<String>,
    passed_prs: &BTreeSet<u64>,
    merged_prs: &BTreeSet<u64>,
) -> bool {
    for dep in &item.after {
        if let Some(rest) = dep.strip_prefix("pr:") {
            let Ok(n) = rest.parse::<u64>() else {
                return false;
            };
            if !passed_prs.contains(&n) && !merged_prs.contains(&n) {
                return false;
            }
        } else if !done.contains(dep) && !done.contains(base_key(dep)) {
            return false;
        }
    }
    true
}

/// Return any `pr:<n>` prerequisites of `item` that were closed without merging.
pub fn closed_after(item: &TodoItem, closed_prs: &BTreeSet<u64>) -> Vec<u64> {
    let mut out = Vec::new();
    for dep in &item.after {
        if let Some(rest) = dep.strip_prefix("pr:") {
            if let Ok(n) = rest.parse::<u64>() {
                if closed_prs.contains(&n) {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// Evaluate whether a new task in `area` may start, and which PR it should stack on.
///
/// Rules:
/// - Tasks without an `area` are never area-gated (`ok: true, base_pr: None`).
/// - If any agent in `area` is currently running, the area is busy (`ok: false`).
/// - Otherwise, inspect the latest non-retired agent in `area`:
///   - If it is still running or pending resume, wait (`ok: false`).
///   - If it has a PR attribution problem, block (`ok: false`).
///   - If it exited with `pr_none` (`exit == 0`), the area is free on `main` (`ok: true, base_pr: None`).
///   - If its PR merged or was closed unmerged, the area resets to `main` (`ok: true, base_pr: None`).
///   - If its PR passed batch verification, the next task stacks on that PR (`ok: true, base_pr: Some(pr)`).
///   - If its PR failed verification, block (`ok: false`).
///   - Otherwise, wait for batch verification of that PR (`ok: false`).
#[allow(clippy::too_many_arguments)]
pub fn area_gate(
    area: &str,
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    live_keys: &BTreeSet<String>,
    passed_prs: &BTreeSet<u64>,
    failed_prs: &BTreeSet<u64>,
    merged_prs: &BTreeSet<u64>,
    closed_prs: &BTreeSet<u64>,
) -> AreaGate {
    if area.trim().is_empty() {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: "no area".to_string(),
        };
    }
    let mut area_runs: Vec<&AgentRun> = agents
        .values()
        .filter(|a| {
            !retired.contains(&a.key) && !retired.contains(base_key(&a.key)) && a.area == area
        })
        .collect();
    if area_runs.is_empty() {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: "clean".to_string(),
        };
    }
    for a in &area_runs {
        if live_keys.contains(&a.key) {
            return AreaGate {
                ok: false,
                base_pr: None,
                reason: format!("{} is running", a.key),
            };
        }
    }
    area_runs.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| attempt(&a.key).cmp(&attempt(&b.key)))
            .then_with(|| a.key.cmp(&b.key))
    });
    let last = area_runs.last().copied().unwrap();
    if let Some(prob) = pr_problem(last, None) {
        return AreaGate {
            ok: false,
            base_pr: None,
            reason: prob,
        };
    }
    if last.pr_none && last.exit_code == Some(0) {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: format!("{} finished with no PR", last.key),
        };
    }
    let Some(pr) = recorded_pr(last) else {
        if attempt(&last.key) < MAX_RETRIES {
            return AreaGate {
                ok: false,
                base_pr: None,
                reason: format!("{} waiting to resume", last.key),
            };
        }
        return AreaGate {
            ok: false,
            base_pr: None,
            reason: format!("{} stopped without a PR", last.key),
        };
    };
    if merged_prs.contains(&pr) {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: format!("PR #{pr} merged"),
        };
    }
    if closed_prs.contains(&pr) {
        return AreaGate {
            ok: true,
            base_pr: None,
            reason: format!("PR #{pr} closed"),
        };
    }
    if failed_prs.contains(&pr) {
        return AreaGate {
            ok: false,
            base_pr: Some(pr),
            reason: format!("PR #{pr} failed verification"),
        };
    }
    if passed_prs.contains(&pr) {
        return AreaGate {
            ok: true,
            base_pr: Some(pr),
            reason: format!("stacked on passed PR #{pr}"),
        };
    }
    AreaGate {
        ok: false,
        base_pr: Some(pr),
        reason: format!("waiting for PR #{pr} to pass batch verification"),
    }
}

/// Apply `base_pr` stacking to a `TodoItem` when its `base` is not already pinned.
pub fn stacked_task(item: &TodoItem, base_pr: Option<u64>) -> TodoItem {
    let mut out = item.clone();
    if out.base.is_none() {
        if let Some(pr) = base_pr {
            out.base = Some(pr.to_string());
        }
    }
    out
}

/// Build an [`AreaReport`] for every area present across `agents` and `todo`.
#[allow(clippy::too_many_arguments)]
pub fn area_report(
    agents: &BTreeMap<String, AgentRun>,
    todo: &[TodoItem],
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    passed_prs: &BTreeSet<u64>,
    failed_prs: &BTreeSet<u64>,
    merged_prs: &BTreeSet<u64>,
    closed_prs: &BTreeSet<u64>,
) -> Vec<AreaReport> {
    let mut areas = BTreeSet::new();
    for a in agents.values() {
        if !retired.contains(&a.key) && !a.area.is_empty() {
            areas.insert(a.area.clone());
        }
    }
    for t in todo {
        if !retired.contains(&t.key) && !t.area.is_empty() {
            areas.insert(t.area.clone());
        }
    }
    let mut out = Vec::new();
    for area in areas {
        let hold_key = format!("area:{area}");
        if let Some(h) = holds.get(&hold_key) {
            out.push(AreaReport {
                area,
                state: "held".to_string(),
                base_pr: None,
                detail: h.reason.clone(),
                last_key: None,
            });
            continue;
        }
        let gate = area_gate(
            &area, agents, retired, live_keys, passed_prs, failed_prs, merged_prs, closed_prs,
        );
        let last_key = agents
            .values()
            .filter(|a| !retired.contains(&a.key) && a.area == area)
            .max_by_key(|a| (a.started_at, attempt(&a.key)))
            .map(|a| a.key.clone());
        let state = if gate.ok {
            "ready"
        } else if gate.reason.contains("is running") {
            "running"
        } else if gate.reason.contains("waiting for PR")
            || gate.reason.contains("waiting to resume")
        {
            "verifying"
        } else {
            "blocked"
        };
        out.push(AreaReport {
            area,
            state: state.to_string(),
            base_pr: gate.base_pr,
            detail: gate.reason,
            last_key,
        });
    }
    out
}

/// Select up to `free_slots` ready tasks from `todo` (plus any due automatic resumes).
///
/// Tasks are ordered by `(-priority, created_at, index)` and respect:
/// - `retired` keys
/// - `holds` on task keys (`<key>`) or areas (`area:<area>`)
/// - `after` prerequisites (`<task-key>` or `pr:<n>`)
/// - `closed_after` prerequisites (never start if an upstream PR closed unmerged)
/// - `area_gate` (at most one task dispatched per area per tick, stacked on passed PR)
#[allow(clippy::too_many_arguments)]
pub fn next_work(
    todo: &[TodoItem],
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    passed_prs: &BTreeSet<u64>,
    failed_prs: &BTreeSet<u64>,
    merged_prs: &BTreeSet<u64>,
    closed_prs: &BTreeSet<u64>,
    free_slots: usize,
) -> (Vec<TodoItem>, Vec<TodoItem>) {
    if free_slots == 0 {
        return (Vec::new(), todo.to_vec());
    }
    let done = done_keys(agents, retired);
    let mut indexed: Vec<(usize, &TodoItem)> = todo.iter().enumerate().collect();
    indexed.sort_by(|(ia, a), (ib, b)| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| ia.cmp(ib))
    });

    let mut chosen = Vec::new();
    let mut chosen_indices = HashSet::new();
    let mut claimed_areas = HashSet::new();

    for (idx, item) in indexed {
        if chosen.len() >= free_slots {
            break;
        }
        let root = base_key(&item.key);
        if retired.contains(&item.key) || retired.contains(root) {
            chosen_indices.insert(idx);
            continue;
        }
        if holds.contains_key(&item.key) || holds.contains_key(root) {
            continue;
        }
        if !item.area.is_empty() && holds.contains_key(&format!("area:{}", item.area)) {
            continue;
        }
        if live_keys.contains(&item.key) || live_keys.contains(root) {
            continue;
        }
        if !closed_after(item, closed_prs).is_empty() {
            continue;
        }
        if !after_met(item, &done, passed_prs, merged_prs) {
            continue;
        }
        let mut base_pr = None;
        if !item.area.is_empty() {
            if claimed_areas.contains(&item.area) {
                continue;
            }
            let gate = area_gate(
                &item.area, agents, retired, live_keys, passed_prs, failed_prs, merged_prs,
                closed_prs,
            );
            if !gate.ok {
                continue;
            }
            base_pr = gate.base_pr;
            claimed_areas.insert(item.area.clone());
        }
        chosen.push(stacked_task(item, base_pr));
        chosen_indices.insert(idx);
    }

    let remaining = todo
        .iter()
        .enumerate()
        .filter(|(i, _)| !chosen_indices.contains(i))
        .map(|(_, t)| t.clone())
        .collect();
    (chosen, remaining)
}

/// Explain why a specific queued task can or cannot start right now.
#[allow(clippy::too_many_arguments)]
pub fn may_start(
    item: &TodoItem,
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    passed_prs: &BTreeSet<u64>,
    failed_prs: &BTreeSet<u64>,
    merged_prs: &BTreeSet<u64>,
    closed_prs: &BTreeSet<u64>,
) -> (bool, String) {
    let root = base_key(&item.key);
    if retired.contains(&item.key) || retired.contains(root) {
        return (false, format!("{} is retired", item.key));
    }
    if let Some(h) = holds.get(&item.key).or_else(|| holds.get(root)) {
        return (false, format!("held: {}", h.reason));
    }
    if !item.area.is_empty() {
        if let Some(h) = holds.get(&format!("area:{}", item.area)) {
            return (false, format!("area:{} held: {}", item.area, h.reason));
        }
    }
    let closed = closed_after(item, closed_prs);
    if !closed.is_empty() {
        return (
            false,
            format!("prerequisite PR {:?} closed unmerged", closed),
        );
    }
    let done = done_keys(agents, retired);
    if !after_met(item, &done, passed_prs, merged_prs) {
        return (false, format!("waiting on after={:?}", item.after));
    }
    if !item.area.is_empty() {
        let gate = area_gate(
            &item.area, agents, retired, live_keys, passed_prs, failed_prs, merged_prs, closed_prs,
        );
        if !gate.ok {
            return (false, gate.reason);
        }
        return (true, gate.reason);
    }
    (true, "ready".to_string())
}

/// Validate a hold target before adding it.
pub fn hold_problem(
    target: &str,
    todo: &[TodoItem],
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    known_areas: &BTreeSet<String>,
) -> Option<String> {
    if target.trim().is_empty() {
        return Some("hold target cannot be empty".to_string());
    }
    if let Some(rest) = target.strip_prefix("pr:") {
        if rest.parse::<u64>().ok().filter(|&n| n > 0).is_none() {
            return Some(format!(
                "invalid PR hold target '{target}' (expected pr:<n>)"
            ));
        }
        return None;
    }
    if let Some(area) = target
        .strip_prefix("area:")
        .or_else(|| target.strip_prefix("group:"))
    {
        if area.is_empty() {
            return Some("area/group hold target cannot be empty".to_string());
        }
        let in_use = known_areas.contains(area)
            || todo.iter().any(|t| t.area == area)
            || agents.values().any(|a| a.area == area);
        if !in_use && !known_areas.is_empty() {
            return Some(format!("unknown area '{area}'"));
        }
        return None;
    }
    if retired.contains(target) {
        return Some(format!("'{target}' is already retired"));
    }
    let exists = todo
        .iter()
        .any(|t| t.key == target || base_key(&t.key) == target)
        || agents
            .values()
            .any(|a| a.key == target || base_key(&a.key) == target);
    if !exists {
        return Some(format!("unknown task or agent '{target}'"));
    }
    None
}

/// Validate releasing a hold.
pub fn release_problem(target: &str, holds: &BTreeMap<String, Hold>) -> Option<String> {
    if !holds.contains_key(target) {
        Some(format!("no active hold on '{target}'"))
    } else {
        None
    }
}

/// Add or update a hold in `holds`.
pub fn with_hold(
    holds: &BTreeMap<String, Hold>,
    target: &str,
    reason: &str,
    now: i64,
) -> BTreeMap<String, Hold> {
    let mut out = holds.clone();
    out.insert(
        target.to_string(),
        Hold {
            reason: if reason.trim().is_empty() {
                "manual hold".to_string()
            } else {
                reason.trim().to_string()
            },
            since: now,
        },
    );
    out
}

/// Remove a hold from `holds`.
pub fn without_hold(holds: &BTreeMap<String, Hold>, target: &str) -> BTreeMap<String, Hold> {
    let mut out = holds.clone();
    out.remove(target);
    out
}

/// Return holds older than `max_age` seconds (`(target, reason, age_secs)`).
pub fn stale_holds(
    holds: &BTreeMap<String, Hold>,
    now: i64,
    max_age: i64,
) -> Vec<(String, String, i64)> {
    let mut out = Vec::new();
    for (k, h) in holds {
        let age = (now - h.since).max(0);
        if age >= max_age {
            out.push((k.clone(), h.reason.clone(), age));
        }
    }
    out
}

/// Validate retiring a task/run key.
pub fn retire_problem(
    key: &str,
    todo: &[TodoItem],
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    live_keys: &BTreeSet<String>,
) -> Option<String> {
    let root = base_key(key);
    if live_keys.contains(key) || live_keys.contains(root) {
        return Some(format!(
            "cannot retire '{key}' while it is running (stop it first)"
        ));
    }
    if retired.contains(key) || retired.contains(root) {
        return Some(format!("'{key}' is already retired"));
    }
    let exists = todo
        .iter()
        .any(|t| t.key == key || base_key(&t.key) == root)
        || agents
            .values()
            .any(|a| a.key == key || base_key(&a.key) == root);
    if !exists {
        return Some(format!("unknown task or agent '{key}'"));
    }
    None
}

/// Retire a task/run key, removing it from `todo`, stripping it from downstream `todo.after`
/// lists, removing it from `holds`, and recording its lineage in `retired`.
pub fn retire(
    key: &str,
    todo: &[TodoItem],
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    holds: &BTreeMap<String, Hold>,
) -> (Vec<TodoItem>, BTreeSet<String>, BTreeMap<String, Hold>) {
    let root = base_key(key);
    let new_todo = todo
        .iter()
        .filter(|t| t.key != key && base_key(&t.key) != root)
        .map(|t| {
            let mut item = t.clone();
            item.after.retain(|dep| dep != key && base_key(dep) != root);
            item
        })
        .collect();
    let mut new_retired = retired.clone();
    new_retired.insert(root.to_string());
    new_retired.insert(key.to_string());
    for a in agents.values() {
        if base_key(&a.key) == root {
            new_retired.insert(a.key.clone());
        }
    }
    let mut new_holds = holds.clone();
    new_holds.remove(key);
    new_holds.remove(root);
    (new_todo, new_retired, new_holds)
}

/// Return the set of batch group names currently held in `holds` (`group:<name>`, `area:<name>`, or direct group key).
pub fn held_groups(holds: &BTreeMap<String, Hold>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for k in holds.keys() {
        if let Some(g) = k.strip_prefix("group:").or_else(|| k.strip_prefix("area:")) {
            out.insert(g.to_string());
        } else if !k.starts_with("pr:") {
            out.insert(k.clone());
        }
    }
    out
}

/// GitHub reports `UNKNOWN` while computing mergeability; the prior `was_conflicting` state stands until resolved.
pub fn is_conflicting(mergeable: &str, was_conflicting: bool) -> bool {
    if mergeable.eq_ignore_ascii_case("CONFLICTING") {
        true
    } else if mergeable.eq_ignore_ascii_case("MERGEABLE") {
        false
    } else {
        was_conflicting
    }
}

/// Order green PRs `(pr_number, additions, deletions)` for landing: smallest diff first, then lowest PR number.
pub fn merge_order(prs: &[(u64, u64, u64)]) -> Vec<u64> {
    let mut sorted = prs.to_vec();
    sorted.sort_by_key(|&(number, additions, deletions)| (additions + deletions, number));
    sorted.into_iter().map(|(n, _, _)| n).collect()
}

/// Validate PreToolUse / BeforeTool Bash invocations when running under managed worker guardrails.
///
/// Rules:
/// 1. Denies `run_in_background: true`.
/// 2. Denies free-running CPU burners (`yes`, `stress`, `stress-ng`) unless `under_starve == true`.
/// 3. Denies `git stash` inside isolated worker worktrees.
/// 4. Denies `--no-verify` or `git commit -n`.
/// 5. Denies `Co-Authored-By:` or `Generated with/by` attribution trailers in `git commit` / `gh pr` commands.
pub fn guard_bash_tool(
    run_in_background: bool,
    command: &str,
    under_starve: bool,
) -> Result<(), String> {
    if run_in_background {
        return Err(
            "Do not use run_in_background. Run the command in the foreground or use `tacho job <name> -- <cmd>` and `tacho wait <name>`."
                .to_string(),
        );
    }
    if !under_starve {
        static BURNER_CMD_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?:^|[;&|]\s*|\bnohup\s+)(?:yes|stress(?:-ng)?)\b").unwrap()
        });
        if BURNER_CMD_RE.is_match(command) && !command.contains("starve") {
            return Err(
                "Do not run CPU burners (yes, stress, stress-ng) directly; run starvation tests under `tacho starve -- <command>`."
                    .to_string(),
            );
        }
    }
    static GIT_STASH_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:^|[;&|]\s*)git\s+stash\b").unwrap());
    if GIT_STASH_RE.is_match(command) {
        return Err(
            "Do not use `git stash` in an isolated worktree; commit your work or discard unwanted edits explicitly."
                .to_string(),
        );
    }
    static NO_VERIFY_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:^|[;&|]\s*)git\s+(?:commit|push)\b[^\n]*(?:\s--no-verify\b|\s-n\b)")
            .unwrap()
    });
    if NO_VERIFY_RE.is_match(command) {
        return Err(
            "Do not bypass verification hooks with `--no-verify` or `-n`; run `tacho prove` and `tacho pr`."
                .to_string(),
        );
    }
    let attr = crate::decide::verify::attribution(&[command]);
    if !attr.is_empty() {
        return Err(format!(
            "Command refused ({}); remove tool attribution trailers.",
            attr.join(", ")
        ));
    }
    Ok(())
}

/// Infer the provider for a model shorthand or identifier.
pub fn provider_of(model: Option<&str>) -> Option<&'static str> {
    let m = model?.trim().to_ascii_lowercase();
    if m.is_empty() {
        return None;
    }
    for (alias, (prov, _)) in MODEL_ALIASES {
        if *alias == m {
            return Some(*prov);
        }
    }
    if m.starts_with("claude") || m.contains("opus") || m.contains("sonnet") || m.contains("haiku")
    {
        return Some("claude");
    }
    if m.starts_with("gpt-") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("codex")
    {
        return Some("codex");
    }
    if m.starts_with("gemini") {
        return Some("gemini");
    }
    None
}

/// Resolve `(provider, model, effort)` for a task, validating compatibility.
pub fn resolve(
    provider: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    default_provider: &str,
    default_model: Option<&str>,
    default_effort: Option<&str>,
) -> Result<Choice, String> {
    let raw_model = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| default_model.map(str::trim).filter(|s| !s.is_empty()));
    let canonical_model = raw_model.map(|m| {
        for (alias, (_, canon)) in MODEL_ALIASES {
            if alias.eq_ignore_ascii_case(m) {
                return (*canon).to_string();
            }
        }
        m.to_string()
    });
    let inferred = raw_model.and_then(|m| provider_of(Some(m)));
    let prov = match provider.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => {
            if !PROVIDERS.contains(&p) {
                return Err(format!(
                    "unknown provider '{p}' (expected one of {PROVIDERS:?})"
                ));
            }
            if let Some(inf) = inferred {
                if inf != p {
                    return Err(format!(
                        "model '{}' belongs to provider '{inf}', not '{p}'",
                        raw_model.unwrap_or_default()
                    ));
                }
            }
            p.to_string()
        }
        None => inferred.unwrap_or(default_provider).to_string(),
    };
    let eff = effort
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| default_effort.map(str::trim).filter(|s| !s.is_empty()))
        .map(str::to_string);
    Ok(Choice {
        provider: prov,
        model: canonical_model,
        effort: eff,
    })
}

/// Parse a comma-separated `--compare` spec (e.g. `"opus,codex,gemini"`) into `(label, Choice)` pairs.
pub fn parse_compare(
    spec: &str,
    effort: Option<&str>,
    default_effort: Option<&str>,
) -> Result<Vec<(String, Choice)>, String> {
    let parts: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() < 2 {
        return Err("--compare requires at least 2 comma-separated models/providers".to_string());
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for part in parts {
        let label = part
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        if !seen.insert(label.clone()) {
            return Err(format!("duplicate entry '{part}' in --compare"));
        }
        let choice = if PROVIDERS.contains(&part) && !MODEL_ALIASES.iter().any(|(a, _)| *a == part)
        {
            resolve(Some(part), None, effort, part, None, default_effort)?
        } else {
            resolve(None, Some(part), effort, "claude", None, default_effort)?
        };
        out.push((label, choice));
    }
    Ok(out)
}

/// Expand a `TodoItem` with `compare` set into side-by-side worker tasks with clear area (`""`) so they run in parallel.
pub fn compare_items(
    item: &TodoItem,
    default_effort: Option<&str>,
) -> Result<Vec<TodoItem>, String> {
    let Some(ref spec) = item.compare else {
        return Ok(vec![item.clone()]);
    };
    let choices = parse_compare(spec, item.effort.as_deref(), default_effort)?;
    let group = item.key.clone();
    let mut out = Vec::new();
    for (label, choice) in choices {
        out.push(TodoItem {
            key: format!("{group}-{label}"),
            brief: item.brief.clone(),
            area: String::new(),
            base: item.base.clone(),
            after: item.after.clone(),
            priority: item.priority,
            created_at: item.created_at,
            provider: Some(choice.provider),
            model: choice.model,
            effort: choice.effort,
            compare: Some(group.clone()),
        });
    }
    Ok(out)
}

/// Check whether a pull request is valid for dispatch/rebase/fix work.
pub fn refusal(pr: Option<&PullFacts>) -> Option<String> {
    let Some(p) = pr else {
        return Some("PR not found".to_string());
    };
    if !p.state.eq_ignore_ascii_case("OPEN") {
        return Some(format!("PR #{} is {} (not OPEN)", p.number, p.state));
    }
    if p.branch.trim().is_empty() || p.head.trim().is_empty() {
        return Some(format!("PR #{} is missing branch or head SHA", p.number));
    }
    None
}

/// Check whether an auxiliary agent (`rebase-pr<n>` or `fix-pr<n>`) already ran for the exact `head` SHA.
pub fn already_ran(
    prefix: &str,
    pr_number: u64,
    head: &str,
    agents: &BTreeMap<String, AgentRun>,
    live_keys: &BTreeSet<String>,
) -> bool {
    let key_prefix = format!("{prefix}-pr{pr_number}");
    for a in agents.values() {
        if a.key == key_prefix || a.key.starts_with(&format!("{key_prefix}-")) {
            if live_keys.contains(&a.key) {
                return true;
            }
            if !head.is_empty() && a.brief.contains(head) {
                return true;
            }
        }
    }
    false
}

/// Decide which conflicting PRs should receive an automatic rebase agent on this tick (`next_hands`)
/// and which have already spent their single automatic attempt at their current head (`barred`).
pub fn next_hands<'a>(
    open_prs: &'a [PullFacts],
    agents: &BTreeMap<String, AgentRun>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    attempted: &HashMap<u64, String>,
    free_slots: usize,
) -> (Vec<&'a PullFacts>, Vec<(&'a PullFacts, String)>) {
    let mut to_rebase = Vec::new();
    let mut barred = Vec::new();
    for p in open_prs {
        if !p.mergeable.eq_ignore_ascii_case("CONFLICTING") {
            continue;
        }
        if holds.contains_key(&format!("pr:{}", p.number)) {
            continue;
        }
        if already_ran("rebase", p.number, &p.head, agents, live_keys) {
            continue;
        }
        if attempted.get(&p.number).map(String::as_str) == Some(p.head.as_str()) {
            barred.push((
                p,
                format!(
                    "PR #{} still conflicts at {} after rebase attempt",
                    p.number,
                    &p.head[..p.head.len().min(8)]
                ),
            ));
            continue;
        }
        if to_rebase.len() < free_slots {
            to_rebase.push(p);
        }
    }
    (to_rebase, barred)
}

// =============================================================================
// Provider Interface & Configuration
// =============================================================================

/// Planned command invocation for a provider lifecycle step (`start` or `resume`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCommand {
    /// Binary or script to execute.
    pub program: String,
    /// Arguments passed to `program`.
    pub args: Vec<String>,
    /// Environment variables `(key, value)` to set for the child process.
    pub env: Vec<(String, String)>,
}

/// Planned delivery for `tacho tell`: either live delivery into a running worker
/// (via an inbox / live command) or queued for the next turn/resume without killing
/// the running session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TellPlan {
    /// Deliver to a live running session via a custom provider tell command (`{session}`, `{message}`, `{worktree}`).
    LiveCommand { program: String, args: Vec<String> },
    /// Deliver to a live running session via the managed hook inbox so the next PreToolUse/UserPromptSubmit
    /// hook injects the message into the running turn without restarting the worker.
    LiveInbox {
        key: String,
        session_id: Option<String>,
        message: String,
    },
    /// Worker is not currently live; resume the session with the message.
    ResumeWithMessage {
        key: String,
        session_id: Option<String>,
        command: ProviderCommand,
    },
}

/// Planned stop action for a provider worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopPlan {
    pub key: String,
    pub pid: Option<u32>,
    pub custom_cmd: Option<(String, Vec<String>)>,
}

/// Trait representing an agent provider (Claude, Codex, Gemini, or custom configured provider).
///
/// Every method is a pure planner or parser over plain data structures:
/// - `start`: plan the command to start a new worker session with a prompt in a worktree.
/// - `resume`: plan the command to resume an existing session (by `session_id` or in-place worktree) with a prompt.
/// - `tell`: plan how to deliver a message to a live or stopped worker without cold-restarting a live session.
/// - `alive`: check whether a worker is alive given its PID and observed live process table.
/// - `stop`: plan how to stop a worker session cleanly.
/// - `cost`: parse or compute the USD cost of a provider run from its transcript/usage observation.
pub trait Provider {
    /// Provider identifier (e.g. `"claude"`, `"codex"`, `"gemini"`).
    fn name(&self) -> &str;

    /// Plan the command to launch a fresh worker session.
    fn start(
        &self,
        key: &str,
        worktree: &str,
        prompt: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand;

    /// Plan the command to resume a prior worker session (`session_id` if available) with `message`.
    fn resume(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        message: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand;

    /// Plan how to deliver `message` to a worker (`is_live` indicates whether the worker process is currently running).
    fn tell(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        message: &str,
        is_live: bool,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> TellPlan;

    /// Return whether the worker is currently alive given its recorded `pid` and the set of live PIDs on the host.
    fn alive(&self, pid: Option<u32>, live_pids: &BTreeSet<u32>) -> bool;

    /// Plan how to stop the worker.
    fn stop(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        pid: Option<u32>,
    ) -> StopPlan;

    /// Compute or extract USD cost from a provider transcript (`jsonl_lines`) or fallback token counts.
    fn cost(&self, jsonl_lines: &[&str], model: Option<&str>) -> f64;
}

/// Configurable specification for an agent provider.
///
/// Allows projects to configure custom providers or override flags/binaries for built-in providers
/// (`claude`, `codex`, `gemini`) in fleet config without modifying Rust code.
///
/// Template placeholders supported in `start_args`, `resume_args`, `tell_args`, and `stop_args`:
/// - `{prompt}` / `{message}`: the task brief or follow-up message
/// - `{session}`: the provider session ID
/// - `{worktree}`: the worker worktree path
/// - `{key}`: the worker key
/// - `{model}`: the resolved model name
/// - `{effort}`: the resolved effort level
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSpec {
    pub name: String,
    pub binary: String,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub start_args: Vec<String>,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub resume_without_session_args: Vec<String>,
    #[serde(default)]
    pub model_flag: Option<String>,
    #[serde(default)]
    pub effort_flag: Option<String>,
    #[serde(default)]
    pub tell_program: Option<String>,
    #[serde(default)]
    pub tell_args: Vec<String>,
    #[serde(default)]
    pub stop_program: Option<String>,
    #[serde(default)]
    pub stop_args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

impl ProviderSpec {
    /// Built-in specification for Claude Code (`claude`).
    pub fn claude() -> Self {
        Self {
            name: "claude".to_string(),
            binary: "claude".to_string(),
            default_model: Some("opus".to_string()),
            default_effort: None,
            start_args: vec![
                "--dangerously-skip-permissions".to_string(),
                "-p".to_string(),
                "{prompt}".to_string(),
            ],
            resume_args: vec![
                "--dangerously-skip-permissions".to_string(),
                "--resume".to_string(),
                "{session}".to_string(),
                "-p".to_string(),
                "{message}".to_string(),
            ],
            resume_without_session_args: vec![
                "--dangerously-skip-permissions".to_string(),
                "--continue".to_string(),
                "-p".to_string(),
                "{message}".to_string(),
            ],
            model_flag: Some("--model".to_string()),
            effort_flag: Some("--effort".to_string()),
            tell_program: None,
            tell_args: Vec::new(),
            stop_program: None,
            stop_args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Built-in specification for Codex CLI (`codex`).
    pub fn codex() -> Self {
        Self {
            name: "codex".to_string(),
            binary: "codex".to_string(),
            default_model: Some("gpt-5.4".to_string()),
            default_effort: None,
            start_args: vec![
                "exec".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "{prompt}".to_string(),
            ],
            resume_args: vec![
                "exec".to_string(),
                "resume".to_string(),
                "{session}".to_string(),
                "{message}".to_string(),
            ],
            resume_without_session_args: vec![
                "exec".to_string(),
                "resume".to_string(),
                "--last".to_string(),
                "{message}".to_string(),
            ],
            model_flag: Some("-m".to_string()),
            effort_flag: None,
            tell_program: None,
            tell_args: Vec::new(),
            stop_program: None,
            stop_args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Built-in specification for Gemini CLI (`gemini`).
    pub fn gemini() -> Self {
        Self {
            name: "gemini".to_string(),
            binary: "gemini".to_string(),
            default_model: Some("gemini-3.1-pro-preview".to_string()),
            default_effort: None,
            start_args: vec![
                "--yolo".to_string(),
                "-p".to_string(),
                "{prompt}".to_string(),
            ],
            resume_args: vec![
                "--yolo".to_string(),
                "--resume".to_string(),
                "{session}".to_string(),
                "-p".to_string(),
                "{message}".to_string(),
            ],
            resume_without_session_args: vec![
                "--yolo".to_string(),
                "--resume".to_string(),
                "latest".to_string(),
                "-p".to_string(),
                "{message}".to_string(),
            ],
            model_flag: Some("-m".to_string()),
            effort_flag: None,
            tell_program: None,
            tell_args: Vec::new(),
            stop_program: None,
            stop_args: Vec::new(),
            env: Vec::new(),
        }
    }

    fn expand_template(
        template: &[String],
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        prompt_or_msg: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Vec<String> {
        template
            .iter()
            .map(|arg| {
                arg.replace("{key}", key)
                    .replace("{worktree}", worktree)
                    .replace("{session}", session_id.unwrap_or(""))
                    .replace("{prompt}", prompt_or_msg)
                    .replace("{message}", prompt_or_msg)
                    .replace("{model}", model.unwrap_or(""))
                    .replace("{effort}", effort.unwrap_or(""))
            })
            .collect()
    }

    fn build_command(
        &self,
        template: &[String],
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        prompt_or_msg: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand {
        let eff_model = model
            .filter(|s| !s.trim().is_empty())
            .or(self.default_model.as_deref());
        let eff_effort = effort
            .filter(|s| !s.trim().is_empty())
            .or(self.default_effort.as_deref());

        let mut args = Vec::new();
        let template_uses_model = template.iter().any(|a| a.contains("{model}"));
        let template_uses_effort = template.iter().any(|a| a.contains("{effort}"));

        if !template_uses_model {
            if let (Some(flag), Some(m)) = (&self.model_flag, eff_model) {
                args.push(flag.clone());
                args.push(m.to_string());
            }
        }
        if !template_uses_effort {
            if let (Some(flag), Some(e)) = (&self.effort_flag, eff_effort) {
                args.push(flag.clone());
                args.push(e.to_string());
            }
        }

        args.extend(Self::expand_template(
            template,
            key,
            worktree,
            session_id,
            prompt_or_msg,
            eff_model,
            eff_effort,
        ));

        let mut env = self.env.clone();
        env.push(("TACHO_AGENT".to_string(), key.to_string()));
        env.push(("TACHO_WORKTREE".to_string(), worktree.to_string()));

        ProviderCommand {
            program: self.binary.clone(),
            args,
            env,
        }
    }
}

impl Provider for ProviderSpec {
    fn name(&self) -> &str {
        &self.name
    }

    fn start(
        &self,
        key: &str,
        worktree: &str,
        prompt: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand {
        self.build_command(&self.start_args, key, worktree, None, prompt, model, effort)
    }

    fn resume(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        message: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> ProviderCommand {
        let template = if session_id.filter(|s| !s.trim().is_empty()).is_some() {
            &self.resume_args
        } else if !self.resume_without_session_args.is_empty() {
            &self.resume_without_session_args
        } else {
            &self.start_args
        };
        self.build_command(template, key, worktree, session_id, message, model, effort)
    }

    fn tell(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        message: &str,
        is_live: bool,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> TellPlan {
        if is_live {
            if let Some(ref prog) = self.tell_program {
                let args = Self::expand_template(
                    &self.tell_args,
                    key,
                    worktree,
                    session_id,
                    message,
                    model,
                    effort,
                );
                return TellPlan::LiveCommand {
                    program: prog.clone(),
                    args,
                };
            }
            return TellPlan::LiveInbox {
                key: key.to_string(),
                session_id: session_id.map(str::to_string),
                message: message.to_string(),
            };
        }
        TellPlan::ResumeWithMessage {
            key: key.to_string(),
            session_id: session_id.map(str::to_string),
            command: self.resume(key, worktree, session_id, message, model, effort),
        }
    }

    fn alive(&self, pid: Option<u32>, live_pids: &BTreeSet<u32>) -> bool {
        pid.is_some_and(|p| p > 0 && live_pids.contains(&p))
    }

    fn stop(
        &self,
        key: &str,
        worktree: &str,
        session_id: Option<&str>,
        pid: Option<u32>,
    ) -> StopPlan {
        let custom_cmd = self.stop_program.as_ref().map(|prog| {
            (
                prog.clone(),
                Self::expand_template(&self.stop_args, key, worktree, session_id, "", None, None),
            )
        });
        StopPlan {
            key: key.to_string(),
            pid,
            custom_cmd,
        }
    }

    fn cost(&self, jsonl_lines: &[&str], model: Option<&str>) -> f64 {
        let eff_model = model
            .or(self.default_model.as_deref())
            .unwrap_or(&self.name);
        let mut explicit_usd = 0.0;
        let mut in_tok = 0u64;
        let mut out_tok = 0u64;
        let mut cache_read = 0u64;
        let mut cache_write = 0u64;
        for line in jsonl_lines {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(c) = v.get("total_cost_usd").and_then(|x| x.as_f64()) {
                if c > explicit_usd {
                    explicit_usd = c;
                }
            }
            if let Some(u) = v.get("usage") {
                in_tok += u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                out_tok += u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                cache_read += u
                    .get("cache_read_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                cache_write += u
                    .get("cache_creation_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
            }
        }
        if explicit_usd > 0.0 {
            explicit_usd
        } else {
            crate::decide::watch::price(eff_model, in_tok, out_tok, cache_read, cache_write)
        }
    }
}

/// Return the default built-in provider registry (`claude`, `codex`, `gemini`).
pub fn builtin_providers() -> BTreeMap<String, ProviderSpec> {
    let mut map = BTreeMap::new();
    map.insert("claude".to_string(), ProviderSpec::claude());
    map.insert("codex".to_string(), ProviderSpec::codex());
    map.insert("gemini".to_string(), ProviderSpec::gemini());
    map
}

/// Resolve `(provider, model, effort)` against a custom provider map (falling back to built-in providers).
pub fn resolve_with_providers(
    provider: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    default_provider: &str,
    default_model: Option<&str>,
    default_effort: Option<&str>,
    custom_providers: &BTreeMap<String, ProviderSpec>,
) -> Result<Choice, String> {
    if let Some(p) = provider.map(str::trim).filter(|s| !s.is_empty()) {
        if custom_providers.contains_key(p) && !PROVIDERS.contains(&p) {
            let spec = &custom_providers[p];
            return Ok(Choice {
                provider: p.to_string(),
                model: model
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| spec.default_model.clone())
                    .or_else(|| default_model.map(str::to_string)),
                effort: effort
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| spec.default_effort.clone())
                    .or_else(|| default_effort.map(str::to_string)),
            });
        }
    }
    resolve(
        provider,
        model,
        effort,
        default_provider,
        default_model,
        default_effort,
    )
}

/// Order open PRs `(pr_number, migration_numbers)` by their lowest numbered schema migration
/// so PRs with earlier database migrations land first.
pub fn migration_order(prs: &[(u64, Vec<u32>)]) -> Vec<u64> {
    let mut keyed: Vec<(u32, u64)> = prs
        .iter()
        .map(|(pr, migs)| (migs.iter().copied().min().unwrap_or(u32::MAX), *pr))
        .collect();
    keyed.sort_by_key(|&(lowest_mig, pr)| (lowest_mig, pr));
    keyed.into_iter().map(|(_, pr)| pr).collect()
}
