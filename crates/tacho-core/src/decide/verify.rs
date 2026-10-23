//! PR stack ancestry, batch planning, prefix bisection, log failure classification
//! (code vs. infrastructure), flake ledger, single-retry fix state machine, and
//! worker proof/PR preflight guardrails.

use crate::decide::dispatch::{already_ran, AgentRun, Hold, PullFacts, TodoItem};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::LazyLock;

/// Maximum PRs combined into a single speculative verification batch.
pub const MAX_BATCH_SIZE: usize = 6;

/// Flake count threshold at which an automated flake-fix task is queued.
pub const FLAKE_TASK_THRESHOLD: u32 = 2;

/// First port in the isolated worktree port range.
pub const PORT_RANGE_START: u16 = 20_000;

/// Last port (exclusive) in the isolated worktree port range.
pub const PORT_RANGE_END: u16 = 32_000;

/// Number of ports reserved per worktree block.
pub const PORT_BLOCK_SIZE: u16 = 20;

/// A planned verification batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchPlan {
    pub id: String,
    pub group: String,
    pub prs: Vec<u64>,
    pub shas: BTreeMap<u64, String>,
    pub areas: Vec<String>,
    #[serde(default)]
    pub because: String,
}

/// A recorded batch execution in fleet state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchRun {
    pub id: String,
    pub group: String,
    pub prs: Vec<u64>,
    #[serde(default)]
    pub shas: BTreeMap<u64, String>,
    #[serde(default)]
    pub main_sha: String,
    #[serde(default)]
    pub areas: Vec<String>,
    /// `"running"`, `"passed"`, `"failed"`, `"bisecting"`
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub tree: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub log: String,
    #[serde(default)]
    pub culprit: Option<u64>,
    #[serde(default)]
    pub skipped: Vec<String>,
    #[serde(default)]
    pub flakes: Vec<String>,
}

/// Record of a PR passing batch verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PassedRecord {
    pub pr: u64,
    pub sha: String,
    pub main_sha: String,
    pub batch_id: String,
    pub passed_at: i64,
    #[serde(default)]
    pub tree: String,
    #[serde(default)]
    pub areas: Vec<String>,
}

/// Record of a PR failing batch verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FailedRecord {
    pub pr: u64,
    pub sha: String,
    pub batch_id: String,
    pub failed_at: i64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub failed_tests: Vec<String>,
    #[serde(default)]
    pub counterexample: Option<String>,
    #[serde(default)]
    pub log: String,
}

/// State of a binary search bisection over a batch's ordered `[pr, sha]` prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bisect {
    /// Ordered `(pr, sha)` members of the batch.
    pub members: Vec<(u64, String)>,
    /// Prefix length Results: `k -> passed` (`k == 0` is `main` alone, `k == i` merges `members[..i]`).
    pub results: BTreeMap<usize, bool>,
}

/// Next step returned by [`next_probe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeStep {
    /// Run a check on `members[..k]` (`k == 0` tests `main` alone).
    Probe(usize),
    /// Exact culprit member `(pr, sha)` identified.
    Culprit((u64, String)),
    /// `main` itself (`k == 0`) failed.
    MainBroken,
}

/// A recorded test flake entry in the persistent flake ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FlakeEntry {
    pub test: String,
    #[serde(default)]
    pub area: String,
    pub count: u32,
    pub first_seen: i64,
    pub last_seen: i64,
    #[serde(default)]
    pub batches: Vec<String>,
    #[serde(default)]
    pub excerpt: String,
    #[serde(default)]
    pub task_key: Option<String>,
}

/// Rendered row for `tacho flakes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlakeRow {
    pub test: String,
    pub area: String,
    pub count: u32,
    pub last_seen: i64,
    pub batches: Vec<String>,
    pub task_key: Option<String>,
    pub excerpt: String,
}

/// Result of the single-retry fix agent state machine for red PRs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FixDecisions {
    pub dispatch: Vec<(u64, FailedRecord)>,
    pub notify: Vec<(u64, String)>,
}

/// A single check command inside a [`ProofSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCheck {
    pub name: String,
    pub cmd: String,
    pub ok: bool,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub output_tail: String,
}

/// Proof artifact written by `tacho prove` to `.tachoproof/summary.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProofSummary {
    pub head: String,
    pub tree: String,
    pub base: String,
    pub ok: bool,
    pub timestamp: i64,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub checks: Vec<ProofCheck>,
    #[serde(default)]
    pub screenshots: Vec<String>,
}

/// Configurable rule mapping file path prefixes or regexes to area names and check commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaRule {
    pub area: String,
    #[serde(default)]
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub checks: Vec<(String, String)>,
    #[serde(default)]
    pub isolate_batch: bool,
    #[serde(default)]
    pub needs_screenshot: bool,
}

/// Build a map `child_pr -> parent_pr` for PRs whose `base` branch is another open PR's `branch`.
pub fn parents(open_prs: &[PullFacts]) -> BTreeMap<u64, u64> {
    let mut by_branch: HashMap<&str, u64> = HashMap::new();
    for p in open_prs {
        if !p.branch.is_empty() {
            by_branch.insert(&p.branch, p.number);
        }
    }
    let mut out = BTreeMap::new();
    for p in open_prs {
        if let Some(&parent) = by_branch.get(p.base.as_str()) {
            if parent != p.number {
                out.insert(p.number, parent);
            }
        }
    }
    out
}

/// Find the root ancestor PR of `pr` (walking `parents` with cycle protection).
pub fn root(pr: u64, parent_map: &BTreeMap<u64, u64>) -> u64 {
    let mut cur = pr;
    let mut seen = BTreeSet::new();
    while let Some(&p) = parent_map.get(&cur) {
        if !seen.insert(cur) {
            break;
        }
        cur = p;
    }
    cur
}

