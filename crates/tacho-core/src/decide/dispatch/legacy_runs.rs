//! Legacy run records, lineage, PR attribution and resume helpers (pre-typed engine API).
#![allow(unused_imports)]

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

#[allow(unused_imports)]
use super::*;

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
    #[serde(default)]
    pub starttime: Option<u64>,
    #[serde(default)]
    pub cgroup: Option<String>,
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
    #[serde(default)]
    pub labels: Vec<String>,
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
    super::report::stated_pr(tail)
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
