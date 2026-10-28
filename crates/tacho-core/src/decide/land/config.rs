//! Every project-specific value the land decisions need, with generic defaults.
//!
//! Nothing in `land` names a project path, area, tool or forge: it all comes from here
//! (loaded from `tacho.toml` by the hands).

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Configuration for the land concern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LandConfig {
    /// The command the owner types (`<tool> land <id>`, `<tool> release-train <pr>`).
    pub tool: String,
    /// Prefix of owner mail subjects (`<project>: merge #N, k changes`).
    pub project: String,
    /// `owner/repo` slug of the upstream the merge command targets.
    pub upstream: String,
    /// Title of the merge-queue issue maintained by the orchestrator.
    pub queue_issue_title: String,
    /// Head branches with this prefix are combined train PRs (never members, never rebased).
    pub train_branch_prefix: String,
    /// Regexes of paths that change how checks run. A PR touching one travels alone.
    pub ci_paths: Vec<String>,
    /// Path prefixes of user-interface code ("frontend"); such PRs merge last.
    pub ui_roots: Vec<String>,
    /// `(area, regex)` of files a tool writes (screenshots, generated docs). A conflict in one
    /// is resolved by taking the later copy.
    pub generated: Vec<(String, String)>,
    /// Exact paths written by a generator command (path -> argv-ish command string).
    pub generators: BTreeMap<String, String>,
    /// `(prefix, label)` of screenshot directories, for the body's screen list.
    pub shot_dirs: Vec<(String, String)>,
    /// `(area, prefix)`: a train touching `prefix` must also pass `area` (its redraw/suite).
    pub redraw_roots: Vec<(String, String)>,
    /// The compile gate area every train must pass.
    pub gate_area: String,
    /// Areas never required before landing (they run after merge, e.g. formal proofs).
    pub post_merge_areas: Vec<String>,
    /// `(area -> regex)` of paths that a client suite reads in a train (`TRAIN_INPUTS`).
    /// An area in this map is dropped from a train's required areas when no changed file matches.
    pub train_inputs: BTreeMap<String, String>,
    /// `(area -> regex)` of paths that a slow area reads at the single-change tier (`AREA_INPUTS`).
    pub area_inputs: BTreeMap<String, String>,
    /// Regexes of pixel/screenshot workflow files that add `pixel_areas` at the single-change tier.
    pub pixel_files: Vec<String>,
    /// Client areas added when `pixel_files` match at the single-change tier.
    pub pixel_areas: Vec<String>,
    /// `(regex, area)` added at the single-change tier when not in `skip_areas` (e.g. `\.tla$` -> `formal`).
    pub change_file_areas: Vec<(String, String)>,
    /// Areas configured to be skipped at the single-change tier (`SKIP_AREAS`).
    /// Note: on a train, required areas are still required even if listed here.
    pub skip_areas: BTreeSet<String>,
    /// Ordered compile-gate commands run on the assembled train tree.
    pub compile_steps: Vec<String>,
    /// Number of trailing log lines kept when a compile step fails and no structured excerpt matches.
    pub compile_log_lines: usize,
    /// Timeout in seconds for each compile step on the merged tree.
    pub compile_timeout_secs: u64,
    /// Backstop on merge retries per member while the forge recomputes mergeability.
    pub merge_attempts: u32,
    /// When the open-PR listing has at least this many entries it may be truncated, so
    /// "not open" cannot be trusted (abandoned-train detection is skipped).
    pub open_limit: usize,
    /// Number of first-parent main subjects inspected for merged PR numbers.
    pub merged_prs_limit: usize,
    /// Number of main commits inspected when comparing patch-ids for `change_on_main`.
    pub patch_id_depth: usize,
    /// Age window (seconds) during which a recently updated draft PR is left alone by `shape`.
    pub recent_draft_secs: i64,
    /// Cooldown (seconds) between automated main screenshot refresh PRs.
    pub stale_shots_cooldown_secs: i64,
    /// Per-channel antialiasing tolerance (`|a - b| <= pixel_edge` is ignored on non-golden shots).
    pub pixel_edge: u8,
    /// Maximum share of channels (in parts per million, `500` = `0.0005`) allowed to exceed `pixel_edge`.
    pub pixel_share_ppm: u32,
    /// Model tier for rebase tasks.
    pub rebase_model: String,
    /// Model tier for fix tasks.
    pub fix_model: String,
    /// Screenshot scope, redraw, and status-tag cap rules (`[land.shots]`).
    pub shots: super::shots::ShotRules,
}

