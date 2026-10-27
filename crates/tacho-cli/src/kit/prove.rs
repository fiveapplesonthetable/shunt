//! `tacho prove`: maps `git diff` against base to checks (`area` + affected `units`),
//! stops at first failure, writes `.tachoproof/summary.json` recording the git tree hash
//! of tested files so any subsequent commit makes proof stale, and enforces screenshot scope rules.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tacho_core::decide::verify::{checks_for_files, AreaRule, ProofCheck, ProofSummary};
use tacho_core::Scrubber;

use super::procs::signal_group_and_leader;
use super::use_private_tmp;
use super::{
    emit_decision_event, find_worktree_root, is_internal_tacho_path, load_worktree_env, now_ns,
    now_secs, open_fleet_store, resolve_base_sha_for_worktree, tree_of_worktree, write_atomic,
    FdEventWatcher,
};
use crate::cli::ProveArgs;

pub const PROVE_STEP_TIMEOUT_SECS: u64 = 3600;

/// A planned verification check derived from changed files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCheck {
    pub name: String,
    pub cmd: String,
    pub area: String,
    pub units: Vec<String>,
}

fn parse_name_status_lines(raw: &str, out: &mut BTreeSet<String>) {
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Format: `<status>\t<path>` or `R100\t<old_path>\t<new_path>`
        let parts: Vec<&str> = trimmed.split('\t').collect();
        for p in parts.into_iter().skip(1) {
            let clean = p.trim();
            if !clean.is_empty() && !is_internal_tacho_path(clean) {
                out.insert(clean.to_string());
            }
        }
    }
}

/// Returns the sorted list of changed and untracked files relative to `base_sha`
/// (including committed, staged, unstaged, untracked, and both sides of renames).
pub fn changed_files_since(root: &Path, base_sha: &str) -> io::Result<Vec<String>> {
    let mut files = BTreeSet::new();

    // 1. Committed + working-tree diff against base_sha (including both old & new on rename)
    let diff_base = Command::new("git")
        .args(["diff", "--name-status", "-M", base_sha])
        .current_dir(root)
        .output()?;
    if diff_base.status.success() {
        parse_name_status_lines(&String::from_utf8_lossy(&diff_base.stdout), &mut files);
    }

    // 2. Unstaged working tree changes
    let diff_wt = Command::new("git")
        .args(["diff", "--name-status", "-M"])
        .current_dir(root)
        .output()?;
    if diff_wt.status.success() {
        parse_name_status_lines(&String::from_utf8_lossy(&diff_wt.stdout), &mut files);
    }

    // 3. Staged index changes
    let diff_cached = Command::new("git")
        .args(["diff", "--cached", "--name-status", "-M"])
        .current_dir(root)
        .output()?;
    if diff_cached.status.success() {
        parse_name_status_lines(&String::from_utf8_lossy(&diff_cached.stdout), &mut files);
    }

    // 4. Untracked files
    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()?;
    if untracked.status.success() {
        for line in String::from_utf8_lossy(&untracked.stdout).lines() {
            let p = line.trim();
            if !p.is_empty() && !is_internal_tacho_path(p) {
                files.insert(p.to_string());
            }
        }
    }

    Ok(files.into_iter().collect())
}

/// If `path` is a `.png` screenshot, extract the screen name it belongs to.
///
/// Recognizes paths like:
/// - `screenshots/<screen>/<file>.png`
/// - `shots/<screen>/<file>.png`
/// - `tests/screenshots/<screen>/<file>.png`
/// - `**/screenshots/<screen>/<file>.png` or `**/shots/<screen>/<file>.png`
/// - `screenshots/<screen>.png` or `shots/<screen>.png`
pub fn extract_screen_from_screenshot_path(path: &str) -> Option<String> {
    if !path.ends_with(".png") {
        return None;
    }
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for (idx, &seg) in parts.iter().enumerate() {
        if matches!(
            seg,
            "screenshots" | "shots" | "__screenshots__" | "snapshots"
        ) {
            if idx + 2 < parts.len() {
                return Some(parts[idx + 1].to_string());
            } else if idx + 1 < parts.len() {
                let stem = parts[idx + 1]
                    .strip_suffix(".png")
                    .unwrap_or(parts[idx + 1]);
                let screen = stem.split(['-', '_']).next().unwrap_or(stem);
                return Some(screen.to_string());
            }
        }
    }
    None
}