/// Order PR numbers topologically so every parent appears before its children,
/// breaking ties by ascending PR number.
pub fn ordered(prs: &[u64], parent_map: &BTreeMap<u64, u64>) -> Vec<u64> {
    let set: BTreeSet<u64> = prs.iter().copied().collect();
    let mut visited = BTreeSet::new();
    let mut out = Vec::new();

    fn visit(
        n: u64,
        set: &BTreeSet<u64>,
        parent_map: &BTreeMap<u64, u64>,
        visited: &mut BTreeSet<u64>,
        visiting: &mut BTreeSet<u64>,
        out: &mut Vec<u64>,
    ) {
        if visited.contains(&n) || !set.contains(&n) {
            return;
        }
        if !visiting.insert(n) {
            return;
        }
        if let Some(&p) = parent_map.get(&n) {
            if set.contains(&p) {
                visit(p, set, parent_map, visited, visiting, out);
            }
        }
        visiting.remove(&n);
        if visited.insert(n) {
            out.push(n);
        }
    }

    let mut sorted: Vec<u64> = prs.to_vec();
    sorted.sort_unstable();
    let mut visiting = BTreeSet::new();
    for n in sorted {
        visit(n, &set, parent_map, &mut visited, &mut visiting, &mut out);
    }
    out
}

/// Cap an ordered list of PRs to at most `limit` PRs while preserving parent-before-child integrity:
/// - Once `out.len() >= limit`, an item is skipped unless its parent in `parent_map` is already in `out`
///   (in which case the child rides along with its parent even past `limit`).
/// - A child whose parent is in `prs` but was left out of `out` also waits.
pub fn cap(prs: &[u64], parent_map: &BTreeMap<u64, u64>, limit: usize) -> Vec<u64> {
    if limit == 0 || prs.is_empty() {
        return Vec::new();
    }
    let topo = ordered(prs, parent_map);
    let member_set: BTreeSet<u64> = topo.iter().copied().collect();
    let mut out = Vec::new();
    for &n in &topo {
        let parent = parent_map.get(&n).copied();
        if out.len() >= limit && !parent.map(|p| out.contains(&p)).unwrap_or(false) {
            continue;
        }
        if let Some(p) = parent {
            if member_set.contains(&p) && !out.contains(&p) {
                continue;
            }
        }
        out.push(n);
    }
    out
}