impl Default for LandConfig {
    fn default() -> Self {
        Self {
            tool: "tacho".into(),
            project: "tacho".into(),
            upstream: String::new(),
            queue_issue_title: "Merge queue".into(),
            train_branch_prefix: "train-".into(),
            ci_paths: vec![r"^\.github/workflows/".into()],
            ui_roots: Vec::new(),
            generated: Vec::new(),
            generators: BTreeMap::new(),
            shot_dirs: Vec::new(),
            redraw_roots: Vec::new(),
            gate_area: "compile".into(),
            post_merge_areas: vec!["formal".into()],
            train_inputs: BTreeMap::new(),
            area_inputs: BTreeMap::new(),
            pixel_files: Vec::new(),
            pixel_areas: Vec::new(),
            change_file_areas: Vec::new(),
            skip_areas: BTreeSet::new(),
            compile_steps: vec!["cargo check --workspace --all-targets --locked".into()],
            compile_log_lines: 12,
            compile_timeout_secs: 3 * 3600,
            merge_attempts: 12,
            open_limit: 200,
            merged_prs_limit: 500,
            patch_id_depth: 400,
            recent_draft_secs: 20 * 60,
            stale_shots_cooldown_secs: 24 * 3600,
            pixel_edge: 24,
            pixel_share_ppm: 500,
            rebase_model: "cheap".into(),
            fix_model: "default".into(),
            shots: super::shots::ShotRules::default(),
        }
    }
}

impl LandConfig {
    /// Whether `files` change how checks run. An unparsable pattern fails closed (counts as CI).
    pub fn touches_ci<S: AsRef<str>>(&self, files: &[S]) -> bool {
        files.iter().any(|f| {
            self.ci_paths
                .iter()
                .any(|p| Regex::new(p).map_or(true, |re| re.is_match(f.as_ref())))
        })
    }

    /// The area that regenerates `path`, or `None` when it is hand-written. An unparsable
    /// pattern fails closed (never matches, so the file needs a person).
    pub fn generated_area(&self, path: &str) -> Option<&str> {
        self.generated.iter().find_map(|(area, p)| {
            Regex::new(p)
                .ok()
                .filter(|re| re.is_match(path))
                .map(|_| area.as_str())
        })
    }

    /// The areas that regenerate every file in `files`, or `None` when `files` is empty or any
    /// file in `files` is hand-written.
    pub fn generated_areas<S: AsRef<str>>(&self, files: &[S]) -> Option<BTreeSet<String>> {
        if files.is_empty() {
            return None;
        }
        let mut out = BTreeSet::new();
        for f in files {
            let area = self.generated_area(f.as_ref())?;
            out.insert(area.to_string());
        }
        Some(out)
    }

    /// Whether `path` is under a user-interface root.
    pub fn is_ui(&self, path: &str) -> bool {
        self.ui_roots.iter().any(|r| path.starts_with(r.as_str()))
    }

    /// Whether `branch` is a combined train PR's branch.
    pub fn is_train_branch(&self, branch: &str) -> bool {
        !self.train_branch_prefix.is_empty() && branch.starts_with(&self.train_branch_prefix)
    }
}

/// First `n` characters of `s` (sha abbreviations); never panics.
pub(crate) fn short(s: &str, n: usize) -> &str {
    crate::char_prefix(s, n)
}
