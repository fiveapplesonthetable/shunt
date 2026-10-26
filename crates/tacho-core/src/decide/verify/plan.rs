//! PR stack ancestry, batch planning, bisection (legacy surface).

use super::*;

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

/// Compute structured `(area, affected_units)` checks for a set of changed files.
pub fn checks_for_files(files: &[String], rules: &[AreaRule]) -> Vec<AreaCheck> {
    let areas = areas_for_files(files, rules);
    let mut out = Vec::with_capacity(areas.len());
    for area in areas {
        let mut units_set = BTreeSet::new();
        if let Some(rule) = rules.iter().find(|r| r.area == area) {
            if !rule.unit_prefixes.is_empty() {
                for u in affected_units(files, &rule.unit_prefixes) {
                    units_set.insert(u);
                }
            }
        }
        if units_set.is_empty() {
            for f in files {
                if let Some(rest) = f.strip_prefix("crates/") {
                    if let Some(krate) = rest.split('/').next() {
                        if area == krate || area == "workspace" {
                            units_set.insert(krate.to_string());
                        }
                    }
                }
            }
        }
        if units_set.is_empty() {
            units_set.insert("all".to_string());
        }
        out.push(AreaCheck {
            area,
            units: units_set.into_iter().collect(),
        });
    }
    out
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
        if b.status != "passed" || !b.flakes.is_empty() {
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

/// Open PRs held by a `main-broken` batch: members still at the head the batch recorded.
///
/// Main itself was red when the batch ran, so re-checking those heads proves nothing until
/// the batch is cleared (see [`crate::decide::verify::unbreak`]); a member that has moved on is
/// free to be checked again.
fn main_broken_blocked<'a>(
    open_prs: &'a [PullFacts],
    existing_batches: &'a [BatchRun],
) -> impl Iterator<Item = u64> + 'a {
    existing_batches
        .iter()
        .filter(|b| b.status == "main-broken")
        .flat_map(move |b| {
            b.prs.iter().copied().filter(move |n| {
                let recorded = b.shas.get(n);
                open_prs
                    .iter()
                    .any(|p| p.number == *n && recorded.is_none_or(|sha| *sha == p.head))
            })
        })
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
        .chain(main_broken_blocked(open_prs, existing_batches))
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
        let mut all_files = Vec::new();
        for &n in &capped {
            if let Some(p) = by_num.get(&n) {
                shas.insert(n, p.head.clone());
                all_files.extend(p.files.iter().cloned());
                for a in areas_for_files(&p.files, rules) {
                    area_set.insert(a);
                }
            }
        }
        next_num += 1;
        let id = format!("b{next_num:03}");
        let raw_areas: Vec<String> = area_set.into_iter().collect();
        let areas = select_costly_areas(&raw_areas, &all_files, true, false);
        let raw_checks = checks_for_files(&all_files, rules);
        let area_lookup: BTreeSet<&str> = areas.iter().map(String::as_str).collect();
        let mut checks: Vec<AreaCheck> = raw_checks
            .into_iter()
            .filter(|c| area_lookup.contains(c.area.as_str()))
            .collect();
        for a in &areas {
            if !checks.iter().any(|c| &c.area == a) {
                checks.push(AreaCheck {
                    area: a.clone(),
                    units: vec!["all".to_string()],
                });
            }
        }
        plans.push(BatchPlan {
            id,
            group: group.clone(),
            prs: capped.clone(),
            shas,
            areas,
            checks,
            mode: BatchMode::Presubmit,
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