/// Map a list of changed file paths to affected area names.
/// Uses `rules` when configured; otherwise infers top-level workspace crates (`crates/<name>`)
/// or falls back to `"ci"` for `.github/`/`scripts/` and `"workspace"` for other files.
pub fn areas_for_files(files: &[String], rules: &[AreaRule]) -> Vec<String> {
    let mut out = BTreeSet::new();
    if !rules.is_empty() {
        for f in files {
            let mut matched = false;
            for r in rules {
                if r.prefixes.iter().any(|p| f.starts_with(p)) {
                    out.insert(r.area.clone());
                    matched = true;
                }
            }
            if !matched {
                if f.starts_with(".github/") || f.starts_with("scripts/ci") {
                    out.insert("ci".to_string());
                } else {
                    out.insert("workspace".to_string());
                }
            }
        }
    } else {
        for f in files {
            if f.starts_with(".github/") || f.starts_with("ci/") {
                out.insert("ci".to_string());
            } else if let Some(rest) = f.strip_prefix("crates/") {
                if let Some(krate) = rest.split('/').next() {
                    out.insert(krate.to_string());
                } else {
                    out.insert("workspace".to_string());
                }
            } else if f.ends_with(".md") || f.starts_with("docs/") {
                out.insert("docs".to_string());
            } else {
                out.insert("workspace".to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// Check whether a PR touches `ci` or any area configured with `isolate_batch: true`.
pub fn is_isolated_ci(files: &[String], rules: &[AreaRule]) -> bool {
    let areas = areas_for_files(files, rules);
    if areas.iter().any(|a| a == "ci") {
        return true;
    }
    rules
        .iter()
        .any(|r| r.isolate_batch && areas.contains(&r.area))
}

/// Decide whether a PR needs batch verification right now.
pub fn needs_check(
    pr: &PullFacts,
    holds: &BTreeMap<String, Hold>,
    passed: &BTreeMap<u64, PassedRecord>,
    failed: &BTreeMap<u64, FailedRecord>,
    running_prs: &BTreeSet<u64>,
) -> bool {
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        return false;
    }
    if pr.head.trim().is_empty() {
        return false;
    }
    if pr.mergeable.eq_ignore_ascii_case("CONFLICTING") {
        return false;
    }
    if holds.contains_key(&format!("pr:{}", pr.number)) {
        return false;
    }
    if running_prs.contains(&pr.number) {
        return false;
    }
    if let Some(p) = passed.get(&pr.number) {
        if p.sha == pr.head {
            return false;
        }
    }
    if let Some(f) = failed.get(&pr.number) {
        if f.sha == pr.head {
            return false;
        }
    }
    true
}

/// Find a prior passed batch with identical ordered `(pr, sha)` members on `main_sha`
/// covering all `needed_areas`.
pub fn reusable_batch<'a>(
    history: &'a [BatchRun],
    prs: &[u64],
    shas: &BTreeMap<u64, String>,
    main_sha: &str,
    needed_areas: &[String],
) -> Option<&'a BatchRun> {
    for b in history.iter().rev() {
        if b.status != "passed" {
            continue;
        }
        if b.main_sha != main_sha || b.prs != prs || &b.shas != shas {
            continue;
        }
        let covered: BTreeSet<&str> = b.areas.iter().map(String::as_str).collect();
        if needed_areas.iter().all(|a| covered.contains(a.as_str())) {
            return Some(b);
        }
    }
    None
}

/// Generate the next sequential batch identifier (`b001`, `b002`, ...).
pub fn new_batch_id(existing: &[BatchRun]) -> String {
    let mut max_n = 0u64;
    for b in existing {
        if let Some(rest) = b.id.strip_prefix('b') {
            if let Ok(n) = rest.parse::<u64>() {
                max_n = max_n.max(n);
            }
        }
    }
    format!("b{:03}", max_n + 1)
}

/// Plan verification batches for open PRs that need checking.
///
/// Rules:
/// - Groups PRs by stack/area group (`ci` changes are isolated into solo `ci-pr<n>` groups).
/// - At most one batch per group is planned when that group does not already have a running batch.
/// - Within a group, parents are ordered before children, capped at `MAX_BATCH_SIZE` taking whole stacks.
#[allow(clippy::too_many_arguments)]
pub fn batches(
    open_prs: &[PullFacts],
    holds: &BTreeMap<String, Hold>,
    passed: &BTreeMap<u64, PassedRecord>,
    failed: &BTreeMap<u64, FailedRecord>,
    existing_batches: &[BatchRun],
    rules: &[AreaRule],
    limit: usize,
) -> Vec<BatchPlan> {
    let running_prs: BTreeSet<u64> = existing_batches
        .iter()
        .filter(|b| b.status == "running" || b.status == "bisecting")
        .flat_map(|b| b.prs.iter().copied())
        .collect();
    let running_groups: BTreeSet<&str> = existing_batches
        .iter()
        .filter(|b| b.status == "running" || b.status == "bisecting")
        .map(|b| b.group.as_str())
        .collect();

    let parent_map = parents(open_prs);
    let by_num: BTreeMap<u64, &PullFacts> = open_prs.iter().map(|p| (p.number, p)).collect();

    // Candidates that need verification (plus any unpassed ancestor in their stack).
    let mut candidates: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for pr in open_prs {
        if !needs_check(pr, holds, passed, failed, &running_prs) {
            continue;
        }
        let group = if is_isolated_ci(&pr.files, rules) {
            format!("ci-pr{}", pr.number)
        } else {
            let r = root(pr.number, &parent_map);
            let root_files = by_num
                .get(&r)
                .map(|p| p.files.as_slice())
                .unwrap_or(&pr.files);
            let areas = areas_for_files(root_files, rules);
            areas
                .first()
                .cloned()
                .unwrap_or_else(|| "default".to_string())
        };
        if running_groups.contains(group.as_str())
            || holds.contains_key(&group)
            || holds.contains_key(&format!("group:{group}"))
            || holds.contains_key(&format!("area:{group}"))
        {
            continue;
        }
        let entry = candidates.entry(group).or_default();
        // Ensure any unmerged ancestor in open_prs is included in the stack if not yet passed at current head
        let mut cur = pr.number;
        let mut chain = vec![cur];
        let mut seen = BTreeSet::new();
        while let Some(&p) = parent_map.get(&cur) {
            if !seen.insert(cur) {
                break;
            }
            if let Some(p_facts) = by_num.get(&p) {
                if needs_check(p_facts, holds, passed, failed, &running_prs) {
                    chain.push(p);
                }
            }
            cur = p;
        }
        for n in chain {
            if !entry.contains(&n) {
                entry.push(n);
            }
        }
    }

    let mut next_num = existing_batches
        .iter()
        .filter_map(|b| b.id.strip_prefix('b').and_then(|s| s.parse::<u64>().ok()))
        .max()
        .unwrap_or(0);

    let mut plans = Vec::new();
    for (group, pr_list) in candidates {
        let capped = cap(&pr_list, &parent_map, limit.max(1));
        if capped.is_empty() {
            continue;
        }
        let mut shas = BTreeMap::new();
        let mut area_set = BTreeSet::new();
        for &n in &capped {
            if let Some(p) = by_num.get(&n) {
                shas.insert(n, p.head.clone());
                for a in areas_for_files(&p.files, rules) {
                    area_set.insert(a);
                }
            }
        }
        next_num += 1;
        let id = format!("b{next_num:03}");
        let areas: Vec<String> = area_set.into_iter().collect();
        plans.push(BatchPlan {
            id,
            group: group.clone(),
            prs: capped.clone(),
            shas,
            areas,
            because: format!("verify {:?} in group {}", capped, group),
        });
    }
    plans
}

/// Compute the next prefix probe in a binary search bisection over `bisect.members`.
///
/// Invariant:
/// - `k == 0` tests `main` alone (no PRs merged).
/// - `k == i` (`1 <= i <= n`) tests `members[..i]`.
/// - Whenever `k == 1` fails (including a 1-PR batch), `k == 0` (`main` alone) is probed
///   before blaming `members[0]`, so a broken `main` is never misattributed to a PR.
pub fn next_probe(bisect: &Bisect) -> ProbeStep {
    let n = bisect.members.len();
    if n == 0 {
        return ProbeStep::MainBroken;
    }
    if bisect.results.get(&0) == Some(&false) {
        return ProbeStep::MainBroken;
    }

    // Highest passing prefix `lo` and lowest failing prefix `hi`.
    let mut lo: Option<usize> = bisect
        .results
        .iter()
        .filter(|&(_, &ok)| ok)
        .map(|(&k, _)| k)
        .max();
    let hi: usize = bisect
        .results
        .iter()
        .filter(|&(&k, &ok)| k > 0 && !ok)
        .map(|(&k, _)| k)
        .min()
        .unwrap_or(n);

    // If the full batch hasn't been recorded as failed yet and nothing failed, probe `n`.
    if !bisect.results.values().any(|&ok| !ok) {
        return ProbeStep::Probe(n);
    }

    if hi == 1 {
        match bisect.results.get(&0) {
            Some(true) => return ProbeStep::Culprit(bisect.members[0].clone()),
            Some(false) => return ProbeStep::MainBroken,
            None => return ProbeStep::Probe(0),
        }
    }

    let low_bound = lo.unwrap_or(0);
    if low_bound + 1 == hi {
        if low_bound == 0 && !bisect.results.contains_key(&0) {
            return ProbeStep::Probe(0);
        }
        return ProbeStep::Culprit(bisect.members[hi - 1].clone());
    }

    let mid = (low_bound + hi).div_ceil(2);
    let candidate = if mid == hi { hi - 1 } else { mid };
    if candidate == 0 {
        let _ = lo.insert(0);
        return ProbeStep::Probe(0);
    }
    ProbeStep::Probe(candidate)
}

/// Compute remaining PRs to re-batch after a culprit PR is isolated from a multi-PR batch.
pub fn reruns_on_red(batch_prs: &[u64], culprit: u64, parent_map: &BTreeMap<u64, u64>) -> Vec<u64> {
    batch_prs
        .iter()
        .copied()
        .filter(|&pr| {
            if pr == culprit {
                return false;
            }
            // Drop descendants of the culprit as well
            let mut cur = pr;
            let mut seen = BTreeSet::new();
            while let Some(&p) = parent_map.get(&cur) {
                if !seen.insert(cur) {
                    break;
                }
                if p == culprit {
                    return false;
                }
                cur = p;
            }
            true
        })
        .collect()
}

/// Extract a property-based testing counterexample (`"minimal failing input"` / `"proptest"` / `"Falsified"`) from a log.
pub fn counterexample(log: &str) -> Option<String> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)^.*(?:minimal failing input|Falsifying example|shrunk to).*$").unwrap()
    });
    RE.find(log).map(|m| m.as_str().trim().to_string())
}

