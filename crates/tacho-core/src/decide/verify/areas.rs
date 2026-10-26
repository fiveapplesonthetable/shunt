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

// =============================================================================
// Costly Area Parts & Related Unit Test Selection (`since.py`, `related.py`)
// =============================================================================

pub const COSTLY_PARTS: &[&str] = &["android-release", "console-browser"];
pub const EVERYTHING_TESTS: &str = "*";

fn image_inputs_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^deploy/|^\.dockerignore$|^Cargo\.(toml|lock)$|^crates/[^/]+/Cargo\.toml$|^\.sqlx/|^rust-toolchain(\.toml)?$|^\.cargo/|^scripts/(backup|restore-drill|offsite-restore-check|push-image|reuse-image)",
        )
        .expect("static regex")
    })
}

fn android_release_inputs_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^frontend/android/(.*\.gradle(\.kts)?|gradle\.properties|gradle/.*|.*proguard.*|.*consumer-rules\.pro|.*/AndroidManifest\.xml|.*/src/release/.*|scripts/(apk-|release-version).*)$|^scripts/areas/android\.sh$",
        )
        .expect("static regex")
    })
}

fn console_browser_idle_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^frontend/admin/(screenshots/|docs/|screenshots\.golden$)|^frontend/admin/[^/]*\.md$",
        )
        .expect("static regex")
    })
}

/// Filter `found` areas with the release image and costly parts (`android-release`, `console-browser`)
/// set for this run (`selection/since.py::costly`).
///
/// `main` (`!pull_request`) and a `train` run all of them; any other pull request runs `"image"`,
/// `"android-release"`, and `"console-browser"` only when a touched path is something they are built from.
pub fn select_costly_areas<S1: AsRef<str>, S2: AsRef<str>>(
    found: &[S1],
    paths: &[S2],
    pull_request: bool,
    train: bool,
) -> Vec<String> {
    let full = train || !pull_request;
    let img_re = image_inputs_re();
    let wanted_image = full || paths.iter().any(|p| img_re.is_match(p.as_ref()));
    let mut out: Vec<String> = Vec::new();
    let mut push_unique = |item: &str| {
        if !out.iter().any(|x| x == item) {
            out.push(item.to_string());
        }
    };
    for a in found {
        let a = a.as_ref();
        if a == "android-release" || a == "console-browser" {
            continue;
        }
        if a != "image" || wanted_image {
            push_unique(a);
        }
    }
    if pull_request && wanted_image {
        push_unique("image");
    }
    let has_android = found
        .iter()
        .any(|a| matches!(a.as_ref(), "android" | "android-release"));
    let and_re = android_release_inputs_re();
    if has_android && (full || paths.iter().any(|p| and_re.is_match(p.as_ref()))) {
        push_unique("android-release");
    }
    let has_console = found
        .iter()
        .any(|a| matches!(a.as_ref(), "console" | "console-browser"));
    let idle_re = console_browser_idle_re();
    let console_paths: Vec<&str> = paths
        .iter()
        .map(AsRef::as_ref)
        .filter(|p| p.starts_with("frontend/admin/"))
        .collect();
    if has_console && (full || console_paths.iter().any(|p| !idle_re.is_match(p))) {
        push_unique("console-browser");
    }
    out
}

/// Select `tests.<module>` names to run for a change under `root_prefix` (e.g. `"tools/fleet"`),
/// or `["*"]` when a touched path under `root_prefix` is not a module or test (`selection/related.py::fleet`).
pub fn related_package_tests<S: AsRef<str>>(
    paths: &[S],
    tests: &std::collections::BTreeMap<String, String>,
    root_prefix: &str,
    pkg_name: &str,
) -> Vec<String> {
    static IMPORT_LINE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let import_line_re =
        IMPORT_LINE_RE.get_or_init(|| Regex::new(r"(?m)^\s*(?:from|import)\s.*$").unwrap());

    let root = format!("{}/", root_prefix.trim_end_matches('/'));
    let pkg_prefix = format!("{root}{pkg_name}/");
    let tests_prefix = format!("{root}tests/");

    let test_name = |test_path: &str| -> String {
        let base = test_path.rsplit_once('/').map_or(test_path, |(_, b)| b);
        base.strip_suffix(".py").unwrap_or(base).to_string()
    };
    let imports_module = |text: &str, module: &str| -> bool {
        let Ok(word_re) = Regex::new(&format!(r"\b{}\b", regex::escape(module))) else {
            return false;
        };
        import_line_re
            .find_iter(text)
            .any(|m| word_re.is_match(m.as_str()))
    };

    let mut chosen = BTreeSet::new();
    for raw in paths {
        let path = raw.as_ref();
        if !path.starts_with(&root) {
            continue;
        }
        if let Some(rest) = path.strip_prefix(&tests_prefix) {
            if let Some(stem) = rest.strip_suffix(".py") {
                if !stem.contains('/') && stem.starts_with("test_") {
                    chosen.insert(stem.to_string());
                    continue;
                }
            }
            for t in tests.keys() {
                chosen.insert(test_name(t));
            }
        } else if let Some(rest) = path.strip_prefix(&pkg_prefix) {
            if rest.ends_with(".py") {
                let first = rest.split('/').next().unwrap_or("");
                let mod_name = first.strip_suffix(".py").unwrap_or(first);
                if !mod_name.is_empty()
                    && mod_name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    for (t, text) in tests {
                        if imports_module(text, mod_name) {
                            chosen.insert(test_name(t));
                        }
                    }
                    continue;
                }
            }
            return vec![EVERYTHING_TESTS.to_string()];
        } else {
            return vec![EVERYTHING_TESTS.to_string()];
        }
    }
    chosen.into_iter().map(|n| format!("tests.{n}")).collect()
}

