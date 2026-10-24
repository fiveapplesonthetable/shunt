//! Final-report detection and PR attribution: `unfinished`, `stated_pr`, `find_pr`, `best_pr`,
//! `pr_problem`, `record_pr`, `record_prs`, `mark_no_change`.

use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use super::config::DispatchConfig;
use super::model::{ForgePr, FoundPr, PrHow, PrRecord, PrState, Run, RunStatus, Runs};
use super::runs::{latest, lineage};

static REPORTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)(?:PR|pull request) #?\d+|/pull/\d+|#\d{3,}\b|^PR: (?:#\d+|none)\s*$")
        .unwrap()
});
static STATED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^PR: (?:#(\d+)|(none))[ \t\r]*$").unwrap());
static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:#|/pull/)(\d+)").unwrap());

/// Whether the log lacks a final report in its last `report_window` non-noise lines.
///
/// This is the fallback; a structured provider result (or the `PR:` line) is preferred.
pub fn unfinished(log: &str, cfg: &DispatchConfig) -> bool {
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !cfg.noise_prefixes.iter().any(|p| l.starts_with(p.as_str())))
        .collect();
    let start = lines.len().saturating_sub(cfg.report_window);
    !REPORTED.is_match(&lines[start..].join("\n"))
}

/// What the agent stated in its last `PR: #<n>` / `PR: none` line.
///
/// `Some(Some(n))`, `Some(None)` for `none`, `None` when it wrote no such line.
pub fn stated_pr(log: &str) -> Option<Option<u64>> {
    let caps = STATED.captures_iter(log).last()?;
    if caps.get(2).is_some() {
        return Some(None);
    }
    caps.get(1).and_then(|m| m.as_str().parse().ok()).map(Some)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// Whether `branch` appears in `log` not adjacent to word, dot or dash characters.
pub fn mentions_branch(log: &str, branch: &str) -> bool {
    if branch.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(i) = log[from..].find(branch) {
        let at = from + i;
        let end = at + branch.len();
        let before = log[..at].chars().next_back();
        let after = log[end..].chars().next();
        if !before.is_some_and(is_word) && !after.is_some_and(is_word) {
            return true;
        }
        from = at + branch.chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn names(log: &str, number: u64) -> bool {
    NAMED
        .captures_iter(log)
        .any(|c| c[1].parse::<u64>().ok() == Some(number))
}

/// Guess a run's PR from its worktree branches and its log (owner-filtered, OPEN before MERGED).
pub fn find_pr<'a>(
    branches: &[String],
    log: &str,
    prs: &'a [ForgePr],
    started: i64,
    owner: &str,
) -> Option<&'a ForgePr> {
    let candidates: Vec<&ForgePr> = prs
        .iter()
        .filter(|p| owner.is_empty() || p.owner == owner)
        .filter(|p| matches!(p.state, PrState::Open | PrState::Merged))
        .collect();
    let mut pool: Vec<&ForgePr> = candidates
        .iter()
        .copied()
        .filter(|p| branches.contains(&p.branch) || mentions_branch(log, &p.branch))
        .collect();
    if pool.is_empty() {
        pool = candidates
            .iter()
            .copied()
            .filter(|p| p.updated >= started && names(log, p.number))
            .collect();
    }
    for state in [PrState::Open, PrState::Merged] {
        let in_state: Vec<&ForgePr> = pool.iter().copied().filter(|p| p.state == state).collect();
        match in_state.len() {
            0 => continue,
            1 => return Some(in_state[0]),
            _ => {
                let named: Vec<&ForgePr> = in_state
                    .into_iter()
                    .filter(|p| names(log, p.number))
                    .collect();
                return if named.len() == 1 {
                    Some(named[0])
                } else {
                    None
                };
            }
        }
    }
    None
}

/// Attribute a finished run's PR. `None` means "retry later" (a stated PR whose branch is unknown).
pub fn record_pr(
    run: &Run,
    prs: &[ForgePr],
    branches_by_pr: &BTreeMap<u64, String>,
    cfg: &DispatchConfig,
) -> Option<PrRecord> {
    match stated_pr(&run.summary) {
        Some(Some(n)) => {
            let branch = branches_by_pr
                .get(&n)
                .cloned()
                .or_else(|| prs.iter().find(|p| p.number == n).map(|p| p.branch.clone()))?;
            Some(PrRecord::Found(FoundPr {
                number: n,
                branch,
                how: PrHow::Stated,
            }))
        }
        Some(None) => Some(PrRecord::NoPr { stated: true }),
        None => Some(
            match find_pr(
                &run.branches,
                &run.summary,
                prs,
                run.started,
                &cfg.fork_owner,
            ) {
                Some(p) => PrRecord::Found(FoundPr {
                    number: p.number,
                    branch: p.branch.clone(),
                    how: PrHow::Guessed,
                }),
                None => PrRecord::NoPr { stated: false },
            },
        ),
    }
}

/// Attribute PRs for all `Done` runs that have `PrRecord::NotLooked`, returning the event lines.
pub fn record_prs(
    runs: &mut Runs,
    prs: &[ForgePr],
    branches_by_pr: &BTreeMap<u64, String>,
    cfg: &DispatchConfig,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (key, run) in runs.iter_mut() {
        if run.status != RunStatus::Done || run.pr != PrRecord::NotLooked {
            continue;
        }
        let Some(rec) = record_pr(run, prs, branches_by_pr, cfg) else {
            continue;
        };
        let line = match &rec {
            PrRecord::Found(pr) => format!("{key} -> #{} ({})", pr.number, pr.how.as_str()),
            PrRecord::NoPr { stated: true } => format!("{key} -> stated no PR"),
            PrRecord::NoPr { stated: false } | PrRecord::NotLooked => {
                format!("{key} -> no PR found")
            }
        };
        run.pr = rec;
        lines.push(line);
    }
    lines
}

/// The most trusted PR on any run of the task (`Attached > Stated > Guessed`, newest of equals).
pub fn best_pr(base: &str, runs: &Runs) -> Option<FoundPr> {
    let mut best: Option<&FoundPr> = None;
    for (_, run) in lineage(base, runs).into_iter().rev() {
        if let PrRecord::Found(pr) = &run.pr {
            if best.is_none_or(|b| pr.how > b.how) {
                best = Some(pr);
            }
        }
    }
    best.cloned()
}

/// Alias matching the reference `runs.recorded_pr`.
pub fn recorded_pr(base: &str, runs: &Runs) -> Option<FoundPr> {
    best_pr(base, runs)
}

/// Why the PR of a finished task cannot be built on: `(sentence, a person decides)`, or `None`.
pub fn pr_problem(base: &str, runs: &Runs, cfg: &DispatchConfig) -> Option<(String, bool)> {
    let tool = &cfg.tool;
    if let Some(pr) = best_pr(base, runs) {
        if pr.how == PrHow::Guessed {
            let n = pr.number;
            return Some((
                format!(
                    "#{n} for {base} was guessed from logs, not stated; check it, then `{tool} attach {base} {n}`"
                ),
                true,
            ));
        }
        return None;
    }
    let chain = lineage(base, runs);
    let stated_none = chain
        .iter()
        .any(|(_, r)| r.pr == PrRecord::NoPr { stated: true });
    let no_change = chain.iter().any(|(_, r)| r.no_change);
    if stated_none && !no_change {
        return Some((
            format!("{base} stated `PR: none`; a person decides ({tool} attach {base} <pr>)"),
            true,
        ));
    }
    if chain.iter().any(|(_, r)| r.pr != PrRecord::NotLooked) {
        return None;
    }
    Some((format!("looking for the PR of {base}"), false))
}

/// Records that a task which stated `PR: none` left nothing in its worktree, unblocking the area behind it (R14).
pub fn mark_no_change(base: &str, runs: &mut Runs) -> bool {
    let Some((key, _)) = latest(base, runs) else {
        return false;
    };
    if let Some(run) = runs.get_mut(&key) {
        run.no_change = true;
        return true;
    }
    false
}