/// Extract named failing tests from Rust (`test <name> ... FAILED`), Gradle/JUnit (`<Class > method> FAILED`),
/// Playwright (`✘ ... [browser] › <spec> (Ns)`), pytest (`FAILED <name>`), or Vitest/Jest (`FAIL <name>`) log output.
pub fn failed_tests(log: &str) -> Vec<String> {
    static RUST_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^test\s+([^\s]+)\s+\.\.\.\s+FAILED\b").unwrap());
    static GRADLE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*(\w+\s+>\s+\w+)\s+FAILED\b").unwrap());
    static PLAYWRIGHT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)✘\s+\d+\s+\[\w+\]\s+›\s+(.+?)\s+\(\d").unwrap());
    static PY_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^FAILED\s+([^\s]+?)(?:\s+-\s+.*)?$").unwrap());
    static JS_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\s*FAIL\s+([^\s]+\.(?:test|spec)\.[jt]sx?)\b").unwrap());

    let mut seen = BTreeSet::new();
    for caps in RUST_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().to_string());
        }
    }
    for caps in GRADLE_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().trim().to_string());
        }
    }
    for caps in PLAYWRIGHT_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().trim().to_string());
        }
    }
    for caps in PY_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().to_string());
        }
    }
    for caps in JS_RE.captures_iter(log) {
        if let Some(m) = caps.get(1) {
            seen.insert(m.as_str().to_string());
        }
    }
    seen.into_iter().collect()
}

/// Classify a failure log as `"code"` or `"infrastructure"`.
///
/// Key invariant: if the log names a failing test or a property counterexample (`"minimal failing input"`),
/// or contains compiler/type/lint errors, it is ALWAYS classified as `"code"` even if infrastructure
/// strings (`ECONNREFUSED`, `503`, `timed out`) also appear in the log.
pub fn classify(log: &str) -> &'static str {
    if !failed_tests(log).is_empty() || counterexample(log).is_some() {
        return "code";
    }
    static CODE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?im)(?:^error(?:\[E\d+\])?:|panicked at|AssertionError|SyntaxError|TypeError|could not compile)",
        )
        .unwrap()
    });
    if CODE_RE.is_match(log) {
        return "code";
    }
    static INFRA_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?im)(?:ECONNREFUSED|ECONNRESET|ETIMEDOUT|No space left on device|Cannot connect to the Docker daemon|error during connect:.*docker|docker: Error response from daemon|toomanyrequests|rate limit exceeded|502 Bad Gateway|503 Service Unavailable|504 Gateway Time-out|TLS handshake timeout|connection reset by peer|Temporary failure in name resolution|Could not resolve host|failed to fetch|OOMKilled|Cannot allocate memory|out of memory|signal: 9, SIGKILL|resource temporarily unavailable|disk quota exceeded|: command not found|terminating connection due to administrator command|not found: content digest sha256:|failed to export layer|No such image: sha256:|address already in use|containers failed to start|network .* not found|docker could not create the network|Gradle build daemon disappeared unexpectedly|fatal: Unable to create .*\.lock|Failed to initialize NVML)",
        )
        .unwrap()
    });
    if INFRA_RE.is_match(log) {
        return "infrastructure";
    }
    "code"
}

/// Extract the first meaningful error line from a failure log.
pub fn first_error_line(log: &str) -> String {
    if let Some(ce) = counterexample(log) {
        return ce;
    }
    let tests = failed_tests(log);
    if let Some(first) = tests.first() {
        return format!("test {first} ... FAILED");
    }
    for line in log.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("error")
            || lower.contains("panicked at")
            || lower.contains("failed")
            || lower.contains("assertion")
        {
            return trimmed.chars().take(160).collect();
        }
    }
    log.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("check failed")
        .trim()
        .chars()
        .take(160)
        .collect()
}

/// Build a concise summary line for a batch failure.
pub fn summary_line(log: &str) -> String {
    let kind = classify(log);
    let detail = first_error_line(log);
    format!("[{kind}] {detail}")
}

