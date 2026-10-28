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
            r"^frontend/android/(.*\.gradle(\.kts)?|gradle\.properties|gradle/.*|.*proguard.*|.*consumer-rules\.pro|.*/AndroidManifest\.xml|.*/src/release/.*|scripts/(apk-|release-version).*)$|^scripts/areas/android\.sh$|^\.github/workflows/ci\.yaml$",
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
/// `main` (`!pull_request`) runs all of them; a `train` runs `"image"` and `"console-browser"` and
/// leaves `"android-release"` to main; any other pull request runs `"image"`, `"android-release"`,
/// and `"console-browser"` only when a touched path is something they are built from.
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
    if has_android
        && (!pull_request || (!train && paths.iter().any(|p| and_re.is_match(p.as_ref()))))
    {
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

/// Select `tests.<module>` names to run for a change under `root_prefix` (e.g. `"tools/fleet"`).
/// Delegates to [`related_package_tests_with_modules`] with an empty module source map.
pub fn related_package_tests<S: AsRef<str>>(
    paths: &[S],
    tests: &std::collections::BTreeMap<String, String>,
    root_prefix: &str,
    pkg_name: &str,
) -> Vec<String> {
    related_package_tests_with_modules(
        paths,
        tests,
        &std::collections::BTreeMap::new(),
        root_prefix,
        pkg_name,
    )
}

/// Select `tests.<module>` names to run for a change under `root_prefix` (e.g. `"tools/fleet"`),
/// following transitive intra-package module imports in `modules` and selecting tests that mention
/// non-module script files by relative path or file name (`selection/related.py::fleet`).
pub fn related_package_tests_with_modules<S: AsRef<str>>(
    paths: &[S],
    tests: &std::collections::BTreeMap<String, String>,
    modules: &std::collections::BTreeMap<String, String>,
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
    let mod_name_of = |rest: &str| -> Option<String> {
        let first = rest.split('/').next().unwrap_or("");
        let stem = first.strip_suffix(".py").unwrap_or(first);
        if !stem.is_empty()
            && stem != "__init__"
            && stem != "__main__"
            && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            Some(stem.to_string())
        } else {
            None
        }
    };
    let names_file = |text: &str, rel_under_root: &str| -> bool {
        let name = rel_under_root
            .rsplit_once('/')
            .map_or(rel_under_root, |(_, b)| b);
        if text.contains(rel_under_root) {
            return true;
        }
        if name.is_empty() || name == pkg_name {
            return false;
        }
        Regex::new(&format!(r"(?:\b|/){}\b", regex::escape(name)))
            .map(|re| re.is_match(text))
            .unwrap_or(false)
    };

    let mut mod_texts: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for (mpath, mtext) in modules {
        let rel = mpath.strip_prefix(&pkg_prefix).unwrap_or(mpath.as_str());
        if let Some(mname) = mod_name_of(rel) {
            let entry = mod_texts.entry(mname).or_default();
            if !entry.is_empty() {
                entry.push('\n');
            }
            entry.push_str(mtext);
        }
    }

    let importers = |start: &str| -> BTreeSet<String> {
        let mut seen = BTreeSet::from([start.to_string()]);
        let mut pending = vec![start.to_string()];
        while let Some(cur) = pending.pop() {
            for (mod_name, text) in &mod_texts {
                if !seen.contains(mod_name) && imports_module(text, &cur) {
                    seen.insert(mod_name.clone());
                    pending.push(mod_name.clone());
                }
            }
        }
        seen
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
                if let Some(m) = mod_name_of(rest) {
                    for mod_name in importers(&m) {
                        for (t, text) in tests {
                            if imports_module(text, &mod_name) {
                                chosen.insert(test_name(t));
                            }
                        }
                    }
                    continue;
                }
            }
            let rel = path.strip_prefix(&root).unwrap_or(path);
            for (t, text) in tests {
                if names_file(text, rel) {
                    chosen.insert(test_name(t));
                }
            }
        } else {
            let rel = path.strip_prefix(&root).unwrap_or(path);
            for (t, text) in tests {
                if names_file(text, rel) {
                    chosen.insert(test_name(t));
                }
            }
        }
    }
    chosen.into_iter().map(|n| format!("tests.{n}")).collect()
}

