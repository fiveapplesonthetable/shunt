//! `tacho start <slug> [--base main|<pr>]`: worktree provisioning, pinned base SHA resolution,
//! collision-free 20-port block allocation, `.tachoenv` generation, idempotent re-run, and
//! bounded first-install / DB provisioner execution (exiting 3 when first install exceeds cap).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tacho_core::decide::verify::{parse_env, slug_problem};
use tacho_core::TaskStatus;
use tacho_daemon::{FleetState, WorktreeEntry};

use super::procs::signal_group_and_leader;
use super::use_private_tmp;
use super::{
    emit_decision_event, now_ns, now_secs, open_fleet_store, write_atomic, FdEventWatcher,
};
use crate::cli::StartArgs;

pub const PORT_BASE: u16 = 20000;
pub const PORT_BLOCK_SIZE: u16 = 20;
pub const PORT_CEILING: u16 = 32768;
pub const DEFAULT_INSTALL_TIMEOUT_SECS: u64 = 600;

/// True iff `s` is a 40-character hexadecimal git commit SHA.
pub fn is_full_hex_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Resolve a revision or ref in `repo_root` to a verified 40-character commit SHA.
pub fn git_rev_parse_commit(repo_root: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    let out = Command::new("git")
        .args(["rev-parse", "--verify", "-q", &spec])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_ascii_lowercase();
    is_full_hex_sha(&sha).then_some(sha)
}

/// Resolve `--base <main|pr|sha>` to a pinned 40-character commit SHA and optional base PR number.
///
/// Strictly refuses unresolvable PR numbers or unknown revisions without ever falling back to `HEAD`
/// or returning an unpinned branch name.
pub fn resolve_pinned_base_sha(
    repo_root: &Path,
    base_spec: &str,
    state: &FleetState,
) -> io::Result<(String, Option<u64>)> {
    let trimmed = base_spec.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "base specification cannot be empty",
        ));
    }

    let pr_candidate = trimmed
        .strip_prefix("pr:")
        .or_else(|| trimmed.strip_prefix('#'))
        .unwrap_or(trimmed);

    let is_explicit_pr = trimmed.starts_with("pr:") || trimmed.starts_with('#');
    let parsed_pr = if is_explicit_pr
        || (pr_candidate.len() <= 9 && pr_candidate.bytes().all(|b| b.is_ascii_digit()))
    {
        pr_candidate.parse::<u64>().ok()
    } else {
        None
    };

    if let Some(pr_num) = parsed_pr {
        if let Some(pr) = state.open_prs_cache.iter().find(|p| p.number == pr_num) {
            if let Some(sha) = git_rev_parse_commit(repo_root, &pr.head) {
                return Ok((sha, Some(pr_num)));
            }
            if is_full_hex_sha(&pr.head) {
                let fetch_out = Command::new("git")
                    .args(["fetch", "origin", &pr.head])
                    .current_dir(repo_root)
                    .output()?;
                if fetch_out.status.success() {
                    if let Some(sha) = git_rev_parse_commit(repo_root, &pr.head) {
                        return Ok((sha, Some(pr_num)));
                    }
                }
            }
        }

        let pull_ref = format!("refs/pull/{pr_num}/head");
        if let Some(sha) = git_rev_parse_commit(repo_root, &pull_ref) {
            return Ok((sha, Some(pr_num)));
        }

        if let Ok(fetch_out) = Command::new("git")
            .args(["fetch", "-q", "origin", &format!("{pull_ref}:{pull_ref}")])
            .current_dir(repo_root)
            .output()
        {
            if fetch_out.status.success() {
                if let Some(sha) = git_rev_parse_commit(repo_root, &pull_ref) {
                    return Ok((sha, Some(pr_num)));
                }
            }
        }

        if let Ok(gh_out) = Command::new("gh")
            .args([
                "pr",
                "view",
                &pr_num.to_string(),
                "--json",
                "headRefOid",
                "--jq",
                ".headRefOid",
            ])
            .current_dir(repo_root)
            .output()
        {
            if gh_out.status.success() {
                let oid = String::from_utf8_lossy(&gh_out.stdout).trim().to_string();
                if !oid.is_empty() {
                    let fetch_out = Command::new("git")
                        .args(["fetch", "-q", "origin", &oid])
                        .current_dir(repo_root)
                        .output()?;
                    if fetch_out.status.success() {
                        if let Some(sha) = git_rev_parse_commit(repo_root, &oid) {
                            return Ok((sha, Some(pr_num)));
                        }
                    }
                }
            }
        }

        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("could not resolve PR #{pr_num} to a local commit SHA"),
        ));
    }

    if trimmed == "main" || trimmed == "origin/main" {
        for candidate in [
            "origin/main",
            "refs/remotes/origin/main",
            "refs/heads/main",
            "main",
        ] {
            if let Some(sha) = git_rev_parse_commit(repo_root, candidate) {
                return Ok((sha, None));
            }
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("could not resolve '{trimmed}' to a pinned commit SHA"),
        ));
    }

    if let Some(sha) = git_rev_parse_commit(repo_root, trimmed) {
        return Ok((sha, None));
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("could not resolve base '{trimmed}' to a commit SHA"),
    ))
}

