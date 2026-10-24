//! Area selection & pruning (R13–R16), redraw area selection (Parity 49), and
//! generated-file settlement (R30).

use super::config::AreaSelectConfig;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// R13: verification tier (`Change` for PR checks, `Train` for pre-merge train batches,
/// `All` for full suite runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    Change,
    Train,
    All,
}

/// R13: result of selecting areas for a file list at a given tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AreaSelection {
    pub areas: BTreeSet<String>,
    /// Files that did not match any configured path rule (reported for train telemetry).
    pub unplaced: Vec<String>,
    pub reason: String,
}

fn matches_any(file: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| match Regex::new(p) {
        Ok(re) => re.is_match(file),
        Err(_) => file.contains(p.as_str()),
    })
}

/// R13: map changed files to CI areas for `tier` using `cfg.path_rules`.
///
/// - `Tier::All`: returns `cfg.all_areas` directly.
/// - Empty `files`: returns empty set.
/// - Files matching `cfg.doc_ignore` (and not `cfg.doc_keep`) contribute nothing.
/// - Each file matches the first rule in `cfg.path_rules` whose pattern matches.
/// - An unplaced file (matching no rule):
///   - on `Tier::Train`: adds `cfg.train_unplaced_areas` (default `rust` + `gate`) and records the path in `unplaced`.
///   - on `Tier::Change`: fails closed by adding `cfg.all_areas` and records the path in `unplaced`.
pub fn select_areas(files: &[String], tier: Tier, cfg: &AreaSelectConfig) -> AreaSelection {
    if tier == Tier::All {
        return AreaSelection {
            areas: cfg.all_areas.clone(),
            unplaced: Vec::new(),
            reason: "tier=all selects all configured areas".to_string(),
        };
    }
    if files.is_empty() {
        return AreaSelection {
            areas: BTreeSet::new(),
            unplaced: Vec::new(),
            reason: "empty file list selects no areas".to_string(),
        };
    }
    let mut areas = BTreeSet::new();
    let mut unplaced = Vec::new();
    for f in files {
        if matches_any(f, &cfg.doc_ignore) && !matches_any(f, &cfg.doc_keep) {
            continue;
        }
        let mut matched = false;
        for rule in &cfg.path_rules {
            let hit = match Regex::new(&rule.pattern) {
                Ok(re) => re.is_match(f),
                Err(_) => f.contains(&rule.pattern),
            };
            if hit {
                let rule_areas = match tier {
                    Tier::Train => rule.train_areas.as_ref().unwrap_or(&rule.areas),
                    Tier::Change | Tier::All => &rule.areas,
                };
                areas.extend(rule_areas.iter().cloned());
                matched = true;
                break;
            }
        }
        if !matched {
            unplaced.push(f.clone());
            match tier {
                Tier::Train => {
                    if cfg.train_unplaced_areas.is_empty() {
                        if let Some(g) = &cfg.gate {
                            areas.insert(g.clone());
                        }
                    } else {
                        areas.extend(cfg.train_unplaced_areas.iter().cloned());
                    }
                }
                Tier::Change | Tier::All => {
                    areas.extend(cfg.all_areas.iter().cloned());
                }
            }
        }
    }
    let reason = if unplaced.is_empty() {
        format!("selected {:?} for {:?} tier", areas, tier)
    } else {
        format!(
            "selected {:?} for {:?} tier ({} unplaced file(s): {:?})",
            areas,
            tier,
            unplaced.len(),
            unplaced
        )
    };
    AreaSelection {
        areas,
        unplaced,
        reason,
    }
}

/// R14: compute the areas a run actually executes from `selected` areas and touched `files`.
///
/// 1. Starts from `selected - skipped - cfg.post_merge`.
/// 2. If `train`: adds `cfg.gate` (if `Some`).
/// 3. If `!train`: adds `cfg.pixel_areas - skipped` when any file matches `cfg.pixel_files`,
///    and adds `cfg.change_extra`.
/// 4. Prunes any area in `cfg.train_inputs` (when `train`) or `cfg.change_inputs` (when `!train`)
///    whose input pattern matches none of `files`.
pub fn check_areas(
    selected: &BTreeSet<String>,
    files: &[String],
    skipped: &BTreeSet<String>,
    train: bool,
    cfg: &AreaSelectConfig,
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = selected
        .iter()
        .filter(|a| !skipped.contains(*a) && !cfg.post_merge.contains(*a))
        .cloned()
        .collect();
    if train {
        if let Some(gate) = &cfg.gate {
            if !skipped.contains(gate) {
                out.insert(gate.clone());
            }
        }
    } else {
        if !cfg.pixel_files.is_empty() && files.iter().any(|f| matches_any(f, &cfg.pixel_files)) {
            for a in &cfg.pixel_areas {
                if !skipped.contains(a) {
                    out.insert(a.clone());
                }
            }
        }
        for a in &cfg.change_extra {
            if !skipped.contains(a) {
                out.insert(a.clone());
            }
        }
    }
    let inputs = if train {
        &cfg.train_inputs
    } else {
        &cfg.change_inputs
    };
    for (area, pat) in inputs {
        if out.contains(area) {
            let hit = match Regex::new(pat) {
                Ok(re) => files.iter().any(|f| re.is_match(f)),
                Err(_) => true,
            };
            if !hit {
                out.remove(area);
            }
        }
    }
    out
}