/// Extract the subset of `areas` named after `failed:` in a summary line, or all of `areas` if none match.
pub fn failed_areas(line: &str, areas: &[String]) -> Vec<String> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"failed:\s*([a-z0-9_\- ]+)").unwrap());
    if let Some(caps) = RE.captures(line) {
        if let Some(m) = caps.get(1) {
            let named: BTreeSet<&str> = m.as_str().split_whitespace().collect();
            let matched: Vec<String> = areas
                .iter()
                .filter(|a| named.contains(a.as_str()))
                .cloned()
                .collect();
            if !matched.is_empty() {
                return matched;
            }
        }
    }
    areas.to_vec()
}

/// Build a deterministic task key for a flaky test (`flake-<slug>`).
/// Names whose slug exceeds 40 characters append a deterministic 6-hex-char FNV-1a hash
/// suffix so long test names sharing a 40-char prefix never collide.
pub fn flake_key(test_name: &str) -> String {
    let slug: String = test_name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.len() <= 40 {
        format!("flake-{}", collapsed.trim_matches('-'))
    } else {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in test_name.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
        let prefix: String = collapsed.chars().take(40).collect();
        let prefix = prefix.trim_end_matches('-');
        format!("flake-{prefix}-{:06x}", h & 0xffffff)
    }
}

/// Extract context lines surrounding mentions of `test_name` in `log`.
pub fn lines_about(log: &str, test_name: &str, max_lines: usize) -> String {
    let all: Vec<&str> = log.lines().collect();
    let mut out = Vec::new();
    for (idx, line) in all.iter().enumerate() {
        if line.contains(test_name) {
            let start = idx.saturating_sub(2);
            let end = (idx + 5).min(all.len());
            for l in &all[start..end] {
                if out.len() < max_lines {
                    out.push(*l);
                }
            }
        }
    }
    out.join("\n")
}

/// Record newly observed flaky tests into the persistent flake ledger.
/// Idempotent per `batch_id`: if a test was already recorded for `batch_id`, its count is not incremented again.
pub fn record_flakes(
    ledger: &BTreeMap<String, FlakeEntry>,
    tests: &[String],
    area: &str,
    batch_id: &str,
    log: &str,
    now: i64,
) -> BTreeMap<String, FlakeEntry> {
    let mut out = ledger.clone();
    for t in tests {
        let entry = out.entry(t.clone()).or_insert_with(|| FlakeEntry {
            test: t.clone(),
            area: area.to_string(),
            count: 0,
            first_seen: now,
            last_seen: now,
            batches: Vec::new(),
            excerpt: String::new(),
            task_key: None,
        });
        if !batch_id.is_empty() && entry.batches.iter().any(|b| b == batch_id) {
            continue;
        }
        entry.count += 1;
        entry.last_seen = now;
        if !area.is_empty() && entry.area.is_empty() {
            entry.area = area.to_string();
        }
        if !batch_id.is_empty() {
            entry.batches.push(batch_id.to_string());
        }
        let excerpt = lines_about(log, t, 12);
        if !excerpt.is_empty() {
            entry.excerpt = excerpt;
        }
    }
    out
}

/// Generate `TodoItem`s for any flaky test that has flaked `>= FLAKE_TASK_THRESHOLD` times
/// and does not already have a queued or completed fix task.
pub fn flake_tasks(
    ledger: &BTreeMap<String, FlakeEntry>,
    todo: &[TodoItem],
    agents: &BTreeMap<String, AgentRun>,
    retired: &BTreeSet<String>,
    now: i64,
) -> (BTreeMap<String, FlakeEntry>, Vec<TodoItem>) {
    let mut updated = ledger.clone();
    let mut new_tasks = Vec::new();
    for (test, entry) in updated.iter_mut() {
        if entry.count < FLAKE_TASK_THRESHOLD {
            continue;
        }
        let key = entry.task_key.clone().unwrap_or_else(|| flake_key(test));
        if retired.contains(&key) || todo.iter().any(|t| t.key == key) || agents.contains_key(&key)
        {
            entry.task_key = Some(key);
            continue;
        }
        entry.task_key = Some(key.clone());
        let mut brief = format!(
            "Fix flaky test `{test}` (flaked {} times in batches {:?}). Reproduce under CPU starvation (`tacho starve`) and eliminate the race or timing assumption.",
            entry.count, entry.batches
        );
        if !entry.excerpt.is_empty() {
            brief.push_str(&format!("\n\nFailure excerpt:\n{}", entry.excerpt));
        }
        new_tasks.push(TodoItem {
            key,
            brief,
            area: entry.area.clone(),
            base: None,
            after: Vec::new(),
            priority: 1,
            created_at: now,
            provider: None,
            model: None,
            effort: None,
            compare: None,
        });
    }
    (updated, new_tasks)
}