/// Read active TCP local ports from `/proc/net/tcp` and `/proc/net/tcp6`.
fn active_proc_tcp_ports() -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines().skip(1) {
            if let Some(port_hex) = line
                .split_whitespace()
                .nth(1)
                .and_then(|addr| addr.rsplit_once(':'))
                .map(|(_, p)| p)
            {
                if let Ok(p) = u16::from_str_radix(port_hex, 16) {
                    if p > 0 {
                        ports.insert(p);
                    }
                }
            }
        }
    }
    ports
}

/// Verify that every port in `base_port..base_port + block_size` is free in `/proc/net/tcp{,6}`
/// and bindable on `127.0.0.1`.
pub fn port_block_free(base_port: u16, block_size: u16) -> bool {
    let proc_ports = active_proc_tcp_ports();
    let mut listeners = Vec::with_capacity(block_size as usize);
    for offset in 0..block_size {
        let Some(p) = base_port.checked_add(offset) else {
            return false;
        };
        if proc_ports.contains(&p) {
            return false;
        }
        match TcpListener::bind(("127.0.0.1", p)) {
            Ok(l) => listeners.push(l),
            Err(_) => return false,
        }
    }
    drop(listeners);
    true
}

/// Allocate a 20-port block (`base..base+20`) that does not collide with any worktree in `state`
/// and whose ports are all free on the host.
pub fn allocate_port_block(slug: &str, state: &FleetState) -> io::Result<u16> {
    if let Some(existing) = state.worktrees.get(slug) {
        if existing.port >= PORT_BASE {
            return Ok(existing.port);
        }
    }

    let mut reserved_ports = BTreeSet::new();
    for (other_slug, wt) in &state.worktrees {
        if other_slug == slug {
            continue;
        }
        let last = wt
            .port_last
            .max(wt.port.saturating_add(PORT_BLOCK_SIZE - 1));
        for p in wt.port..=last {
            reserved_ports.insert(p);
        }
    }

    let mut candidate = PORT_BASE;
    while candidate.saturating_add(PORT_BLOCK_SIZE) <= PORT_CEILING {
        let overlaps_reserved =
            (candidate..candidate + PORT_BLOCK_SIZE).any(|p| reserved_ports.contains(&p));
        if !overlaps_reserved && port_block_free(candidate, PORT_BLOCK_SIZE) {
            return Ok(candidate);
        }
        candidate = candidate.saturating_add(PORT_BLOCK_SIZE);
    }

    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "exhausted all 20-port blocks between 20000 and 32768",
    ))
}

