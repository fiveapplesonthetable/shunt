//! Legacy holds, retirement, guardrails, provider resolution, and PR rebase helpers.
#![allow(unused_imports)]

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use super::*;

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
    super::select::is_conflicting_str(mergeable, was_conflicting)
}

/// Order green PRs `(pr_number, additions, deletions)` for landing: smallest diff first, then lowest PR number.
pub fn merge_order(prs: &[(u64, u64, u64)]) -> Vec<u64> {
    let mut sorted = prs.to_vec();
    sorted.sort_by_key(|&(number, additions, deletions)| (additions + deletions, number));
    sorted.into_iter().map(|(n, _, _)| n).collect()
}

/// Validate PreToolUse / BeforeTool Bash invocations when running under managed worker guardrails.
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