/// Render sorted rows for `tacho flakes`.
pub fn flake_rows(ledger: &BTreeMap<String, FlakeEntry>) -> Vec<FlakeRow> {
    let mut rows: Vec<FlakeRow> = ledger
        .values()
        .map(|e| FlakeRow {
            test: e.test.clone(),
            area: e.area.clone(),
            count: e.count,
            last_seen: e.last_seen,
            batches: e.batches.clone(),
            task_key: e.task_key.clone(),
            excerpt: e.excerpt.clone(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
            .then_with(|| a.test.cmp(&b.test))
    });
    rows
}

/// Single-retry fix agent state machine for PRs that failed batch verification:
/// - Dispatches at most 1 automatic `fix-pr<n>` agent per red `(pr, sha)`.
/// - If a fix agent already ran for that `(pr, sha)` and the PR is still red at that head,
///   emits a human notification instead of looping.
pub fn decide_fixes(
    failed: &BTreeMap<u64, FailedRecord>,
    open_prs: &[PullFacts],
    agents: &BTreeMap<String, AgentRun>,
    holds: &BTreeMap<String, Hold>,
    live_keys: &BTreeSet<String>,
    notified: &HashMap<u64, String>,
    free_slots: usize,
) -> FixDecisions {
    let by_num: HashMap<u64, &PullFacts> = open_prs.iter().map(|p| (p.number, p)).collect();
    let mut out = FixDecisions::default();

    for (&pr_num, rec) in failed {
        if holds.contains_key(&format!("pr:{pr_num}")) {
            continue;
        }
        let Some(pr) = by_num.get(&pr_num) else {
            continue;
        };
        if !pr.state.eq_ignore_ascii_case("OPEN") || pr.head != rec.sha {
            continue;
        }
        if already_ran("fix", pr_num, &rec.sha, agents, live_keys) {
            let key_prefix = format!("fix-pr{pr_num}");
            let still_running = agents.values().any(|a| {
                (a.key == key_prefix || a.key.starts_with(&format!("{key_prefix}-")))
                    && live_keys.contains(&a.key)
            });
            if !still_running && notified.get(&pr_num) != Some(&rec.sha) {
                out.notify.push((
                    pr_num,
                    format!(
                        "PR #{pr_num} remains red at {} after automatic fix attempt: {}",
                        &rec.sha[..rec.sha.len().min(8)],
                        rec.summary
                    ),
                ));
            }
            continue;
        }
        if out.dispatch.len() < free_slots {
            out.dispatch.push((pr_num, rec.clone()));
        }
    }
    out
}

/// Validate a worker slug (`3..=80` chars, lowercase ASCII alphanumeric separated by single hyphens).
pub fn slug_problem(slug: &str) -> Option<String> {
    if slug.len() < 3 || slug.len() > 80 {
        return Some(format!(
            "slug '{slug}' must be between 3 and 80 characters (got {})",
            slug.len()
        ));
    }
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9]+(-[a-z0-9]+)*$").unwrap());
    if !RE.is_match(slug) {
        return Some(format!(
            "slug '{slug}' must match ^[a-z0-9]+(-[a-z0-9]+)*$ (lowercase letters, digits, single hyphens)"
        ));
    }
    None
}

/// Allocate an isolated block of [`PORT_BLOCK_SIZE`] ports in `PORT_RANGE_START..PORT_RANGE_END`
/// that does not overlap any `used_ports` and passes `port_free`.
pub fn allocate_ports<F>(used_ports: &BTreeSet<u16>, mut port_free: F) -> Result<(u16, u16), String>
where
    F: FnMut(u16) -> bool,
{
    let mut start = PORT_RANGE_START;
    while start + PORT_BLOCK_SIZE <= PORT_RANGE_END {
        let last = start + PORT_BLOCK_SIZE - 1;
        let block_used = (start..=last).any(|p| used_ports.contains(&p));
        if !block_used && (start..=last).all(&mut port_free) {
            return Ok((start, last));
        }
        start += PORT_BLOCK_SIZE;
    }
    Err(format!(
        "no free {PORT_BLOCK_SIZE}-port block available in {PORT_RANGE_START}..{PORT_RANGE_END}"
    ))
}

/// Validate a background job name (`tacho job <name>`): 1..=40 chars matching `^[a-z0-9][a-z0-9._-]*$`.
pub fn job_name_problem(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 40 {
        return Some(format!(
            "job name '{name}' must be between 1 and 40 characters"
        ));
    }
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]*$").unwrap());
    if !RE.is_match(name) {
        return Some(format!(
            "job name '{name}' must match ^[a-z0-9][a-z0-9._-]*$"
        ));
    }
    None
}

fn sh_quote(val: &str) -> String {
    format!("'{}'", val.replace('\'', "'\\''"))
}

/// Format the worktree `.tachoenv` file contents using POSIX `export KEY='VALUE'` syntax.
pub fn env_file(
    slug: &str,
    base_sha: &str,
    base_pr: Option<u64>,
    port: u16,
    port_last: u16,
    target_dir: Option<&str>,
) -> String {
    let base_pr_str = base_pr.map(|n| n.to_string()).unwrap_or_default();
    let mut lines = vec![
        format!("export SLUG={}", sh_quote(slug)),
        format!("export BASE_SHA={}", sh_quote(base_sha)),
        format!("export BASE_PR={}", sh_quote(&base_pr_str)),
        format!("export PORT={}", sh_quote(&port.to_string())),
        format!("export PORT_LAST={}", sh_quote(&port_last.to_string())),
    ];
    if let Some(td) = target_dir {
        if !td.is_empty() {
            lines.push(format!("export CARGO_TARGET_DIR={}", sh_quote(td)));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Parse a KEY=VALUE or `export KEY='VALUE'` `.tachoenv` file into a map, unescaping `'\''` -> `'`.
pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let stripped = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if let Some((k, v)) = stripped.split_once('=') {
            let raw = v.trim();
            let val = if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
                raw[1..raw.len() - 1]
                    .replace("'\\''", "'")
                    .replace("'\"'\"'", "'")
            } else if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
                raw[1..raw.len() - 1].to_string()
            } else {
                raw.to_string()
            };
            out.insert(k.trim().to_string(), val);
        }
    }
    out
}

/// Prepend missing directories from `extra` onto `current_path` without duplicates.
pub fn merge_path(current_path: &str, extra: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for e in extra {
        if !e.is_empty() && !parts.iter().any(|p| p == e) {
            parts.push((*e).to_string());
        }
    }
    for seg in current_path.split(':').filter(|s| !s.is_empty()) {
        if !parts.iter().any(|p| p == seg) {
            parts.push(seg.to_string());
        }
    }
    parts.join(":")
}