/// Read golden screenshot paths listed in `.golden` / `screenshots/.golden` / `shots/.golden` if present.
pub fn load_golden_screenshots(root: &Path) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for rel in [
        ".golden",
        "screenshots/.golden",
        "shots/.golden",
        "tests/screenshots/.golden",
    ] {
        let p = root.join(rel);
        if let Ok(txt) = fs::read_to_string(&p) {
            for line in txt.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    set.insert(trimmed.to_string());
                }
            }
        }
    }
    set
}

/// Compute the set of screen names touched by non-PNG source files in `changed_files`.
pub fn touched_screens_for_files(changed_files: &[String]) -> BTreeSet<String> {
    let mut screens = BTreeSet::new();
    for f in changed_files {
        if f.ends_with(".png") || f.ends_with(".golden") {
            continue;
        }
        for part in f.split('/') {
            let stem = part.rsplit_once('.').map(|(s, _)| s).unwrap_or(part).trim();
            if !stem.is_empty() {
                screens.insert(stem.to_string());
                for token in stem.split(['-', '_']) {
                    if !token.is_empty() {
                        screens.insert(token.to_string());
                    }
                }
            }
        }
    }
    screens
}

/// Identify `.png` screenshot files in `changed_files` that are outside the touched screens
/// and not listed in `.golden`.
pub fn stray_screenshots_for_files(root: &Path, changed_files: &[String]) -> Vec<String> {
    let golden = load_golden_screenshots(root);
    let touched_screens = touched_screens_for_files(changed_files);
    let mut stray = Vec::new();

    for f in changed_files {
        if !f.ends_with(".png") {
            continue;
        }
        if golden.contains(f) {
            continue;
        }
        if let Some(screen) = extract_screen_from_screenshot_path(f) {
            if !touched_screens.contains(&screen) {
                stray.push(f.clone());
            }
        } else if touched_screens.is_empty() {
            // Standalone PNG with no source files touched at all
            stray.push(f.clone());
        }
    }
    stray
}

/// Exclude stray `.png` screenshots when mapping changed files to area checks so a stray
/// screenshot does not trigger unrelated test suites.
pub fn filter_files_for_area_mapping(root: &Path, changed_files: &[String]) -> Vec<String> {
    let stray: BTreeSet<String> = stray_screenshots_for_files(root, changed_files)
        .into_iter()
        .collect();
    changed_files
        .iter()
        .filter(|f| !stray.contains(*f))
        .cloned()
        .collect()
}

