//! The result contract of one merge-check run, and the pure readings of it:
//! classification (R25), not-run (R26), proof completeness (R27), failed areas and
//! the strict summary line (R28), and the blame excerpt (R22).

use super::config::LogPatterns;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The merge-check could not fetch main, check out, or enforce its budget.
pub const RC_SETUP_FAILED: i32 = 90;
/// A migration failed.
pub const RC_MIGRATION_FAILED: i32 = 91;
/// Nothing merged: every member was skipped. Not a "not run".
pub const RC_NOTHING_MERGED: i32 = 93;
/// An area could not start (no CPUs, service never healthy).
pub const RC_NOT_STARTED: i32 = 97;
/// An area was stopped because another failed first. Proves nothing.
pub const RC_STOPPED: i32 = 98;
/// The batch wall clock expired.
pub const RC_WALL_CLOCK: i32 = 124;

/// A batch member: a PR at an exact head.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Member {
    pub pr: u64,
    pub sha: String,
}

/// Why a member was not merged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SkipReason {
    FetchFailed,
    HeadMoved,
    Conflict { files: Vec<String> },
}

impl SkipReason {
    pub fn describe(&self) -> String {
        match self {
            SkipReason::FetchFailed => "its head could not be fetched".to_string(),
            SkipReason::HeadMoved => "its head moved after the batch was formed".to_string(),
            SkipReason::Conflict { files } => format!("CONFLICT {}", files.join(" ")),
        }
    }
}

/// Structured per-area result (R39). The source of truth for proof, not the log text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AreaResult {
    pub rc: i32,
    #[serde(default)]
    pub wall_s: u64,
    #[serde(default)]
    pub cpu_s: f64,
    #[serde(default)]
    pub cache_hits: u64,
    #[serde(default)]
    pub cache_misses: u64,
    #[serde(default)]
    pub tests_passed: u64,
}

/// What a merge-check run reports (R33).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CheckResult {
    pub rc: i32,
    /// Set when the runner left no result: the process's own exit status.
    #[serde(default)]
    pub process_rc: Option<i32>,
    #[serde(default)]
    pub merged: Vec<u64>,
    /// Skipped PR numbers (when reported as a plain list or alongside `skips`).
    #[serde(default)]
    pub skipped: Vec<u64>,
    #[serde(default)]
    pub skips: BTreeMap<u64, SkipReason>,
    /// The runner's closing `passed:`/`failed:` line.
    #[serde(default)]
    pub line: String,
    /// Tree of the merged checkout, recorded before any area ran.
    #[serde(default)]
    pub tree: Option<String>,
    #[serde(default)]
    pub stats: BTreeMap<String, AreaResult>,
    /// Log text (or its tail) the decisions read.
    #[serde(default)]
    pub log: String,
}

impl CheckResult {
    /// All PR numbers skipped in this run, combining `skipped` and `skips.keys()` in order.
    pub fn all_skipped(&self) -> Vec<u64> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for &n in &self.skipped {
            if seen.insert(n) {
                out.push(n);
            }
        }
        for &n in self.skips.keys() {
            if seen.insert(n) {
                out.push(n);
            }
        }
        out
    }
}

/// Code or machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Classification {
    Code,
    Infrastructure,
}

fn compile_all(pats: &[String]) -> Vec<Option<Regex>> {
    pats.iter().map(|p| Regex::new(p).ok()).collect()
}