/// Check whether `child` is inside `worktree` (canonicalizing symlinks when paths exist on disk).
pub fn owns(child: &Path, worktree: &Path) -> bool {
    match (
        std::fs::canonicalize(child),
        std::fs::canonicalize(worktree),
    ) {
        (Ok(real_child), Ok(real_wt)) => real_child.starts_with(&real_wt),
        _ => child.starts_with(worktree),
    }
}

/// Build the list of `(name, command)` checks required to prove `changed_files`.
///
/// Uses configured `rules` when matched, and automatically adds targeted Rust crate/module/test
/// checks (`--lib -- <module>::`, `--test <suite>`, `cargo fmt --all -- --check`) when `.rs`
/// files or `Cargo.toml` are modified, plus `bash -n` for `.sh` scripts.
pub fn proof_plan(changed_files: &[String], rules: &[AreaRule]) -> Vec<(String, String)> {
    let mut checks: Vec<(String, String)> = Vec::new();
    let mut seen_cmds = BTreeSet::new();
    let mut add = |name: String, cmd: String| {
        if seen_cmds.insert(cmd.clone()) {
            checks.push((name, cmd));
        }
    };

    if !changed_files.is_empty() {
        add("whitespace".to_string(), "git diff --check".to_string());
    }

    for r in rules {
        if changed_files
            .iter()
            .any(|f| r.prefixes.iter().any(|p| f.starts_with(p)))
        {
            for (name, cmd) in &r.checks {
                add(name.clone(), cmd.clone());
            }
        }
    }

    let mut rust_crates = BTreeSet::new();
    let mut whole_crate = BTreeSet::new();
    let mut crate_modules: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut crate_itests: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut rust_all = false;
    let mut any_rust = false;
    let mut shell_files = Vec::new();

    for f in changed_files {
        if f == "Cargo.toml" || f == "Cargo.lock" || f == "rust-toolchain.toml" {
            rust_all = true;
            any_rust = true;
        } else if let Some(rest) = f.strip_prefix("crates/") {
            let parts: Vec<&str> = rest.split('/').collect();
            if let Some(&krate) = parts.first() {
                if f.ends_with(".rs") || f.ends_with("Cargo.toml") {
                    any_rust = true;
                    rust_crates.insert(krate.to_string());
                    match parts.get(1).copied() {
                        Some("Cargo.toml") | Some("build.rs") => {
                            whole_crate.insert(krate.to_string());
                        }
                        Some("src") if parts.len() >= 3 && f.ends_with(".rs") => {
                            let rel_src = &parts[2..];
                            if rel_src == ["lib.rs"] || rel_src == ["main.rs"] {
                                whole_crate.insert(krate.to_string());
                            } else {
                                let mut mod_parts: Vec<&str> = rel_src.to_vec();
                                if let Some(last) = mod_parts.last_mut() {
                                    if *last == "mod.rs" {
                                        mod_parts.pop();
                                    } else if let Some(stem) = last.strip_suffix(".rs") {
                                        *last = stem;
                                    }
                                }
                                if mod_parts.is_empty() {
                                    whole_crate.insert(krate.to_string());
                                } else {
                                    crate_modules
                                        .entry(krate.to_string())
                                        .or_default()
                                        .insert(format!("{}::", mod_parts.join("::")));
                                }
                            }
                        }
                        Some("tests") if parts.len() == 3 && f.ends_with(".rs") => {
                            if let Some(suite) = parts[2].strip_suffix(".rs") {
                                crate_itests
                                    .entry(krate.to_string())
                                    .or_default()
                                    .insert(suite.to_string());
                            } else {
                                whole_crate.insert(krate.to_string());
                            }
                        }
                        _ => {
                            whole_crate.insert(krate.to_string());
                        }
                    }
                }
            }
        } else if f.ends_with(".rs") {
            rust_all = true;
            any_rust = true;
        }
        if f.ends_with(".sh") {
            shell_files.push(f.clone());
        }
    }

    if !shell_files.is_empty() {
        let quoted: Vec<String> = shell_files.iter().map(|s| format!("'{s}'")).collect();
        add(
            "shell-syntax".to_string(),
            format!("bash -n {}", quoted.join(" ")),
        );
    }

    if any_rust {
        add(
            "rust-fmt".to_string(),
            "cargo fmt --all -- --check".to_string(),
        );
    }

    if rust_all {
        add(
            "clippy-workspace".to_string(),
            "cargo clippy --workspace --all-targets -- -D warnings".to_string(),
        );
        add(
            "test-workspace".to_string(),
            "cargo test --workspace".to_string(),
        );
    } else if !rust_crates.is_empty() {
        let pkg_flags: Vec<String> = rust_crates.iter().map(|c| format!("-p {c}")).collect();
        let pkgs = pkg_flags.join(" ");
        add(
            format!(
                "clippy ({})",
                rust_crates.iter().cloned().collect::<Vec<_>>().join(",")
            ),
            format!("cargo clippy {pkgs} --all-targets -- -D warnings"),
        );
        for c in &rust_crates {
            if whole_crate.contains(c) {
                add(format!("test ({c})"), format!("cargo test -p {c}"));
            } else {
                if let Some(mods) = crate_modules.get(c) {
                    for m in mods {
                        let label = m.trim_end_matches("::");
                        add(
                            format!("test {c}::{label}"),
                            format!("cargo test -p {c} --lib -- {m}"),
                        );
                    }
                }
                if let Some(suites) = crate_itests.get(c) {
                    for suite in suites {
                        add(
                            format!("test {c} --test {suite}"),
                            format!("cargo test -p {c} --test {suite}"),
                        );
                    }
                }
            }
        }
    }

    checks
}