/// Map changed files in a Rust workspace (`crates/<crate>/...`) to targeted checks (`area` + `units`).
pub fn plan_rust_unit_checks(changed_files: &[String]) -> Vec<PlannedCheck> {
    let mut workspace_wide = false;
    let mut whole_crates: BTreeSet<String> = BTreeSet::new();
    let mut crate_modules: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut crate_test_suites: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for f in changed_files {
        if f == "Cargo.toml" || f == "Cargo.lock" || f.starts_with(".cargo/") {
            workspace_wide = true;
            continue;
        }
        let parts: Vec<&str> = f.split('/').collect();
        if parts.len() >= 3 && parts[0] == "crates" {
            let krate = parts[1].to_string();
            let rel = &parts[2..];
            if rel[0] == "Cargo.toml"
                || rel[0] == "build.rs"
                || rel[0] == "migrations"
                || (rel[0] == "src"
                    && rel.len() == 2
                    && (rel[1] == "lib.rs" || rel[1] == "main.rs"))
            {
                whole_crates.insert(krate);
            } else if rel[0] == "src" && rel.len() >= 2 {
                let mod_name = rel[1].strip_suffix(".rs").unwrap_or(rel[1]).to_string();
                crate_modules.entry(krate).or_default().insert(mod_name);
            } else if rel[0] == "tests" && rel.len() >= 2 {
                let suite = rel[1].strip_suffix(".rs").unwrap_or(rel[1]).to_string();
                crate_test_suites.entry(krate).or_default().insert(suite);
            } else {
                whole_crates.insert(krate);
            }
        }
    }

    if workspace_wide {
        return vec![PlannedCheck {
            name: "workspace:cargo-test".to_string(),
            cmd: "cargo test --workspace".to_string(),
            area: "workspace".to_string(),
            units: vec!["workspace".to_string()],
        }];
    }

    let mut checks = Vec::new();
    let all_crates: BTreeSet<String> = whole_crates
        .iter()
        .chain(crate_modules.keys())
        .chain(crate_test_suites.keys())
        .cloned()
        .collect();

    for krate in all_crates {
        if whole_crates.contains(&krate) {
            checks.push(PlannedCheck {
                name: format!("{krate}:test"),
                cmd: format!("cargo test -p {krate}"),
                area: krate.clone(),
                units: vec![krate],
            });
            continue;
        }
        if let Some(mods) = crate_modules.get(&krate) {
            for m in mods {
                let unit = format!("{krate}::{m}");
                checks.push(PlannedCheck {
                    name: format!("{krate}:lib:{m}"),
                    cmd: format!("cargo test -p {krate} --lib -- {m}::"),
                    area: krate.clone(),
                    units: vec![unit],
                });
            }
        }
        if let Some(suites) = crate_test_suites.get(&krate) {
            for s in suites {
                let unit = format!("{krate}::test::{s}");
                checks.push(PlannedCheck {
                    name: format!("{krate}:test:{s}"),
                    cmd: format!("cargo test -p {krate} --test {s}"),
                    area: krate.clone(),
                    units: vec![unit],
                });
            }
        }
    }
    checks
}

/// Plan the ordered list of checks (`PlannedCheck`) for `changed_files` against `area_rules`.
pub fn plan_prove_checks(
    root: &Path,
    changed_files: &[String],
    area_rules: &[AreaRule],
) -> Vec<PlannedCheck> {
    let filtered = filter_files_for_area_mapping(root, changed_files);
    if filtered.is_empty() {
        return Vec::new();
    }

    let mut planned = Vec::new();
    if !area_rules.is_empty() {
        let area_checks = checks_for_files(&filtered, area_rules);
        for ac in area_checks {
            if let Some(rule) = area_rules.iter().find(|r| r.area == ac.area) {
                if !rule.checks.is_empty() {
                    for (chk_name, chk_cmd) in &rule.checks {
                        let units_joined = ac.units.join(",");
                        let resolved_cmd = chk_cmd.replace("{units}", &units_joined);
                        planned.push(PlannedCheck {
                            name: format!("{}:{chk_name}", ac.area),
                            cmd: resolved_cmd,
                            area: ac.area.clone(),
                            units: ac.units.clone(),
                        });
                    }
                } else {
                    for u in &ac.units {
                        planned.push(PlannedCheck {
                            name: format!("{}:{u}", ac.area),
                            cmd: format!("cargo test -p {u}"),
                            area: ac.area.clone(),
                            units: vec![u.clone()],
                        });
                    }
                }
            }
        }
    }

    if planned.is_empty() {
        let rust_checks = plan_rust_unit_checks(&filtered);
        if !rust_checks.is_empty() {
            return rust_checks;
        }
        // Check if Python package or console tests in the worktree match via related test discovery
        if root.join("tools/tacho/tests").is_dir() {
            let mut test_contents = BTreeMap::new();
            if let Ok(entries) = fs::read_dir(root.join("tools/tacho/tests")) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("py") {
                        if let (Some(fname), Ok(txt)) = (
                            p.file_name().and_then(|n| n.to_str()),
                            fs::read_to_string(&p),
                        ) {
                            test_contents.insert(format!("tools/tacho/tests/{fname}"), txt);
                        }
                    }
                }
            }
            let related = tacho_core::decide::verify::related_package_tests(
                &filtered,
                &test_contents,
                "tools/tacho",
                "tacho",
            );
            if !related.is_empty() {
                planned.push(PlannedCheck {
                    name: "tacho:unit".to_string(),
                    cmd: format!("python3 -m unittest {}", related.join(" ")),
                    area: "tacho".to_string(),
                    units: related,
                });
                return planned;
            }
        }
        if root.join("Cargo.toml").is_file() {
            planned.push(PlannedCheck {
                name: "workspace:cargo-check".to_string(),
                cmd: "cargo check --workspace".to_string(),
                area: "workspace".to_string(),
                units: vec!["workspace".to_string()],
            });
        } else {
            planned.push(PlannedCheck {
                name: "workspace:git-status".to_string(),
                cmd: "git status --short".to_string(),
                area: "workspace".to_string(),
                units: vec!["workspace".to_string()],
            });
        }
    }

    planned
}