/// R15 & R16: recompute the areas to check from the current PR heads' file lists rather than
/// trusting a stale queue snapshot.
///
/// - When `head_files` is non-empty (every member's file list was fetched): recomputes
///   `check_areas` on the union of `selector(files, Tier::Train)` across members. If the
///   resulting area set is empty (doc-only batch), runs `[gate]` only (R16).
/// - When `head_files` is empty (e.g. git diff could not be fetched): falls back to `queued_areas`
///   filtered by `skipped` and `post_merge`, plus `gate`.
pub fn recompute_checked_areas(
    queued_areas: &[String],
    head_files: &[Vec<String>],
    skipped: &BTreeSet<String>,
    cfg: &AreaSelectConfig,
    selector: impl Fn(&[String], Tier) -> BTreeSet<String>,
) -> Vec<String> {
    if head_files.is_empty() {
        let mut out: BTreeSet<String> = queued_areas
            .iter()
            .filter(|a| !skipped.contains(*a) && !cfg.post_merge.contains(*a))
            .cloned()
            .collect();
        if let Some(gate) = &cfg.gate {
            if !skipped.contains(gate) {
                out.insert(gate.clone());
            }
        }
        return out.into_iter().collect();
    }
    let mut all_files = Vec::new();
    let mut raw_areas = BTreeSet::new();
    for files in head_files {
        all_files.extend(files.iter().cloned());
        raw_areas.extend(selector(files, Tier::Train));
    }
    let checked = check_areas(&raw_areas, &all_files, skipped, true, cfg);
    if checked.is_empty() {
        if let Some(gate) = &cfg.gate {
            return vec![gate.clone()];
        }
    }
    checked.into_iter().collect()
}

/// Parity 49: select the visual-redraw areas (`pixel_areas`) needed to refresh goldens for `files`.
/// Returns `cfg.pixel_areas` when any file matches `cfg.pixel_files`, else empty.
pub fn redraw_areas(files: &[String], cfg: &AreaSelectConfig) -> BTreeSet<String> {
    if !cfg.pixel_files.is_empty() && files.iter().any(|f| matches_any(f, &cfg.pixel_files)) {
        cfg.pixel_areas.clone()
    } else {
        BTreeSet::new()
    }
}

/// Parity 49: the areas a train pass must have checked for a PR touching `files` (`selected`
/// train areas pruned by `check_areas`).
pub fn required_train_areas(
    files: &[String],
    selected: &BTreeSet<String>,
    skipped: &BTreeSet<String>,
    cfg: &AreaSelectConfig,
) -> BTreeSet<String> {
    let mut out = check_areas(selected, files, skipped, true, cfg);
    if let Some(gate) = &cfg.gate {
        out.remove(gate);
    }
    out
}

/// R30: if every conflicting file is a generated file with a configured regenerator area,
/// return the set of regenerator areas to run; otherwise `None` (conflict is real).
pub fn generated_areas(files: &[String], cfg: &AreaSelectConfig) -> Option<BTreeSet<String>> {
    if files.is_empty() {
        return None;
    }
    let mut out = BTreeSet::new();
    for f in files {
        let mut hit = None;
        for (pat, area) in &cfg.generated {
            let m = match Regex::new(pat) {
                Ok(re) => re.is_match(f),
                Err(_) => false,
            };
            if m {
                hit = Some(area.clone());
                break;
            }
        }
        out.insert(hit?);
    }
    Some(out)
}

/// R30: decision when a merge has conflicts in `conflict_files` and `unmerged_after` remains
/// after attempting regeneration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Settlement {
    /// Every conflict is in generated files; run these regenerator areas and stage all changes.
    Regenerate {
        areas: BTreeSet<String>,
        reason: String,
    },
    /// Conflict is in hand-written files or regeneration left unmerged paths; abort and skip.
    AbortAndSkip { files: Vec<String>, reason: String },
}

/// R30: decide whether a merge conflict in `conflict_files` can be settled by regenerating.
pub fn settle_conflicts(conflict_files: &[String], cfg: &AreaSelectConfig) -> Settlement {
    match generated_areas(conflict_files, cfg) {
        Some(areas) => Settlement::Regenerate {
            reason: format!(
                "all conflicting files {:?} are generated; regenerating {:?}",
                conflict_files, areas
            ),
            areas,
        },
        None => Settlement::AbortAndSkip {
            files: conflict_files.to_vec(),
            reason: format!(
                "conflicting files {:?} include non-generated files; aborting merge",
                conflict_files
            ),
        },
    }
}