/// Detect forbidden attribution/trailer markers (`Co-Authored-By`, `Generated with/by`,
/// session links, or robot emoji) in commit messages, PR titles, or descriptions.
pub fn attribution(texts: &[&str]) -> Vec<String> {
    static CO_AUTHOR_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?im)^\s*co-authored-by\s*:").unwrap());
    static GEN_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?im)(?:generated\s+(?:with|by)\b|https?://\S*(?:claude\.ai|anthropic\.com)\S*|https?://\S*session\S*)",
        )
        .unwrap()
    });

    let mut problems = Vec::new();
    for text in texts {
        let first_tok = text.split_whitespace().next().unwrap_or("");
        let sha_prefix = if first_tok.len() >= 7
            && first_tok.len() <= 40
            && first_tok.chars().all(|c| c.is_ascii_hexdigit())
        {
            Some(&first_tok[..first_tok.len().min(8)])
        } else {
            None
        };
        if CO_AUTHOR_RE.is_match(text) {
            if let Some(sha) = sha_prefix {
                problems.push(format!(
                    "commit {sha} contains forbidden 'Co-Authored-By:' trailer"
                ));
            } else {
                problems.push("contains forbidden 'Co-Authored-By:' trailer".to_string());
            }
        }
        if GEN_RE.is_match(text) {
            if let Some(sha) = sha_prefix {
                problems.push(format!(
                    "commit {sha} contains forbidden 'Generated with/by' or session link attribution"
                ));
            } else {
                problems
                    .push("contains forbidden 'Generated with/by' attribution line".to_string());
            }
        }
        if text.contains('\u{1F916}') {
            problems.push("contains forbidden robot emoji attribution".to_string());
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

/// Check whether a `ProofSummary` is valid for the current `head_sha` and `head_tree`.
pub fn proof_problems(
    proof: Option<&ProofSummary>,
    head_sha: &str,
    head_tree: &str,
    changed_files: &[String],
    rules: &[AreaRule],
) -> Vec<String> {
    let Some(p) = proof else {
        return vec!["no proof found (run `tacho prove` first)".to_string()];
    };
    let mut problems = Vec::new();
    if !p.ok {
        problems.push("last `tacho prove` failed (all checks must pass)".to_string());
    }
    if !head_tree.is_empty() && p.tree != head_tree {
        problems.push(format!(
            "proof is stale: proved tree {} != HEAD tree {} (re-run `tacho prove`)",
            &p.tree[..p.tree.len().min(12)],
            &head_tree[..head_tree.len().min(12)]
        ));
    } else if head_tree.is_empty() && !head_sha.is_empty() && p.head != head_sha {
        problems.push(format!(
            "proof head {} != current HEAD {} (re-run `tacho prove`)",
            &p.head[..p.head.len().min(12)],
            &head_sha[..head_sha.len().min(12)]
        ));
    }
    let needs_shot = rules.iter().any(|r| {
        r.needs_screenshot
            && changed_files
                .iter()
                .any(|f| r.prefixes.iter().any(|pref| f.starts_with(pref)))
    });
    if needs_shot && p.screenshots.is_empty() {
        problems.push("UI files changed but proof has no screenshots".to_string());
    }
    problems
}

/// Validate all preflight rules before `tacho pr` pushes or opens a pull request.
#[allow(clippy::too_many_arguments)]
pub fn pr_refusals(
    title: &str,
    description: &str,
    commits: &[String],
    merge_commits: &[String],
    changed_files: &[String],
    dirty_files: &[String],
    head_sha: &str,
    head_tree: &str,
    proof: Option<&ProofSummary>,
    rules: &[AreaRule],
) -> Vec<String> {
    let mut refusals = Vec::new();
    if title.trim().is_empty() {
        refusals.push("PR title cannot be empty".to_string());
    }
    if description.trim().is_empty() {
        refusals.push("PR description (-m) cannot be empty".to_string());
    }
    if commits.is_empty() || changed_files.is_empty() {
        refusals.push("branch has no commits or changed files ahead of base".to_string());
    }
    if !merge_commits.is_empty() {
        refusals.push(format!(
            "branch contains merge commit(s) {:?} (rebase onto base instead of merging)",
            merge_commits
        ));
    }
    if !dirty_files.is_empty() {
        refusals.push(format!(
            "worktree has uncommitted changes {:?} (commit or clean before `tacho pr`)",
            dirty_files
        ));
    }
    refusals.extend(proof_problems(
        proof,
        head_sha,
        head_tree,
        changed_files,
        rules,
    ));

    let mut texts: Vec<&str> = vec![title, description];
    for c in commits {
        texts.push(c.as_str());
    }
    if let Some(p) = proof {
        for chk in &p.checks {
            texts.push(chk.output_tail.as_str());
        }
    }
    refusals.extend(attribution(&texts));
    refusals
}

/// Format a pull request body combining the human summary and the structured `## Proof` table.
pub fn build_body(description: &str, proof: &ProofSummary) -> String {
    let mut lines = vec![
        description.trim().to_string(),
        String::new(),
        "## Proof".to_string(),
        String::new(),
        format!(
            "- **Head**: `{}` (tree `{}`)",
            &proof.head[..proof.head.len().min(12)],
            &proof.tree[..proof.tree.len().min(12)]
        ),
        format!("- **Base**: `{}`", &proof.base[..proof.base.len().min(12)]),
    ];
    if !proof.checks.is_empty() {
        lines.push(String::new());
        lines.push("| Check | Status | Duration | Command |".to_string());
        lines.push("| :--- | :--- | ---: | :--- |".to_string());
        for c in &proof.checks {
            let status = if c.ok { "PASS" } else { "FAIL" };
            lines.push(format!(
                "| `{}` | {} | {}ms | `{}` |",
                c.name, status, c.duration_ms, c.cmd
            ));
        }
    }
    lines.join("\n")
}
