//! Combined train PR: eligibility, CI isolation and proof gate, merge order, conflict
//! settlement, compile-breaker bisection, and the words of the PR it opens.

use super::config::{short, LandConfig};
use super::train::Sha;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Shown in the PR body and the letter of a train that changes how checks run.
pub const CI_NOTICE: &str = "This changes how checks run, so it is batched alone. Once it merges, \
one real run of main's ci must pass before any other change lands.";

/// A PR the train may take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub number: u64,
    pub files: Vec<String>,
}

impl Candidate {
    pub fn new(number: u64, files: &[&str]) -> Self {
        Self {
            number,
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// What eligibility needs to know about an open PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenPr {
    pub number: u64,
    pub branch: String,
    pub head: Sha,
}

/// Summary of a batch for computing [`green_at_head`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchSummary {
    pub status: String,
    pub members: Vec<(u64, Sha)>,
}

/// The `(pr, head)` pairs where a batch of that single PR alone is `"green"` (or `"passed"`).
pub fn green_at_head(batches: &[BatchSummary]) -> BTreeSet<(u64, Sha)> {
    batches
        .iter()
        .filter(|b| (b.status == "green" || b.status == "passed") && b.members.len() == 1)
        .map(|b| b.members[0].clone())
        .collect()
}

/// Open PRs whose task is done (no running agent owns them), not held, not a train, and whose
/// own single-PR batch is green at the current head. Ascending.
pub fn eligible(
    open: &[OpenPr],
    running: &BTreeSet<u64>,
    held: &BTreeSet<u64>,
    green_single: &BTreeSet<(u64, Sha)>,
    cfg: &LandConfig,
) -> Vec<u64> {
    let mut out: Vec<u64> = open
        .iter()
        .filter(|p| {
            !cfg.is_train_branch(&p.branch)
                && !running.contains(&p.number)
                && !held.contains(&p.number)
                && green_single.contains(&(p.number, p.head.clone()))
        })
        .map(|p| p.number)
        .collect();
    out.sort_unstable();
    out
}

/// A change to how checks run travels alone: the lowest-numbered such candidate, else all.
pub fn solo_ci(candidates: Vec<Candidate>, cfg: &LandConfig) -> Vec<Candidate> {
    let lowest_ci = candidates
        .iter()
        .filter(|c| cfg.touches_ci(&c.files))
        .min_by_key(|c| c.number)
        .cloned();
    match lowest_ci {
        Some(c) => vec![c],
        None => candidates,
    }
}

/// Main's latest CI verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MainCi {
    pub sha: Sha,
    pub ok: bool,
}

/// Why no train may form: the newest pipeline commit `ci_sha` has no passing main CI run at or
/// after it. `None` when the train may form.
pub fn ci_proof_pending(
    ci_sha: Option<&str>,
    main_ci: &MainCi,
    is_ancestor: impl Fn(&str, &str) -> bool,
) -> Option<String> {
    let ci_sha = ci_sha.filter(|s| !s.is_empty())?;
    if main_ci.ok && !main_ci.sha.is_empty() && is_ancestor(ci_sha, &main_ci.sha) {
        return None;
    }
    Some(format!(
        "{} changed how ci runs and no main ci run has passed since; nothing else lands until one does",
        short(ci_sha, 8)
    ))
}

fn screens(c: &Candidate, cfg: &LandConfig) -> BTreeSet<String> {
    c.files
        .iter()
        .filter(|f| cfg.is_ui(f) && cfg.generated_area(f).is_none())
        .map(|f| f.rsplit_once('/').map_or("", |(d, _)| d).to_string())
        .collect()
}