/// Execute a single proof check command in its own process group (`process_group(0)`),
/// bounded by `timeout_secs` using [`FdEventWatcher`] (kernel event poll).
///
/// Returns `(exit_code, duration_ms, output_tail)`.
/// - On timeout: sends `SIGKILL` to `-pgid`, reaps child, returns `124`.
/// - On missing binary (`NotFound`): returns `127`.
pub fn run_prove_step(
    root: &Path,
    cmd_str: &str,
    env_vars: &BTreeMap<String, String>,
    area: &str,
    units: &[String],
    timeout_secs: u64,
) -> io::Result<(i32, u64, String)> {
    let t0 = Instant::now();
    let out_tmp = tempfile::NamedTempFile::new()?;
    let out_file = out_tmp.reopen()?;
    let err_file = out_tmp.reopen()?;

    let mut cmd = Command::new("bash");
    use_private_tmp(&mut cmd, "prove")?;
    cmd.args(["-c", cmd_str])
        .current_dir(root)
        .env("TACHO_AREA", area)
        .env("TACHO_UNITS", units.join(","))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .process_group(0);
    for (k, v) in env_vars {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((127, 0, format!("executable not found: {e}")));
        }
        Err(e) => return Err(e),
    };

    let pid = child.id();
    let mut watcher = FdEventWatcher::new()?;
    let _ = watcher.register_pid(pid);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));

    let exit_code = loop {
        if let Some(status) = child.try_wait()? {
            break status.code().unwrap_or(1);
        }
        let now = Instant::now();
        if now >= deadline {
            signal_group_and_leader(pid, libc::SIGKILL)?;
            child.wait()?;
            break 124;
        }
        let rem_ms = deadline.saturating_duration_since(now).as_millis().min(500) as i32;
        watcher.wait_ms(rem_ms.max(1));
    };

    let duration_ms = t0.elapsed().as_millis() as u64;
    let mut raw_output = String::new();
    let _ = File::open(out_tmp.path()).and_then(|mut f| f.read_to_string(&mut raw_output));
    let scrubbed = Scrubber::new().scrub(&raw_output);
    let tail_lines: Vec<&str> = scrubbed.lines().rev().take(40).collect();
    let mut tail: String = tail_lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    if exit_code == 124 {
        if !tail.is_empty() {
            tail.push('\n');
        }
        tail.push_str(&format!(
            "[tacho prove] check exceeded {timeout_secs}s timeout cap (exit 124)"
        ));
    }

    Ok((exit_code, duration_ms, tail))
}

