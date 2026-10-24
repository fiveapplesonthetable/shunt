//! CI job types, head-movement refusal, hang reports.

use super::*;

// =============================================================================
// Batch Workflow Job Parser, Head-Movement Refusal & Area Hang Reporting
// =============================================================================

/// GitHub Actions `uses:` steps that are stand-in setup/cache/artifact steps when running
/// a workflow job directly inside its container on the local batch runner.
pub const STAND_IN_USES: &[&str] = &[
    "actions/checkout",
    "Swatinem/rust-cache",
    "actions/setup-node",
    "actions/upload-artifact",
];

/// A service container attached to a CI workflow job (e.g. `postgres`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiService {
    pub name: String,
    pub image: String,
    pub env: BTreeMap<String, String>,
    pub ports: Vec<String>,
    pub options: String,
}

/// A shell step extracted from a CI workflow job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiStep {
    pub name: String,
    pub run: String,
    pub working_directory: Option<String>,
    pub env: BTreeMap<String, String>,
}

/// A parsed CI workflow job ready for containerized execution by `tacho batch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiJob {
    pub area: String,
    pub name: String,
    pub image: Option<String>,
    pub env: BTreeMap<String, String>,
    pub service: Option<CiService>,
    pub steps: Vec<CiStep>,
}

/// Batch verification depth (`Presubmit` vs `Postsubmit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BatchMode {
    /// Fast presubmit verification (`affected_units` + area checks).
    #[default]
    Presubmit,
    /// Full postsubmit verification (including full suite / E2E / extended jobs).
    Postsubmit,
}

/// Check whether `main` or any PR in a running batch moved its head SHA while verification was running.
pub fn batch_refs_moved(
    expected_main: &str,
    current_main: &str,
    expected_shas: &BTreeMap<u64, String>,
    current_shas: &BTreeMap<u64, String>,
) -> Option<String> {
    let mut moved = Vec::new();
    if !expected_main.is_empty() && !current_main.is_empty() && expected_main != current_main {
        moved.push(format!(
            "main moved from {} to {}",
            crate::char_prefix(expected_main, 8),
            crate::char_prefix(current_main, 8)
        ));
    }
    for (pr, expected) in expected_shas {
        match current_shas.get(pr) {
            Some(actual) if actual == expected => {}
            Some(actual) => {
                moved.push(format!(
                    "PR #{pr} moved from {} to {}",
                    crate::char_prefix(expected, 8),
                    crate::char_prefix(actual, 8)
                ));
            }
            None => {
                moved.push(format!(
                    "PR #{pr} (at {}) is no longer open",
                    crate::char_prefix(expected, 8)
                ));
            }
        }
    }
    if moved.is_empty() {
        None
    } else {
        Some(format!(
            "batch invalidated because refs moved during verification: {}",
            moved.join(", ")
        ))
    }
}

/// Check whether any PR in a running batch moved its head SHA while verification was running.
///
/// Returns `Some(reason)` when any PR's live head SHA differs from the SHA recorded when the
/// batch started (or if the PR disappeared / closed), refusing to mark the stale batch as passed.
pub fn batch_heads_moved(
    expected_shas: &BTreeMap<u64, String>,
    current_shas: &BTreeMap<u64, String>,
) -> Option<String> {
    batch_refs_moved("", "", expected_shas, current_shas)
        .map(|s| s.replace("refs moved", "PR heads moved"))
}

pub(crate) fn path_under(path: &str, prefix: &str) -> bool {
    let p = prefix.trim_start_matches("./");
    if p.is_empty() {
        return true;
    }
    if p.ends_with('/') {
        path.starts_with(p) || path == p.trim_end_matches('/')
    } else {
        path == p || path.starts_with(&format!("{p}/"))
    }
}

/// Determine affected verification units (packages/crates/targets) from changed file paths
/// using a configurable `(path_prefix -> unit_name)` map.
///
/// If a changed file does not match any prefix, `"all"` is returned so callers run the full area check.
pub fn affected_units(
    changed_files: &[String],
    unit_prefixes: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    if unit_prefixes.is_empty() {
        return Vec::new();
    }
    let mut units = BTreeSet::new();
    for file in changed_files {
        let norm = file.trim_start_matches("./");
        let mut matched = false;
        for (unit, prefixes) in unit_prefixes {
            if prefixes.iter().any(|p| path_under(norm, p)) {
                units.insert(unit.clone());
                matched = true;
            }
        }
        if !matched {
            return vec!["all".to_string()];
        }
    }
    units.into_iter().collect()
}

/// Structured hang report when an area check exceeds its timeout during batch verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaHangReport {
    pub batch_id: String,
    pub area: String,
    pub timeout_secs: u64,
    pub active_step: Option<String>,
    pub log_tail: String,
    pub summary: String,
}

/// Build a structured hang report when a batch area job exceeds `timeout_secs`,
/// identifying the last `=== step: <name> ===` marker and capturing the trailing output.
pub fn area_hang_report(
    batch_id: &str,
    area: &str,
    timeout_secs: u64,
    log: &str,
) -> AreaHangReport {
    let mut active_step = None;
    for line in log.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("=== step: ") {
            if let Some(step_name) = rest.strip_suffix(" ===") {
                active_step = Some(step_name.to_string());
            }
        }
    }
    let tail_lines: Vec<&str> = log.lines().rev().take(25).collect();
    let log_tail = tail_lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    let step_desc = active_step
        .as_deref()
        .map(|s| format!(" in step '{s}'"))
        .unwrap_or_default();
    let summary =
        format!("batch {batch_id} area '{area}' timed out after {timeout_secs}s{step_desc}");
    AreaHangReport {
        batch_id: batch_id.to_string(),
        area: area.to_string(),
        timeout_secs,
        active_step,
        log_tail,
        summary,
    }
}