/// Merge order: no-UI first, smaller before larger, then number; a PR sharing a screen with
/// those just placed follows directly (the screen set grows as PRs are pulled forward).
pub fn order(candidates: &[Candidate], cfg: &LandConfig) -> Vec<u64> {
    let mut ranked: Vec<&Candidate> = candidates.iter().collect();
    ranked.sort_by_key(|c| {
        (
            c.files.iter().any(|f| cfg.is_ui(f)),
            c.files.len(),
            c.number,
        )
    });
    let mut placed = Vec::new();
    while !ranked.is_empty() {
        let first = ranked.remove(0);
        placed.push(first.number);
        let mut seen = screens(first, cfg);
        while !seen.is_empty() {
            let Some(i) = ranked
                .iter()
                .position(|c| !screens(c, cfg).is_disjoint(&seen))
            else {
                break;
            };
            let next = ranked.remove(i);
            placed.push(next.number);
            seen.extend(screens(next, cfg));
        }
    }
    placed
}

/// `--max N` truncation (0 = no limit).
pub fn truncate_max(ordered: Vec<u64>, max: usize) -> Vec<u64> {
    if max == 0 {
        ordered
    } else {
        ordered.into_iter().take(max).collect()
    }
}

/// What to do with one conflicted merge.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Settlement {
    /// Take the later PR's copy.
    pub take: Vec<String>,
    /// Then run the path's generator on the merged tree.
    pub regenerate: Vec<String>,
    /// Needs a person: the PR is left out.
    pub by_hand: Vec<String>,
}

/// Generator outputs and generated screenshots are resolved; anything else needs a person.
pub fn settle(conflicted: &[String], cfg: &LandConfig) -> Settlement {
    let mut s = Settlement::default();
    for path in conflicted {
        if cfg.generators.contains_key(path) {
            s.take.push(path.clone());
            s.regenerate.push(path.clone());
        } else if path.ends_with(".png") && cfg.generated_area(path).is_some() {
            s.take.push(path.clone());
        } else {
            s.by_hand.push(path.clone());
        }
    }
    s
}

/// Body lines for an automatic resolution.
pub fn resolution_lines(n: u64, s: &Settlement) -> Vec<(u64, String)> {
    let mut rows = Vec::new();
    let shots = s.take.iter().filter(|p| !s.regenerate.contains(p)).count();
    if shots > 0 {
        rows.push((
            n,
            format!("{shots} screenshot(s) conflicted; the later copy was kept for the batch to refresh"),
        ));
    }
    for p in &s.regenerate {
        rows.push((
            n,
            format!(
                "{p} conflicted; the later copy was kept and its generator ran on the merged tree"
            ),
        ));
    }
    rows
}

/// Drops PRs whose merge breaks the build, one at a time. `compile_error(prefix)` is the error
/// of merging exactly `prefix`, or `None`. The shortest failing prefix's last PR is blamed. A
/// tree that compiles is built exactly once.
pub fn drop_breakers(
    numbers: &[u64],
    mut compile_error: impl FnMut(&[u64]) -> Option<String>,
) -> (Vec<u64>, Vec<(u64, String)>) {
    let mut kept = numbers.to_vec();
    let mut dropped = Vec::new();
    while !kept.is_empty() {
        let Some(mut error) = compile_error(&kept) else {
            break;
        };
        let (mut low, mut high) = (1usize, kept.len());
        while low < high {
            let mid = (low + high) / 2;
            match compile_error(&kept[..mid]) {
                Some(found) => {
                    high = mid;
                    error = found;
                }
                None => low = mid + 1,
            }
        }
        dropped.push((kept.remove(high - 1), error));
    }
    (kept, dropped)
}

/// Everyone left out of the final train: the probe's left-outs, the final assembly's
/// left-outs, and any compiled member missing from the final merge (with no files).
pub fn left_out(
    probe_left: &[(u64, Vec<String>)],
    compiled: &[u64],
    final_merged: &[u64],
    final_left: &[(u64, Vec<String>)],
) -> Vec<(u64, Vec<String>)> {
    let mut out: Vec<(u64, Vec<String>)> = Vec::new();
    for (n, files) in probe_left.iter().chain(final_left.iter()) {
        if let Some(existing) = out.iter_mut().find(|(m, _)| *m == *n) {
            existing.1 = files.clone();
        } else {
            out.push((*n, files.clone()));
        }
    }
    for &n in compiled {
        if !final_merged.contains(&n) && !out.iter().any(|(m, _)| *m == n) {
            out.push((n, Vec::new()));
        }
    }
    out
}