/// Merge toolchain directories ahead of `current_path` without duplicates.
pub fn merge_path(extra_dirs: &[PathBuf], current_path: &str) -> String {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for s in extra_dirs
        .iter()
        .map(|d| d.to_string_lossy().trim().to_string())
        .chain(current_path.split(':').map(|p| p.trim().to_string()))
    {
        if !s.is_empty() && seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out.join(":")
}

/// Discover standard toolchain directories for `.tachoenv` `PATH`.
pub fn default_toolchain_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(custom) = std::env::var("TACHO_TOOLCHAIN_PATH") {
        for p in custom.split(':').filter(|s| !s.trim().is_empty()) {
            dirs.push(PathBuf::from(p.trim()));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let h = PathBuf::from(home);
        for rel in [
            ".cargo/bin",
            ".local/share/fnm",
            ".nvm/versions/node",
            ".local/bin",
        ] {
            let candidate = h.join(rel);
            if candidate.is_dir() {
                dirs.push(candidate);
            }
        }
    }
    for sys_dir in ["/usr/local/cargo/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        let p = PathBuf::from(sys_dir);
        if p.is_dir() {
            dirs.push(p);
        }
    }
    dirs
}

/// Resolve git author/committer name and email from environment or `git config`.
pub fn resolve_git_identity(repo_root: &Path) -> (Option<String>, Option<String>) {
    let read_cfg = |key: &str| -> Option<String> {
        let out = Command::new("git")
            .args(["config", "--get", key])
            .current_dir(repo_root)
            .output()
            .ok()?;
        if out.status.success() {
            let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!val.is_empty()).then_some(val)
        } else {
            None
        }
    };

    let name = std::env::var("TACHO_GIT_NAME")
        .ok()
        .or_else(|| std::env::var("GIT_AUTHOR_NAME").ok())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| read_cfg("user.name"));

    let email = std::env::var("TACHO_GIT_EMAIL")
        .ok()
        .or_else(|| std::env::var("GIT_AUTHOR_EMAIL").ok())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| read_cfg("user.email"));

    (name, email)
}

