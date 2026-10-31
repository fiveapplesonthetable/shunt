//! Worker proof plans and PR preflight.

use super::*;

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

    let selected = areas_for_files(changed_files, rules);
    for r in rules {
        if selected.contains(&r.area) {
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
            Some(crate::char_prefix(first_tok, 8))
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
            crate::char_prefix(head_tree, 12)
        ));
    } else if head_tree.is_empty() && !head_sha.is_empty() && p.head != head_sha {
        problems.push(format!(
            "proof head {} != current HEAD {} (re-run `tacho prove`)",
            &p.head[..p.head.len().min(12)],
            crate::char_prefix(head_sha, 12)
        ));
    }
    let selected = areas_for_files(changed_files, rules);
    let needs_shot = rules
        .iter()
        .any(|rule| rule.needs_screenshot && selected.contains(&rule.area));
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

/// Render `.tachoenv` with additional project/plugin environment variables (such as `DATABASE_URL`).
pub fn env_file_with_vars(
    slug: &str,
    base_sha: &str,
    base_pr: Option<u64>,
    port: u16,
    port_last: u16,
    target_dir: Option<&str>,
    extra_vars: &[(&str, &str)],
) -> String {
    let mut base = env_file(slug, base_sha, base_pr, port, port_last, target_dir);
    for (k, v) in extra_vars {
        base.push_str(&format!("export {}={}\n", k, sh_quote(v)));
    }
    base
}

/// R48: extract the first high-signal compiler or test error line from a check's output,
/// falling back to the last non-empty line.
pub fn first_error(output: &str) -> String {
    let line = super::logs::first_error_line(output);
    if !line.is_empty() {
        return line;
    }
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// R48: keep the last `max_lines` of a check log so proof artifacts stay compact.
pub fn log_tail(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// R48: compute the environment overrides for a local `prove` check (`CARGO_INCREMENTAL=0`,
/// optional `RUSTC_WRAPPER=sccache`, optional `CARGO_TARGET_DIR`, and `PORT`).
pub fn check_environment(
    port: Option<u16>,
    target_dir: Option<&str>,
    sccache_bin: Option<&str>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("CARGO_INCREMENTAL".to_string(), "0".to_string());
    if let Some(p) = port {
        env.insert("PORT".to_string(), p.to_string());
    }
    if let Some(td) = target_dir {
        if !td.is_empty() {
            env.insert("CARGO_TARGET_DIR".to_string(), td.to_string());
        }
    }
    if let Some(sc) = sccache_bin {
        if !sc.is_empty() {
            env.insert("RUSTC_WRAPPER".to_string(), sc.to_string());
        }
    }
    env
}

/// R48: conservative fallback proof plan when `changed_files` is empty (e.g. no diff against base):
/// runs full workspace fmt, clippy, and tests so an empty file list never vacantly passes.
pub fn conservative_proof_plan(
    changed_files: &[String],
    rules: &[AreaRule],
) -> Vec<(String, String)> {
    if changed_files.is_empty() {
        return vec![
            ("whitespace".to_string(), "git diff --check".to_string()),
            (
                "rust-fmt".to_string(),
                "cargo fmt --all -- --check".to_string(),
            ),
            (
                "clippy-workspace".to_string(),
                "cargo clippy --workspace --all-targets -- -D warnings".to_string(),
            ),
            (
                "test-workspace".to_string(),
                "cargo test --workspace".to_string(),
            ),
        ];
    }
    proof_plan(changed_files, rules)
}