/// The reported error when a compile step on the merged tree fails (`R-AS-7`):
/// the structured batch excerpt if non-empty, else the last `max_lines` (default 12) lines.
pub fn compile_error_excerpt(log: &str, batch_excerpt: Option<&str>, max_lines: usize) -> String {
    if let Some(ex) = batch_excerpt.map(str::trim).filter(|s| !s.is_empty()) {
        return ex.to_string();
    }
    let lines: Vec<&str> = log.lines().collect();
    let keep = max_lines.max(1);
    let start = lines.len().saturating_sub(keep);
    lines[start..].join("\n")
}

/// Message printed when no candidate compiles or all candidates conflict (`R-AS-8`).
pub fn nothing_to_land_reason(dropped: &[(u64, String)]) -> String {
    if dropped.is_empty() {
        "nothing to land: every change conflicts".into()
    } else {
        let parts: Vec<String> = dropped
            .iter()
            .map(|(n, _)| format!("#{n} does not compile"))
            .collect();
        format!("nothing to land: {}", parts.join("; "))
    }
}

/// Why a member is out. Without files it must never say "conflict".
pub fn left_out_reason(files: &[String]) -> String {
    if files.is_empty() {
        "it compiled in the trial merge but was missing from the final merge".into()
    } else {
        format!("its changes conflict in {}", files.join(", "))
    }
}

/// `"{count} open change{s} land together"`.
pub fn title(count: usize) -> String {
    format!(
        "{count} open change{} land together",
        if count == 1 { "" } else { "s" }
    )
}

/// The screens named by changed shot files, `"<label>: <stem>"`, sorted, unique.
pub fn shot_screens(changed: &[String], cfg: &LandConfig) -> Vec<String> {
    let mut out = BTreeSet::new();
    for path in changed.iter().filter(|p| p.ends_with(".png")) {
        for (prefix, label) in &cfg.shot_dirs {
            if let Some(rest) = path.strip_prefix(prefix.as_str()) {
                let stem = rest.strip_suffix(".png").unwrap_or(rest);
                out.insert(format!("{label}: {stem}"));
            }
        }
    }
    out.into_iter().collect()
}

/// Inputs to the train PR body.
#[derive(Debug, Clone, Default)]
pub struct BodyParts<'a> {
    pub members: &'a [u64],
    pub resolutions: &'a [(u64, String)],
    pub left_out: &'a [(u64, String)],
    pub dropped: &'a [(u64, String)],
    pub screens: &'a [String],
    pub unverified: &'a [String],
}

/// The PR description: who is in, every automatic resolution, who is out and why, and what
/// nobody has checked.
pub fn body(p: &BodyParts<'_>) -> String {
    fn section(out: &mut Vec<String>, heading: &str, rows: &[(u64, String)]) {
        out.push(heading.into());
        out.push(String::new());
        if rows.is_empty() {
            out.push("- none".into());
        }
        out.extend(rows.iter().map(|(n, why)| format!("- #{n}: {why}")));
        out.push(String::new());
    }
    let mut out = vec!["Changes, in merge order:".to_string(), String::new()];
    out.extend(
        p.members
            .iter()
            .enumerate()
            .map(|(i, n)| format!("{}. #{n}", i + 1)),
    );
    out.push(String::new());
    section(&mut out, "Resolved without a person:", p.resolutions);
    section(
        &mut out,
        "Left out, each with a rebase queued behind this pull request:",
        p.left_out,
    );
    section(
        &mut out,
        "Dropped because the merged tree did not compile:",
        p.dropped,
    );
    out.push("Screenshots are refreshed by the batch on the merged tree:".into());
    out.push(String::new());
    if p.screens.is_empty() {
        out.push("- all affected screens".into());
    }
    out.extend(p.screens.iter().map(|s| format!("- {s}")));
    out.push(String::new());
    out.push(
        "Not verified here: tests. The merge check that follows this pull request runs every \
         area over the result; this change set was only merged and compiled."
            .into(),
    );
    out.extend(p.unverified.iter().map(|l| format!("Also: {l}")));
    format!("{}\n", out.join("\n").trim_end())
}

