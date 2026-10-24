//! Prefix bisection with the reference semantics (R21).
//!
//! `results[k]` is whether `members[..k]` merged onto main passed (`k == 0` is main
//! alone). Until some prefix has passed, main itself is unproven, so halving reaches
//! `k == 0` before anyone is blamed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What to do next in a bisection (`next_prefix`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Probe {
    /// Check `members[..k]`.
    Prefix(usize),
    /// `members[i]` is the earliest member whose addition turns the prefix red.
    Culprit(usize),
    /// Main alone is red; nobody is blamed.
    MainBroken,
}

/// The next probe given `n` members and the prefix results so far.
pub fn next_prefix(n: usize, results: &BTreeMap<usize, bool>) -> Probe {
    let hi = results
        .iter()
        .filter(|(_, ok)| !**ok)
        .map(|(k, _)| *k)
        .min()
        .unwrap_or(n);
    let lo = results
        .iter()
        .filter(|(k, ok)| **ok && **k < hi)
        .map(|(k, _)| *k)
        .max();
    if hi == 0 {
        return Probe::MainBroken;
    }
    match lo {
        None => Probe::Prefix(hi / 2),
        Some(lo) if hi - lo == 1 => Probe::Culprit(hi - 1),
        Some(lo) => Probe::Prefix((lo + hi) / 2),
    }
}

/// A recorded prefix probe outcome (`prefix_len` members merged onto main).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BisectProbe {
    pub prefix_len: usize,
    pub passed: bool,
}

/// Decision returned by [`step`] during prefix bisection over `cand`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BisectDecision {
    /// Run a check on `prefix` (`cand[..prefix_len]`).
    Probe {
        prefix_len: usize,
        prefix: Vec<u64>,
        reason: String,
    },
    /// Identified `culprit` as the earliest PR whose prefix failed while the preceding prefix passed.
    Resolved { culprit: u64, reason: String },
    /// Every probed prefix passed even though the full batch failed (flaky failure).
    Inconclusive { reason: String },
}

/// Pure prefix bisection step over `cand` given `probes` (where `prefix_len` in `1..=cand.len()`).
pub fn step(cand: &[u64], probes: &[BisectProbe]) -> BisectDecision {
    if cand.is_empty() {
        return BisectDecision::Inconclusive {
            reason: "cannot bisect an empty candidate list".to_string(),
        };
    }
    let mut map: BTreeMap<usize, bool> = probes
        .iter()
        .filter(|p| p.prefix_len >= 1 && p.prefix_len <= cand.len())
        .map(|p| (p.prefix_len, p.passed))
        .collect();
    map.entry(cand.len()).or_insert(false);

    if map.get(&cand.len()) == Some(&true) {
        return BisectDecision::Inconclusive {
            reason: "full prefix passed during bisection".to_string(),
        };
    }

    let hi = map
        .iter()
        .filter(|&(_, &ok)| !ok)
        .map(|(&k, _)| k)
        .min()
        .unwrap_or(cand.len());
    let lo = map
        .iter()
        .filter(|&(&k, &ok)| ok && k < hi)
        .map(|(&k, _)| k)
        .max()
        .unwrap_or(0);

    if hi - lo <= 1 {
        let idx = hi.saturating_sub(1);
        let culprit = cand[idx];
        return BisectDecision::Resolved {
            culprit,
            reason: format!(
                "prefix [..={lo}] passed (or empty) and prefix [..={hi}] failed; PR #{culprit} is the first failing change"
            ),
        };
    }

    let mid = (lo + hi) / 2;
    let prefix = cand[..mid].to_vec();
    BisectDecision::Probe {
        prefix_len: mid,
        prefix: prefix.clone(),
        reason: format!("bisecting ({lo}, {hi}] at prefix_len={mid} ({prefix:?})"),
    }
}

/// Whether `pr` is the resolved culprit of `cand` given `probes`.
pub fn is_culprit(cand: &[u64], probes: &[BisectProbe], pr: u64) -> bool {
    matches!(step(cand, probes), BisectDecision::Resolved { culprit, .. } if culprit == pr)
}