fn normalise_rel_path(path: &str) -> String {
    let mut out = Vec::new();
    for part in path.split('/') {
        if part == ".." {
            out.pop();
        } else if part != "." && !part.is_empty() {
            out.push(part);
        }
    }
    out.join("/")
}

/// Select the console unit test files (relative to `admin_prefix`, e.g. `"frontend/admin/"`)
/// whose relative imports reach a changed source module, or `["*"]` when console configuration
/// changed (`selection/related.py::console`).
pub fn related_console_tests<S: AsRef<str>>(
    paths: &[S],
    modules: &std::collections::BTreeMap<String, String>,
    tests: &BTreeSet<String>,
    admin_prefix: &str,
) -> Vec<String> {
    static CONFIG_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static IMPORT_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let config_re = CONFIG_RE.get_or_init(|| {
        Regex::new(r"(package(-lock)?\.json|tsconfig[^/]*\.json|vite\.config|\.mjs$)").unwrap()
    });
    let import_re = IMPORT_RE
        .get_or_init(|| Regex::new(r#"(?:from|import)\s*\(?\s*['"](\.[^'"]+)['"]"#).unwrap());

    let admin = format!("{}/", admin_prefix.trim_end_matches('/'));
    let changed: Vec<&str> = paths
        .iter()
        .map(AsRef::as_ref)
        .filter(|p| p.starts_with(&admin))
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    for &path in &changed {
        if config_re.is_match(path) && !path.contains("/tests/") {
            return vec![EVERYTHING_TESTS.to_string()];
        }
    }
    let src_prefix = format!("{admin}src/");
    let sources: BTreeSet<&str> = changed
        .iter()
        .copied()
        .filter(|p| p.starts_with(&src_prefix))
        .collect();
    let mut chosen: BTreeSet<String> = changed
        .iter()
        .filter(|&&p| tests.contains(p))
        .map(|&p| p.to_string())
        .collect();

    let imports_of = |path: &str| -> Vec<String> {
        let base = path.rsplit_once('/').map_or("", |(b, _)| b);
        let Some(text) = modules.get(path) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for cap in import_re.captures_iter(text) {
            let rel = &cap[1];
            let joined = normalise_rel_path(&format!("{base}/{rel}"));
            let candidates = [
                joined.clone(),
                format!("{joined}.ts"),
                format!("{joined}.tsx"),
                format!("{joined}.mjs"),
                format!("{joined}.js"),
                format!("{joined}/index.ts"),
                format!("{joined}/index.tsx"),
            ];
            for cand in candidates {
                if modules.contains_key(&cand) {
                    found.push(cand);
                    break;
                }
            }
        }
        found
    };

    let closure = |start: &str| -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![start.to_string()];
        while let Some(cur) = pending.pop() {
            for target in imports_of(&cur) {
                if seen.insert(target.clone()) {
                    pending.push(target);
                }
            }
        }
        seen
    };

    for test in tests {
        if !sources.is_empty() {
            let text = modules.get(test).map(String::as_str).unwrap_or("");
            let reads_files = text.contains("node:fs") || text.contains("readFileSync");
            if reads_files || closure(test).iter().any(|m| sources.contains(m.as_str())) {
                chosen.insert(test.clone());
            }
        }
    }

    chosen
        .into_iter()
        .map(|p| p.strip_prefix(&admin).unwrap_or(&p).to_string())
        .collect()
}