/// Computes the incremental Rust crate/test targets affected by `paths` given workspace crate
/// manifests (`crate_name -> Cargo.toml text`), matching `selection/changed_tests.py::rust_targets`:
/// - `crate` for each crate reached by source changes (including reverse-dependency closure),
/// - `crate` (without reverse dependents) when non-top-level test helper files under `crates/<crate>/tests/...` changed,
/// - `crate --test binary` when only `crates/<crate>/tests/<binary>.rs` changed and `<crate>` is not in `whole`,
/// - `["*"]` when `fuzz/`, a migration (`.../migrations/...`), workspace root file (`Cargo.toml`, `Cargo.lock`), or unknown crate is touched.
pub fn affected_rust_targets_from_manifests<S: AsRef<str>>(
    paths: &[S],
    manifests: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let mut parsed_docs: std::collections::BTreeMap<String, toml::Table> =
        std::collections::BTreeMap::new();
    let mut pkg_to_crate: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    for (dir_name, text) in manifests {
        pkg_to_crate.insert(dir_name.clone(), dir_name.clone());
        if let Ok(doc) = text.parse::<toml::Table>() {
            if let Some(pkg_name) = doc
                .get("package")
                .and_then(|p| p.as_table())
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
            {
                pkg_to_crate.insert(pkg_name.to_string(), dir_name.clone());
            }
            parsed_docs.insert(dir_name.clone(), doc);
        }
    }

    let crates: BTreeSet<&str> = manifests.keys().map(String::as_str).collect();
    let mut above: std::collections::BTreeMap<String, BTreeSet<String>> = crates
        .iter()
        .map(|&c| (c.to_string(), BTreeSet::new()))
        .collect();

    let record_dep_table =
        |name: &str,
         table: &toml::Table,
         above: &mut std::collections::BTreeMap<String, BTreeSet<String>>| {
            for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(deps) = table.get(kind).and_then(|v| v.as_table()) {
                    for (dep_key, dep_val) in deps {
                        let candidate = dep_val
                            .as_table()
                            .and_then(|t| t.get("package"))
                            .and_then(|p| p.as_str())
                            .unwrap_or(dep_key.as_str());
                        for key in [dep_key.as_str(), candidate] {
                            if let Some(target_crate) = pkg_to_crate.get(key) {
                                if target_crate != name {
                                    above
                                        .entry(target_crate.clone())
                                        .or_default()
                                        .insert(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        };

    for (name, doc) in &parsed_docs {
        record_dep_table(name, doc, &mut above);
        if let Some(targets) = doc.get("target").and_then(|t| t.as_table()) {
            for target_val in targets.values() {
                if let Some(target_table) = target_val.as_table() {
                    record_dep_table(name, target_table, &mut above);
                }
            }
        }
    }

    let reaching = |starts: &BTreeSet<String>| -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut queue: Vec<String> = starts.iter().cloned().collect();
        while let Some(cur) = queue.pop() {
            if seen.insert(cur.clone()) {
                if let Some(dependents) = above.get(&cur) {
                    for dep in dependents {
                        if !seen.contains(dep) {
                            queue.push(dep.clone());
                        }
                    }
                }
            }
        }
        seen
    };

    let mut source: BTreeSet<String> = BTreeSet::new();
    let mut own: BTreeSet<String> = BTreeSet::new();
    let mut only_test: std::collections::BTreeMap<String, BTreeSet<String>> =
        std::collections::BTreeMap::new();

    for raw in paths {
        let path = raw.as_ref();
        if !path.starts_with("crates/")
            && !path.starts_with("fuzz/")
            && path != "Cargo.toml"
            && path != "Cargo.lock"
        {
            continue;
        }
        if !path.starts_with("crates/") {
            return vec![EVERYTHING_TESTS.to_string()];
        }
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() < 3 {
            return vec![EVERYTHING_TESTS.to_string()];
        }
        let crate_name = parts[1];
        if !crates.contains(crate_name) || path.contains("/migrations/") {
            return vec![EVERYTHING_TESTS.to_string()];
        }
        if parts[2] == "tests" && parts.len() == 4 && parts[3].ends_with(".rs") {
            let bin = parts[3].trim_end_matches(".rs").to_string();
            only_test
                .entry(crate_name.to_string())
                .or_default()
                .insert(bin);
        } else if parts[2] == "tests" {
            // Non-top-level test helper under `crates/<crate>/tests/...`: reaches only its own crate.
            own.insert(crate_name.to_string());
        } else {
            source.insert(crate_name.to_string());
        }
    }

    let mut whole = reaching(&source);
    whole.extend(own);

    let mut targets: Vec<String> = whole.iter().cloned().collect();
    for (crate_name, bins) in only_test {
        if !whole.contains(&crate_name) {
            for binary in bins {
                targets.push(format!("{crate_name} --test {binary}"));
            }
        }
    }
    targets
}

/// Reads `crates/*/Cargo.toml` under `repo_root` and returns [`affected_rust_targets_from_manifests`].
pub fn affected_rust_targets<S: AsRef<str>>(
    repo_root: &std::path::Path,
    paths: &[S],
) -> Vec<String> {
    let mut manifests = std::collections::BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(repo_root.join("crates")) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let toml_path = p.join("Cargo.toml");
                if let (Some(name), Ok(text)) = (
                    p.file_name().and_then(|n| n.to_str()),
                    std::fs::read_to_string(toml_path),
                ) {
                    manifests.insert(name.to_string(), text);
                }
            }
        }
    }
    affected_rust_targets_from_manifests(paths, &manifests)
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

fn screen_stem(filename: &str) -> Option<String> {
    static SCREEN_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re =
        SCREEN_RE.get_or_init(|| Regex::new(r"^([A-Za-z0-9]+)Screen\.tsx$").expect("static regex"));
    let cap = re.captures(filename)?;
    let mut word = cap[1].to_ascii_lowercase();
    if word.len() > 3 && word.ends_with('s') {
        word.pop();
    }
    Some(word)
}

/// Select the Playwright spec files (`e2e/<name>.spec.ts`) for a console change, or `["*"]`
/// when shared code, shell, or fixtures changed (`selection/related.py::console_specs`).
pub fn related_console_specs<S: AsRef<str>>(
    paths: &[S],
    specs: &std::collections::BTreeMap<String, String>,
    admin_prefix: &str,
) -> Vec<String> {
    let admin = format!("{}/", admin_prefix.trim_end_matches('/'));
    let e2e_prefix = format!("{admin}e2e/");
    let screens_prefix = format!("{admin}src/screens/");
    let changed: Vec<&str> = paths
        .iter()
        .map(AsRef::as_ref)
        .filter(|p| p.starts_with(&admin))
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    let mut chosen = BTreeSet::new();
    let mut stems = BTreeSet::new();
    for path in changed {
        if let Some(rest) = path.strip_prefix(&e2e_prefix) {
            if !rest.contains('/') && rest.ends_with(".spec.ts") {
                chosen.insert(format!("e2e/{rest}"));
                continue;
            }
            return vec![EVERYTHING_TESTS.to_string()];
        }
        if let Some(rest) = path.strip_prefix(&screens_prefix) {
            if !rest.contains('/') {
                if let Some(stem) = screen_stem(rest) {
                    stems.insert(stem);
                    continue;
                }
            }
            return vec![EVERYTHING_TESTS.to_string()];
        }
        return vec![EVERYTHING_TESTS.to_string()];
    }
    for (spec, text) in specs {
        let rel = spec.strip_prefix(&admin).unwrap_or(spec.as_str());
        let base = rel
            .rsplit_once('/')
            .map_or(rel, |(_, b)| b)
            .to_ascii_lowercase();
        let lower = text.to_ascii_lowercase();
        if stems
            .iter()
            .any(|s| base.contains(s.as_str()) || lower.contains(s.as_str()))
        {
            chosen.insert(rel.to_string());
        }
    }
    chosen.into_iter().collect()
}

/// Rule mapping a deploy/scripts path regex to the deploy check names it exercises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployCheckSpec<'a> {
    pub pattern: &'a str,
    pub checks: &'a [&'a str],
}

/// Default deploy check rules (`selection/related.py::_DEPLOY_CHECKS`).
pub const DEFAULT_DEPLOY_CHECKS: &[DeployCheckSpec<'static>] = &[
    DeployCheckSpec {
        pattern: r"^deploy/(prometheus|alertmanager|grafana)/|^scripts/validate-observability\.sh$",
        checks: &["observability"],
    },
    DeployCheckSpec {
        pattern: r"^deploy/Dockerfile$|^scripts/verify-dockerfile\.sh$",
        checks: &["dockerfile"],
    },
    DeployCheckSpec {
        pattern: r"^deploy/docker-compose\.prod\.ya?ml$|^scripts/verify-prod-compose\.sh$",
        checks: &["prod-compose", "observability"],
    },
    DeployCheckSpec {
        pattern: r"^deploy/helm/|^scripts/validate-helm\.sh$",
        checks: &["helm", "network-policies"],
    },
    DeployCheckSpec {
        pattern: r"^scripts/verify-network-policies\.sh$",
        checks: &["network-policies"],
    },
    DeployCheckSpec {
        pattern: r"^deploy/terraform/|^scripts/validate-terraform\.sh$",
        checks: &["terraform"],
    },
    DeployCheckSpec {
        pattern: r"^scripts/verify-runbooks\.sh$",
        checks: &["runbooks"],
    },
    DeployCheckSpec {
        pattern: r"^scripts/(backup|restore|offsite).*\.sh$",
        checks: &["backup-syntax", "restore-drill"],
    },
    DeployCheckSpec {
        pattern: r"^scripts/(deploy|rollback|smoke-test|rotate-keys|verify-backup)\.sh$",
        checks: &["backup-syntax"],
    },
];

/// Select the deploy check names to run for a change under `deploy/` or `scripts/`, or `["*"]`
/// when `scripts/areas/deploy.sh` or an unmatched deploy/scripts path is touched.
pub fn related_deploy_checks<S: AsRef<str>>(
    paths: &[S],
    rules: &[DeployCheckSpec<'_>],
) -> Vec<String> {
    let mut chosen = BTreeSet::new();
    let compiled: Vec<(Regex, &[&str])> = rules
        .iter()
        .filter_map(|r| Regex::new(r.pattern).ok().map(|re| (re, r.checks)))
        .collect();
    for raw in paths {
        let path = raw.as_ref();
        if !(path.starts_with("deploy/") || path.starts_with("scripts/")) {
            continue;
        }
        if path == "scripts/areas/deploy.sh" {
            return vec![EVERYTHING_TESTS.to_string()];
        }
        let mut matched = false;
        for (re, names) in &compiled {
            if re.is_match(path) {
                for &n in *names {
                    chosen.insert(n.to_string());
                }
                matched = true;
            }
        }
        if !matched {
            return vec![EVERYTHING_TESTS.to_string()];
        }
    }
    let mut order = Vec::new();
    for r in rules {
        for &n in r.checks {
            if chosen.contains(n) && !order.iter().any(|x| x == n) {
                order.push(n.to_string());
            }
        }
    }
    order
}