/// First line of a compile error, for the body.
pub fn dropped_line(error: &str) -> String {
    error
        .lines()
        .find(|l| !l.trim().is_empty())
        .map_or_else(|| "the build failed".into(), |l| l.trim().to_string())
}

/// The rebase brief for a left-out member.
pub fn rebase_brief(n: u64, train_pr: u64, files: &[String]) -> String {
    if files.is_empty() {
        return format!(
            "PR #{n} was left out of #{train_pr}: it compiled in the trial merge but was missing \
             from the final merge. Rebase it onto upstream/main as plain commits, check that it \
             still merges cleanly with main, regenerate any screenshots the change alters, run \
             the checks for the areas it touches, and push with --force-with-lease."
        );
    }
    format!(
        "{} It conflicted while the changes for #{train_pr} were put together.",
        super::rebase::hands_task(n, &super::rebase::Hands::files(files))
    )
}

/// The fix brief for a compile breaker, embedding the full error.
pub fn fix_brief(n: u64, train_pr: u64, error: &str) -> String {
    format!(
        "PR #{n} was left out of #{train_pr} because merging it made the build fail:\n\n{error}\n\n\
         Fix it on its own branch so it compiles on top of main, and push."
    )
}

/// `<prefix><date>-<n>` for the first n above any already used for that date.
pub fn next_branch(prefix: &str, date: &str, existing: &[String]) -> String {
    let head = format!("{prefix}{date}-");
    let taken = existing
        .iter()
        .filter_map(|b| b.strip_prefix(head.as_str()))
        .filter(|rest| !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()))
        .filter_map(|rest| rest.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("{head}{}", taken + 1)
}

/// A task the land concern queues (replaces any task with the same key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandTask {
    pub key: String,
    pub kind: String,
    pub after: Vec<String>,
    pub model: String,
    pub pr: u64,
    pub brief: String,
}

/// Rebase tasks for left-out members (queued behind the train) and fix tasks for breakers.
pub fn train_tasks(
    train_pr: u64,
    left: &[(u64, Vec<String>)],
    dropped: &[(u64, String)],
    cfg: &LandConfig,
) -> Vec<LandTask> {
    let mut out: Vec<LandTask> = left
        .iter()
        .map(|(n, files)| LandTask {
            key: format!("rebase-{n}"),
            kind: "rebase".into(),
            after: vec![format!("pr:{train_pr}")],
            model: cfg.rebase_model.clone(),
            pr: *n,
            brief: rebase_brief(*n, train_pr, files),
        })
        .collect();
    out.extend(dropped.iter().map(|(n, err)| LandTask {
        key: format!("fix-compile-{n}"),
        kind: "fix-compile".into(),
        after: Vec::new(),
        model: cfg.fix_model.clone(),
        pr: *n,
        brief: fix_brief(*n, train_pr, err),
    }));
    out
}

/// One fix task per failing area of a train's red verdict, once per failure sha.
pub fn red_fix_tasks(
    train_pr: u64,
    failure_sha: &str,
    areas: &[String],
    why: &str,
    log: &str,
    fixed_for: Option<&str>,
    cfg: &LandConfig,
) -> Vec<LandTask> {
    if fixed_for == Some(failure_sha) {
        return Vec::new();
    }
    areas
        .iter()
        .map(|area| LandTask {
            key: format!("train-fix-{train_pr}-{area}"),
            kind: "train-fix".into(),
            after: Vec::new(),
            model: cfg.fix_model.clone(),
            pr: train_pr,
            brief: format!(
                "The merge check of train #{train_pr} failed in {area}. Fix it on the train's \
                 branch (or on the change that caused it) and push.\n\n{why}\n\n{log}"
            ),
        })
        .collect()
}

/// The record fields a freshly opened combined train carries (helper for hands).
pub fn source_heads(members: &[u64], heads: &BTreeMap<u64, Sha>) -> BTreeMap<u64, Sha> {
    members
        .iter()
        .filter_map(|n| heads.get(n).map(|h| (*n, h.clone())))
        .collect()
}