/// Render the `.tachoenv` file contents for a provisioned worktree.
#[allow(clippy::too_many_arguments)]
pub fn format_tachoenv_content(
    wt_env: &WorktreeEntry,
    cargo_target_dir: &Path,
    merged_path: &str,
    git_name: Option<&str>,
    git_email: Option<&str>,
    extra_kv: &BTreeMap<String, String>,
) -> String {
    let p = wt_env.port;
    let p_last = wt_env.port_last;
    let (console_port, shot_port) = (p + 8, p + 9);

    let mut lines = vec![
        format!("SLUG={}", wt_env.slug),
        format!("TACHO_SLUG={}", wt_env.slug),
        format!("WORKTREE={}", wt_env.path),
        format!("TACHO_WORKTREE={}", wt_env.path),
        format!("BRANCH={}", wt_env.branch),
        format!("TACHO_BRANCH={}", wt_env.branch),
        format!("BASE_SHA={}", wt_env.base_sha),
        format!("TACHO_BASE_SHA={}", wt_env.base_sha),
    ];
    if let Some(bpr) = wt_env.base_pr {
        lines.push(format!("BASE_PR={bpr}"));
        lines.push(format!("TACHO_BASE_PR={bpr}"));
    }
    lines.extend([
        format!("PORT={p}"),
        format!("TACHO_PORT={p}"),
        format!("PORT_LAST={p_last}"),
        format!("TACHO_PORT_LAST={p_last}"),
        format!("API_PORT={p}"),
        format!("WEB_PORT={}", p + 1),
        format!("DB_PORT={}", p + 2),
        format!("REDIS_PORT={}", p + 3),
        format!("METRICS_PORT={}", p + 4),
        format!("DEBUG_PORT={}", p + 5),
        format!("CONSOLE_PORT={console_port}"),
        format!("SHOT_PORT={shot_port}"),
        format!("CONSOLE_URL=http://localhost:{console_port}"),
        format!("SHOT_URL=http://localhost:{shot_port}"),
        format!("CARGO_TARGET_DIR={}", cargo_target_dir.display()),
        "CARGO_PROFILE_DEV_DEBUG=0".to_string(),
        "CARGO_PROFILE_TEST_DEBUG=0".to_string(),
        "CARGO_INCREMENTAL=0".to_string(),
        format!("PATH={merged_path}"),
    ]);
    if let Some(name) = git_name {
        lines.push(format!("GIT_AUTHOR_NAME={name}"));
        lines.push(format!("GIT_COMMITTER_NAME={name}"));
    }
    if let Some(email) = git_email {
        lines.push(format!("GIT_AUTHOR_EMAIL={email}"));
        lines.push(format!("GIT_COMMITTER_EMAIL={email}"));
    }
    for (k, v) in extra_kv {
        if !k.is_empty() {
            lines.push(format!("{k}={v}"));
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Run an optional provisioner/install command (`TACHO_DB_UP_CMD`, `TACHO_INSTALL_CMD`, etc.)
/// in a detached process group waited via [`FdEventWatcher`].
///
/// Returns `Ok(Ok(stdout))` on success, `Ok(Err(3))` if the command exceeds `timeout_secs`
/// (or exits with code 3 or 124), or `Err` if it fails for another reason.
pub fn run_provisioner_bounded(
    wt_path: &Path,
    cmd_str: &str,
    env_vars: &BTreeMap<String, String>,
    timeout_secs: u64,
) -> io::Result<Result<String, i32>> {
    let out_tmp = tempfile::NamedTempFile::new()?;
    let out_file = out_tmp.reopen()?;
    let err_file = out_tmp.reopen()?;

    let mut cmd = Command::new("bash");
    use_private_tmp(&mut cmd, "provision")?;
    cmd.args(["-c", cmd_str])
        .current_dir(wt_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .process_group(0);
    for (k, v) in env_vars {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn()?;
    let pid = child.id();
    let mut watcher = FdEventWatcher::new()?;
    let _ = watcher.register_pid(pid);

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    loop {
        if let Some(status) = child.try_wait()? {
            let mut output = String::new();
            let _ = File::open(out_tmp.path()).and_then(|mut f| f.read_to_string(&mut output));
            let code = status.code().unwrap_or(1);
            if status.success() {
                return Ok(Ok(output));
            }
            if code == 3 || code == 124 {
                return Ok(Err(3));
            }
            return Err(io::Error::other(format!(
                "provisioner command failed (exit {code}): {}",
                output.trim()
            )));
        }

        let now = Instant::now();
        if now >= deadline {
            signal_group_and_leader(pid, libc::SIGKILL)?;
            child.wait()?;
            return Ok(Err(3));
        }

        let rem_ms = deadline.saturating_duration_since(now).as_millis().min(500) as i32;
        watcher.wait_ms(rem_ms.max(1));
    }
}

/// Whether `dir` lies inside the fleet root (`store_home`, `TACHO_HOME`, `TACHO_WORK`,
/// `TACHO_REPO`, or `state.repo_root`).
///
/// Prevents worktree provisioning fallbacks from running against an arbitrary ambient checkout
/// outside the fleet root.
pub fn is_within_fleet_root(dir: &Path, store_home: &Path, state: &FleetState) -> bool {
    let Ok(canon) = dir.canonicalize() else {
        return false;
    };
    let mut roots: Vec<PathBuf> = Vec::new();
    if !store_home.as_os_str().is_empty() {
        roots.push(store_home.to_path_buf());
    }
    for var in ["TACHO_HOME", "TACHO_WORK", "TACHO_REPO"] {
        if let Some(val) = std::env::var_os(var).filter(|s| !s.is_empty()) {
            roots.push(PathBuf::from(val));
        }
    }
    if let Some(ref repo) = state.repo_root {
        if !repo.trim().is_empty() {
            roots.push(PathBuf::from(repo));
        }
    }
    roots.into_iter().any(|r| {
        r.canonicalize()
            .map(|rc| canon.starts_with(&rc))
            .unwrap_or(false)
    })
}

/// Core implementation of `tacho start` against an already-opened [`tacho_daemon::FleetStore`],
/// returning `(worktree_path, exit_code)` (`0` on success, `3` when first install / provisioner
/// exceeds its timeout cap).
pub fn provision_worktree_in_store(
    store: &mut tacho_daemon::FleetStore,
    args: StartArgs,
) -> io::Result<(PathBuf, i32)> {
    if let Some(prob) = slug_problem(&args.slug) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }

    let state = store.load()?;
    let cwd = std::env::current_dir()?;
    let has_explicit_repo_bound = state
        .repo_root
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty())
        || std::env::var_os("TACHO_REPO")
            .filter(|s| !s.is_empty())
            .is_some();
    if has_explicit_repo_bound && !is_within_fleet_root(&cwd, store.home(), &state) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to provision worktree from '{}': current directory is outside the fleet root '{}'",
                cwd.display(),
                store.home().display()
            ),
        ));
    }

    let wt_root = args.dir.unwrap_or_else(|| {
        if let Some(work) = std::env::var_os("TACHO_WORK").filter(|s| !s.is_empty()) {
            PathBuf::from(work).join("worktrees")
        } else if !store.home().as_os_str().is_empty() {
            store.home().join("worktrees")
        } else {
            cwd.join(".worktrees")
        }
    });
    fs::create_dir_all(&wt_root)?;
    let wt_path = wt_root.join(&args.slug);

    let known_in_state = state.worktrees.contains_key(&args.slug);
    if wt_path.exists() && !known_in_state && !wt_path.join(".tachoenv").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to clobber existing untracked path '{}'",
                wt_path.display()
            ),
        ));
    }

    let (base_sha, base_pr) = match &args.base {
        Some(spec) => resolve_pinned_base_sha(&cwd, spec, &state)?,
        None => match state.worktrees.get(&args.slug) {
            Some(existing) if is_full_hex_sha(&existing.base_sha) => {
                (existing.base_sha.clone(), existing.base_pr)
            }
            _ => resolve_pinned_base_sha(&cwd, "main", &state)?,
        },
    };

    let base_port = allocate_port_block(&args.slug, &state)?;
    let port_last = base_port + PORT_BLOCK_SIZE - 1;

    if !wt_path.exists() {
        let out = Command::new("git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "worktree",
                "add",
                "-B",
                &args.slug,
                wt_path.to_str().unwrap_or("."),
                &base_sha,
            ])
            .current_dir(&cwd)
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        for key in [
            format!("branch.{}.remote", args.slug),
            format!("branch.{}.merge", args.slug),
        ] {
            let cfg_out = Command::new("git")
                .args(["config", "--unset", &key])
                .current_dir(&wt_path)
                .output()?;
            if !cfg_out.status.success() && cfg_out.status.code() != Some(5) {
                return Err(io::Error::other(format!(
                    "git config --unset {key} failed: {}",
                    String::from_utf8_lossy(&cfg_out.stderr).trim()
                )));
            }
        }
    }

    let created_at = state
        .worktrees
        .get(&args.slug)
        .map(|w| w.created_at)
        .unwrap_or_else(now_secs);

    let wt_env = WorktreeEntry {
        slug: args.slug.clone(),
        path: wt_path.to_string_lossy().to_string(),
        branch: args.slug.clone(),
        base_sha: base_sha.clone(),
        base_pr,
        port: base_port,
        port_last,
        created_at,
    };

    let because = format!(
        "provisioned worktree {} on base {} (ports {}..={})",
        args.slug,
        &base_sha[..base_sha.len().min(12)],
        base_port,
        port_last
    );
    let wt_env_for_store = wt_env.clone();
    let slug_for_store = args.slug.clone();
    store.mutate(
        Some(("verify", "start_worktree", &args.slug, &because)),
        move |st| {
            st.worktrees
                .insert(slug_for_store.clone(), wt_env_for_store.clone());
        },
    )?;

    let cargo_target_dir = std::env::var("TACHO_CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| wt_root.join(".cargo-target"));
    fs::create_dir_all(&cargo_target_dir)?;

    let current_path = std::env::var("PATH").unwrap_or_default();
    let merged_path = merge_path(&default_toolchain_dirs(), &current_path);
    let (git_name, git_email) = resolve_git_identity(&cwd);

    let mut extra_kv = BTreeMap::new();
    let initial_env_content = format_tachoenv_content(
        &wt_env,
        &cargo_target_dir,
        &merged_path,
        git_name.as_deref(),
        git_email.as_deref(),
        &extra_kv,
    );
    let env_map = parse_env(&initial_env_content);

    if let Some(project) = &state.project {
        let mut request = tacho_core::project::HookRequest {
            version: 1,
            kind: tacho_core::project::HookKind::RunnerTrust,
            repository_root: state
                .repo_root
                .clone()
                .unwrap_or_else(|| cwd.to_string_lossy().into_owned()),
            worktree: wt_path.to_string_lossy().into_owned(),
            base: base_sha.clone(),
            head: base_sha.clone(),
            tree: crate::fleet::store::git_stdout(&wt_path, &["rev-parse", "HEAD^{tree}"])?,
            files: Vec::new(),
            areas: Vec::new(),
            payload: serde_json::json!({"operation":"worker", "slug":args.slug, "port":base_port, "port_last":port_last, "target_dir":cargo_target_dir}),
        };
        if let Some(command) = project
            .hooks
            .get(&tacho_core::project::HookKind::RunnerTrust)
        {
            tacho_daemon::project::invoke(command, Path::new(&request.repository_root), &request)?;
        }
        if let Some(command) = project.hooks.get(&tacho_core::project::HookKind::Prepare) {
            request.kind = tacho_core::project::HookKind::Prepare;
            let output = tacho_daemon::project::invoke(command, &wt_path, &request)?;
            extra_kv.extend(tacho_daemon::project::prepared_environment(&output)?);
        }
    }

    let install_timeout = std::env::var("TACHO_START_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INSTALL_TIMEOUT_SECS);

    let db_up_cmd = std::env::var("TACHO_DB_UP_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| state.db_up_cmd.clone().filter(|s| !s.trim().is_empty()));
    let install_cmd = std::env::var("TACHO_INSTALL_CMD")
        .ok()
        .or_else(|| std::env::var("TACHO_PROVISION_CMD").ok())
        .filter(|s| !s.trim().is_empty());

    for cmd_str in [db_up_cmd, install_cmd].into_iter().flatten() {
        match run_provisioner_bounded(&wt_path, &cmd_str, &env_map, install_timeout)? {
            Ok(stdout) => {
                for (k, v) in parse_env(&stdout) {
                    extra_kv.insert(k, v);
                }
            }
            Err(code) => {
                eprintln!(
                    "First install / provisioner exceeded the {install_timeout}s timeout cap; re-run `tacho start {}` to finish.",
                    args.slug
                );
                return Ok((wt_path, code));
            }
        }
    }

    let final_env_content = format_tachoenv_content(
        &wt_env,
        &cargo_target_dir,
        &merged_path,
        git_name.as_deref(),
        git_email.as_deref(),
        &extra_kv,
    );
    write_atomic(&wt_path.join(".tachoenv"), final_env_content.as_bytes())?;

    store.emit_state_band(
        &format!("task:{}/state", args.slug),
        "InProgress",
        TaskStatus::InProgress,
        &because,
        now_ns(),
    )?;
    emit_decision_event(&format!("agent:{}", args.slug), "verify:start", &because);

    println!("Worktree: {}", wt_path.display());
    println!("Branch:   {}", args.slug);
    println!("Base SHA: {}", base_sha);
    println!("Ports:    {}..={}", base_port, port_last);
    Ok((wt_path, 0))
}

/// Core implementation of `tacho start`, returning an exit code (`0` on success, `3` when
/// first install / provisioner exceeds its timeout cap).
pub fn start_worktree_with_code(args: StartArgs) -> io::Result<i32> {
    let mut store = open_fleet_store()?;
    let (_wt_path, code) = provision_worktree_in_store(&mut store, args)?;
    Ok(code)
}

/// Executes `tacho start <slug> [--base main|<pr>]`.
pub fn execute_start(args: StartArgs) -> io::Result<()> {
    let code = start_worktree_with_code(args)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