/// Named failed tests, sorted and deduped. Patterns with no capture group are ignored.
pub fn failed_tests_with(log: &str, pats: &LogPatterns) -> Vec<String> {
    let mut out = BTreeSet::new();
    for re in compile_all(&pats.failed_test).into_iter().flatten() {
        for c in re.captures_iter(log) {
            if let Some(m) = c.get(1) {
                out.insert(m.as_str().trim().to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// Whether the log shows a property-test counterexample: never a flake, never bisected.
pub fn has_counterexample(log: &str, pats: &LogPatterns) -> bool {
    !pats.counterexample.is_empty() && log.contains(&pats.counterexample)
}

/// R25: a named failed test or a counterexample is the code, whatever noise sits beside it.
/// Otherwise any infrastructure phrase means the machine. An infrastructure pattern that is
/// not a valid regex is matched as a plain substring.
pub fn classify_with(log: &str, pats: &LogPatterns) -> Classification {
    if !failed_tests_with(log, pats).is_empty() || has_counterexample(log, pats) {
        return Classification::Code;
    }
    let infra = pats.infrastructure.iter().any(|p| match Regex::new(p) {
        Ok(re) => re.is_match(log),
        Err(_) => log.contains(p.as_str()),
    });
    if infra {
        Classification::Infrastructure
    } else {
        Classification::Code
    }
}

/// R26: the run did not happen in any sense that proves or blames.
pub fn is_not_run(r: &CheckResult, pats: &LogPatterns) -> bool {
    r.process_rc.is_some()
        || (r.rc >= RC_SETUP_FAILED && r.rc != RC_NOTHING_MERGED)
        || classify_with(&r.log, pats) == Classification::Infrastructure
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
                if !(d.is_ascii_digit() || d == ';') {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// R28: the last line that is exactly `passed:|failed:` followed only by known area names,
/// after stripping colour codes. A test printing `failed: x` cannot stand in for it.
pub fn summary_line_for(log: &str, areas: &[String]) -> String {
    let known: BTreeSet<&str> = areas.iter().map(String::as_str).collect();
    log.lines()
        .rev()
        .map(strip_ansi)
        .find(|l| {
            let Some((head, rest)) = l.split_once(": ") else {
                return false;
            };
            (head == "passed" || head == "failed")
                && !rest.is_empty()
                && rest.split(' ').all(|a| known.contains(a))
        })
        .unwrap_or_default()
}

/// R28 (better): the areas that failed. Structured stats win (rc != 0 and not stopped);
/// otherwise the areas the summary line names; otherwise all of them, since a run that
/// could not say what failed proves nothing about any area.
pub fn failed_areas_of(r: &CheckResult, areas: &[String]) -> Vec<String> {
    let from_stats: Vec<String> = areas
        .iter()
        .filter(|a| {
            r.stats
                .get(*a)
                .is_some_and(|s| s.rc != 0 && s.rc != RC_STOPPED)
        })
        .cloned()
        .collect();
    if !from_stats.is_empty() {
        return from_stats;
    }
    super::logs::failed_areas(&r.line, areas)
}

/// R19 step 7: the areas a rerun checks. A failed gate reruns everything; otherwise the
/// failed areas plus those stopped before they could prove anything.
pub fn retry_areas(r: &CheckResult, areas: &[String], gate: Option<&str>) -> Vec<String> {
    let failed = failed_areas_of(r, areas);
    if gate.is_some_and(|g| failed.iter().any(|a| a == g)) {
        return areas.to_vec();
    }
    let mut out: BTreeSet<String> = failed.into_iter().collect();
    for (a, s) in &r.stats {
        if s.rc == RC_STOPPED && areas.contains(a) {
            out.insert(a.clone());
        }
    }
    out.into_iter().collect()
}

/// R27: the areas with a structured success, each mapped to the run that proved it.
pub fn proved_areas(r: &CheckResult, run: &str) -> BTreeMap<String, String> {
    r.stats
        .iter()
        .filter(|(_, s)| s.rc == 0)
        .map(|(a, _)| (a.clone(), run.to_string()))
        .collect()
}

/// R19 step 9: proofs of a rerun, plus the first run's successes only when both runs
/// checked the same tree. A missing tree on either side never merges.
pub fn merge_proofs(
    first: &CheckResult,
    first_run: &str,
    again: &CheckResult,
    again_run: &str,
) -> BTreeMap<String, String> {
    let mut out = proved_areas(again, again_run);
    if first.tree.is_some() && first.tree == again.tree {
        for (a, run) in proved_areas(first, first_run) {
            out.entry(a).or_insert(run);
        }
    }
    out
}

/// Areas of `want` with no proof, in order.
pub fn missing_proof(want: &[String], proved: &BTreeMap<String, String>) -> Vec<String> {
    want.iter()
        .filter(|a| !proved.contains_key(*a))
        .cloned()
        .collect()
}

/// R22: at most `excerpt_lines` lines that carry a failure marker.
pub fn excerpt(log: &str, pats: &LogPatterns) -> String {
    log.lines()
        .filter(|l| pats.excerpt_markers.iter().any(|m| l.contains(m.as_str())))
        .take(pats.excerpt_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

/// R23: human description of skipped PRs (`#2 (CONFLICT crates/a.rs), #3 (its head could not be fetched)`).
pub fn describe_skips(skipped: &[u64], skips: &BTreeMap<u64, SkipReason>) -> String {
    let mut parts = Vec::new();
    for &p in skipped {
        if let Some(why) = skips.get(&p) {
            parts.push(format!("#{p} ({})", why.describe()));
        } else {
            parts.push(format!("#{p}"));
        }
    }
    parts.join(", ")
}

/// R23: for each PR skipped due to conflict (and not fetch-failed / head-moved), record the
/// conflicting files when its head is known and no hand has already been dispatched for that head.
pub fn skip_rebases(
    skipped: &[u64],
    skips: &BTreeMap<u64, SkipReason>,
    heads: &BTreeMap<u64, String>,
    existing_hands: &BTreeMap<u64, String>,
) -> BTreeMap<u64, Vec<String>> {
    let mut out = BTreeMap::new();
    for &p in skipped {
        let Some(head) = heads.get(&p) else {
            continue;
        };
        if head.is_empty() {
            continue;
        }
        match skips.get(&p) {
            Some(SkipReason::FetchFailed) | Some(SkipReason::HeadMoved) => continue,
            Some(SkipReason::Conflict { files }) => {
                if existing_hands.get(&p) != Some(head) {
                    out.insert(p, files.clone());
                }
            }
            None => {
                if existing_hands.get(&p) != Some(head) {
                    out.insert(p, Vec::new());
                }
            }
        }
    }
    out
}