/// Executes `tacho prove [--base <sha>] [--keep-going]`.
pub fn execute_prove(args: ProveArgs) -> io::Result<()> {
    let store = open_fleet_store()?;
    let state = store.load()?;
    let cwd = std::env::current_dir()?;
    let root = find_worktree_root(&cwd);
    let mut env_map = load_worktree_env(&root);

    let base_sha = resolve_base_sha_for_worktree(&root, args.base.as_deref(), &env_map, &state)?;
    env_map.insert("TACHO_BASE".to_string(), base_sha.clone());
    let changed = changed_files_since(&root, &base_sha)?;
    if changed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no changed files relative to base {base_sha}"),
        ));
    }
    let _ = super::verify_and_plan_worktree_migrations(&root)?;

    let stray_shots = stray_screenshots_for_files(&root, &changed);
    if !stray_shots.is_empty() {
        eprintln!(
            "[tacho prove] notice: ignoring stray screenshots outside touched screens for area mapping: {:?}",
            stray_shots
        );
    }

    let planned = plan_prove_checks(&root, &changed, &state.area_rules);
    if planned.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "no checks matched changed files {:?} (only stray screenshots were modified)",
                changed
            ),
        ));
    }

    let timeout_secs = std::env::var("TACHO_PROVE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(PROVE_STEP_TIMEOUT_SECS);

    let mut executed_checks = Vec::new();
    let mut all_ok = true;

    for item in planned {
        println!("==> [{}] {} (units: {:?})", item.area, item.cmd, item.units);
        let start_ns = now_ns();
        let (exit_code, duration_ms, output_tail) = run_prove_step(
            &root,
            &item.cmd,
            &env_map,
            &item.area,
            &item.units,
            timeout_secs,
        )?;
        let end_ns = now_ns();
        let ok = exit_code == 0;
        store.emit_duration_slice(
            &format!("prove:{}", item.area),
            &item.name,
            start_ns,
            end_ns,
            &format!("cmd=`{}` ok={ok}", item.cmd),
        )?;

        executed_checks.push(ProofCheck {
            name: item.name.clone(),
            cmd: item.cmd.clone(),
            ok,
            area: Some(item.area.clone()),
            units: item.units.clone(),
            duration_ms,
            output_tail: output_tail.clone(),
        });

        if !ok {
            all_ok = false;
            eprintln!(
                "Check '{}' FAILED (exit {exit_code}):\n{output_tail}",
                item.name
            );
            if !args.keep_going {
                break;
            }
        }
    }

    // Record exact git tree hash of worktree files so any subsequent commit or edit makes proof stale
    let head_out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()?;
    let head = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
    let tree = tree_of_worktree(&root)?;

    let mut screenshots: Vec<String> = changed
        .iter()
        .filter(|f| f.ends_with(".png"))
        .cloned()
        .collect();
    for s in &args.screenshots {
        if !screenshots.contains(s) {
            screenshots.push(s.clone());
        }
    }

    let summary = ProofSummary {
        head,
        tree: tree.clone(),
        base: base_sha,
        ok: all_ok,
        timestamp: now_secs(),
        changed_files: changed,
        checks: executed_checks,
        screenshots,
    };

    let proof_dir = root.join(".tachoproof");
    fs::create_dir_all(&proof_dir)?;
    let summary_path = proof_dir.join("summary.json");
    let json_bytes = serde_json::to_vec_pretty(&summary).map_err(io::Error::other)?;
    write_atomic(&summary_path, &json_bytes)?;

    let because = format!("prove ok={all_ok} tree={}", &tree[..tree.len().min(12)]);
    store.record_decision("verify", "prove", &root.to_string_lossy(), &because)?;
    emit_decision_event("system", "verify:prove", &because);

    if !all_ok {
        return Err(io::Error::other(
            "one or more prove checks failed; see .tachoproof/summary.json",
        ));
    }

    println!(
        "Proof passed and written to {} (tree {})",
        summary_path.display(),
        &tree[..tree.len().min(12)]
    );
    Ok(())
}
