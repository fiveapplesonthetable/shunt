//! Worker agent kit commands: `start`, `prove`, `job`, `wait`, `kill`, `pr`, `note`, and `starve`.
//!
//! All decisions delegate to `tacho_core::decide::{verify, watch}` and persist state
//! and decision reasons into `$TACHO_HOME/events.sqlite` via [`FleetStore`].

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tacho_core::decide::verify::{
    allocate_ports, build_body, env_file_with_vars, job_name_problem, owns, parse_env, pr_refusals,
    proof_plan, slug_problem, ProofCheck, ProofSummary,
};
use tacho_core::decide::watch::{parse_note, plan_starve, with_note};
use tacho_core::Scrubber;
use tacho_daemon::{FleetStore, WorktreeEntry};

use crate::cli::{JobArgs, KillArgs, NoteArgs, PrArgs, ProveArgs, StartArgs, StarveArgs, WaitArgs};
use crate::client::DaemonClient;

/// Maximum wait duration per `tacho wait` call (9 minutes = 540 seconds) so agent tool calls
/// with a 10-minute timeout never get killed mid-wait.
pub const MAX_WAIT_SECS: u64 = 540;

fn open_fleet_store() -> io::Result<FleetStore> {
    let home = crate::paths::resolve_home();
    let _ = crate::paths::ensure_dir_0700(&home);
    let db = home.join("events.sqlite");
    FleetStore::open(&db).map_err(|e| io::Error::other(e.to_string()))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn emit_decision_event(track: &str, name: &str, because: &str) {
    let client = DaemonClient::new();
    let _ = client.send_request(&serde_json::json!({
        "input": {
            "type": "custom",
            "source_ts_ns": now_ns(),
            "track": track,
            "name": name,
            "op": "instant",
            "args": {
                "because": because,
            }
        }
    }));
}

fn git_output(cwd: &Path, args: &[&str]) -> io::Result<String> {
    let out = Command::new("git").args(args).current_dir(cwd).output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn repo_root(cwd: &Path) -> io::Result<PathBuf> {
    let top = git_output(cwd, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(top))
}

fn port_is_bindable(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn add_git_excludes(wt_path: &Path) {
    if let Ok(exclude_raw) = git_output(wt_path, &["rev-parse", "--git-path", "info/exclude"]) {
        let p = PathBuf::from(&exclude_raw);
        let exclude = if p.is_absolute() { p } else { wt_path.join(p) };
        if let Some(parent) = exclude.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let existing = fs::read_to_string(&exclude).unwrap_or_default();
        let mut to_add = Vec::new();
        for pat in [".tachoenv", ".tachoproof", ".tachorun"] {
            if !existing
                .lines()
                .any(|l| l.trim().trim_end_matches('/') == pat)
            {
                to_add.push(pat);
            }
        }
        if !to_add.is_empty() {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&exclude) {
                if !existing.is_empty() && !existing.ends_with('\n') {
                    let _ = writeln!(f);
                }
                for pat in to_add {
                    let _ = writeln!(f, "{pat}");
                }
            }
        }
    }
}

/// Computes the exact git tree hash of the working directory (including unstaged and untracked
/// files, excluding ignored files) using a temporary `GIT_INDEX_FILE` so the real index is untouched.
pub fn tree_of_worktree(wt: &Path) -> io::Result<String> {
    add_git_excludes(wt);
    let tmp_dir = tempfile::TempDir::new()?;
    let tmp_path = tmp_dir.path().join("index");
    // Copy current index if present and non-empty so git add -A is fast
    if let Ok(idx_raw) = git_output(wt, &["rev-parse", "--git-path", "index"]) {
        let p = PathBuf::from(&idx_raw);
        let idx_path = if p.is_absolute() { p } else { wt.join(p) };
        if idx_path.is_file() && idx_path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            let _ = fs::copy(&idx_path, &tmp_path);
        }
    }
    let add_status = Command::new("git")
        .args(["add", "-A"])
        .current_dir(wt)
        .env("GIT_INDEX_FILE", &tmp_path)
        .status()?;
    if !add_status.success() {
        return Err(io::Error::other("git add -A on temp index failed"));
    }
    let out = Command::new("git")
        .args(["write-tree"])
        .current_dir(wt)
        .env("GIT_INDEX_FILE", &tmp_path)
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other("git write-tree on temp index failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Lists all files changed relative to `base_sha` (committed, staged, unstaged, and untracked).
pub fn changed_files_since(wt: &Path, base_sha: &str) -> io::Result<Vec<String>> {
    let mut files = BTreeSet::new();
    if let Ok(diff_out) = git_output(wt, &["diff", "--no-renames", "--name-only", base_sha]) {
        for line in diff_out.lines().map(str::trim).filter(|s| !s.is_empty()) {
            files.insert(line.to_string());
        }
    }
    if let Ok(untracked) = git_output(wt, &["ls-files", "--others", "--exclude-standard"]) {
        for line in untracked.lines().map(str::trim).filter(|s| !s.is_empty()) {
            files.insert(line.to_string());
        }
    }
    Ok(files.into_iter().collect())
}

/// Executes `tacho start <slug> [--base main|<pr>|<sha>] [--dir <worktree-root>]`.
pub fn execute_start(args: StartArgs) -> io::Result<()> {
    if let Some(prob) = slug_problem(&args.slug) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }

    let cwd = std::env::current_dir()?;
    let root = repo_root(&cwd)?;

    // Strictly resolve base to an exact commit SHA without silent fallback to stale FETCH_HEAD or HEAD.
    let base_arg = args.base.as_deref().unwrap_or("HEAD");
    let mut base_pr: Option<u64> = None;
    let base_sha = if let Ok(pr_num) = base_arg.trim_start_matches("pr:").parse::<u64>() {
        base_pr = Some(pr_num);
        let pr_ref = format!("refs/tacho/pr/{pr_num}");
        let fetch_ok = Command::new("git")
            .args([
                "fetch",
                "--no-tags",
                "origin",
                &format!("+pull/{pr_num}/head:{pr_ref}"),
            ])
            .current_dir(&root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if fetch_ok {
            git_output(&root, &["rev-parse", &format!("{pr_ref}^{{commit}}")])?
        } else {
            // Try gh pr view for local/mocked gh setups, otherwise refuse explicitly
            let gh_sha = Command::new("gh")
                .args([
                    "pr",
                    "view",
                    &pr_num.to_string(),
                    "--json",
                    "headRefOid",
                    "-q",
                    ".headRefOid",
                ])
                .current_dir(&root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());
            match gh_sha {
                Some(sha) => git_output(&root, &["rev-parse", &format!("{sha}^{{commit}}")]).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("base PR #{pr_num} head SHA '{sha}' is not present in local git object store"),
                    )
                })?,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("failed to resolve base PR #{pr_num}: could not fetch origin pull/{pr_num}/head"),
                    ));
                }
            }
        }
    } else if base_arg == "origin/main" {
        let _ = Command::new("git")
            .args(["fetch", "origin", "main"])
            .current_dir(&root)
            .output();
        git_output(&root, &["rev-parse", "origin/main^{commit}"]).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "failed to resolve base 'origin/main'",
            )
        })?
    } else if base_arg == "main" {
        let _ = Command::new("git")
            .args(["fetch", "origin", "main"])
            .current_dir(&root)
            .output();
        git_output(&root, &["rev-parse", "origin/main^{commit}"])
            .or_else(|_| git_output(&root, &["rev-parse", "main^{commit}"]))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "failed to resolve base 'main' (neither origin/main nor main exists)",
                )
            })?
    } else {
        git_output(&root, &["rev-parse", &format!("{base_arg}^{{commit}}")]).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("failed to resolve base revision '{base_arg}'"),
            )
        })?
    };

    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let existing_entry = state
        .worktrees
        .get(&args.slug)
        .filter(|wt| Path::new(&wt.path).is_dir())
        .cloned();

    let (port, port_last, base_sha, base_pr) = if let Some(ref existing) = existing_entry {
        let eff_base = if args.base.is_some() {
            base_sha
        } else {
            existing.base_sha.clone()
        };
        let eff_pr = if args.base.is_some() {
            base_pr
        } else {
            existing.base_pr
        };
        (existing.port, existing.port_last, eff_base, eff_pr)
    } else {
        let mut used_ports = BTreeSet::new();
        for wt in state.worktrees.values() {
            if Path::new(&wt.path).is_dir() {
                for p in wt.port..=wt.port_last {
                    used_ports.insert(p);
                }
            }
        }
        let (p, pl) = allocate_ports(&used_ports, port_is_bindable).map_err(io::Error::other)?;
        (p, pl, base_sha, base_pr)
    };

    let wt_parent = match args.dir {
        Some(d) => d,
        None => root.parent().unwrap_or(&root).join(format!(
            "{}-worktrees",
            root.file_name().and_then(|s| s.to_str()).unwrap_or("repo")
        )),
    };
    fs::create_dir_all(&wt_parent)?;
    let wt_path = wt_parent.join(&args.slug);

    if !wt_path.exists() {
        // Create or reset branch `<slug>` at exact `base_sha`
        let branch_exists = Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", args.slug),
            ])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let status = if branch_exists {
            Command::new("git")
                .args([
                    "worktree",
                    "add",
                    wt_path.to_str().unwrap_or_default(),
                    &args.slug,
                ])
                .current_dir(&root)
                .status()?
        } else {
            Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    &args.slug,
                    wt_path.to_str().unwrap_or_default(),
                    &base_sha,
                ])
                .current_dir(&root)
                .status()?
        };
        if !status.success() {
            return Err(io::Error::other(format!(
                "git worktree add failed for {}",
                wt_path.display()
            )));
        }
    }

    let target_dir = root.join("target");
    let target_str = target_dir.to_str();

    // Optional per-agent database project plugin (`db_up_cmd` in FleetState or `TACHO_DB_UP_CMD` env var).
    let mut extra_vars: Vec<(String, String)> = Vec::new();
    let db_cmd = std::env::var("TACHO_DB_UP_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| state.db_up_cmd.clone().filter(|s| !s.trim().is_empty()));
    if let Some(cmd_str) = db_cmd {
        let db_port = port.saturating_add(1);
        let out = Command::new("bash")
            .args(["-c", &cmd_str])
            .current_dir(&wt_path)
            .env("SLUG", &args.slug)
            .env("PORT", port.to_string())
            .env("PORT_LAST", port_last.to_string())
            .env("DB_PORT", db_port.to_string())
            .env("WORKTREE", wt_path.to_str().unwrap_or_default())
            .output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(io::Error::other(format!(
                "per-agent database setup plugin failed: {}",
                stderr.trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut db_url = None;
        for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(rest) = line
                .strip_prefix("export DATABASE_URL=")
                .or_else(|| line.strip_prefix("DATABASE_URL="))
            {
                db_url = Some(rest.trim_matches('\'').trim_matches('"').to_string());
            } else if line.contains("://") {
                db_url = Some(line.to_string());
            }
        }
        let resolved_url = db_url.unwrap_or_else(|| {
            format!(
                "postgres://postgres:postgres@127.0.0.1:{db_port}/{}",
                args.slug.replace('-', "_")
            )
        });
        extra_vars.push(("DATABASE_URL".to_string(), resolved_url));
    }

    let extra_refs: Vec<(&str, &str)> = extra_vars
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let env_contents = env_file_with_vars(
        &args.slug,
        &base_sha,
        base_pr,
        port,
        port_last,
        target_str,
        &extra_refs,
    );
    fs::write(wt_path.join(".tachoenv"), &env_contents)?;
    add_git_excludes(&wt_path);

    let now = now_secs();
    let wt_str = wt_path.to_string_lossy().to_string();
    let because = format!(
        "created worktree {} at base {} (ports {}..={})",
        args.slug,
        &base_sha[..base_sha.len().min(12)],
        port,
        port_last
    );
    store.mutate(Some(("kit", "start", &args.slug, &because)), |st| {
        st.worktrees.insert(
            args.slug.clone(),
            WorktreeEntry {
                slug: args.slug.clone(),
                path: wt_str.clone(),
                branch: args.slug.clone(),
                base_sha: base_sha.clone(),
                base_pr,
                port,
                port_last,
                created_at: now,
            },
        );
    })?;
    emit_decision_event(&format!("task:{}", args.slug), "kit:start", &because);

    println!("Worktree: {}", wt_path.display());
    println!("Branch:   {}", args.slug);
    println!("Base SHA: {base_sha}");
    println!("Ports:    {port}..={port_last}");
    Ok(())
}

/// Executes `tacho prove` in the current worktree.
pub fn execute_prove(args: ProveArgs) -> io::Result<()> {
    let cwd = args.dir.unwrap_or(std::env::current_dir()?);
    let root = repo_root(&cwd)?;
    let env_map = fs::read_to_string(root.join(".tachoenv"))
        .map(|s| parse_env(&s))
        .unwrap_or_default();

    let base_sha = args
        .base
        .or_else(|| env_map.get("BASE_SHA").cloned().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| {
            git_output(&root, &["merge-base", "HEAD", "origin/main"])
                .or_else(|_| git_output(&root, &["rev-parse", "HEAD~1"]))
                .or_else(|_| git_output(&root, &["rev-parse", "HEAD"]))
                .unwrap_or_default()
        });

    let head_sha = git_output(&root, &["rev-parse", "HEAD"])?;
    let tree_sha = tree_of_worktree(&root)?;
    let changed = changed_files_since(&root, &base_sha)?;

    let store = open_fleet_store()?;
    let state = store.load()?;
    let plan = proof_plan(&changed, &state.area_rules);

    if args.dry_run {
        println!("Base: {base_sha}");
        println!("Head: {head_sha}");
        println!("Tree: {tree_sha}");
        println!("Changed files ({}):", changed.len());
        for f in &changed {
            println!("  {f}");
        }
        println!("Planned checks ({}):", plan.len());
        for (name, cmd) in &plan {
            println!("  [{name}] {cmd}");
        }
        return Ok(());
    }

    let proof_dir = root.join(".tachoproof");
    fs::create_dir_all(&proof_dir)?;
    add_git_excludes(&root);

    let mut checks = Vec::new();
    let mut all_ok = true;

    for (name, cmd) in plan {
        println!("==> [{name}] {cmd}");
        let start = Instant::now();
        let out = Command::new("bash")
            .args(["-c", &cmd])
            .current_dir(&root)
            .output()?;
        let duration_ms = start.elapsed().as_millis() as u64;
        let ok = out.status.success();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let tail: Vec<&str> = combined.lines().rev().take(25).collect();
        let raw_tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        let output_tail = Scrubber::new().scrub(&raw_tail);
        if !ok {
            all_ok = false;
            eprintln!("FAIL [{name}] ({duration_ms}ms):\n{output_tail}");
        } else {
            println!("PASS [{name}] ({duration_ms}ms)");
        }
        checks.push(ProofCheck {
            name,
            cmd,
            ok,
            duration_ms,
            output_tail,
            area: None,
            units: Vec::new(),
        });
        if !ok && !args.keep_going {
            break;
        }
    }

    let tree_after = tree_of_worktree(&root)?;
    let summary = ProofSummary {
        head: head_sha.clone(),
        tree: tree_after.clone(),
        base: base_sha,
        ok: all_ok,
        timestamp: now_secs(),
        changed_files: changed,
        checks,
        screenshots: args.screenshots,
    };

    let summary_json = serde_json::to_string_pretty(&summary).unwrap_or_default();
    fs::write(proof_dir.join("summary.json"), &summary_json)?;

    let slug = env_map.get("SLUG").cloned().unwrap_or_else(|| {
        root.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("worktree")
            .to_string()
    });
    let because = format!(
        "prove {} on tree {} (ok={})",
        slug,
        &tree_after[..tree_after.len().min(12)],
        all_ok
    );
    let _ = store.record_decision("kit", "prove", &slug, &because);
    emit_decision_event(&format!("task:{slug}"), "kit:prove", &because);

    if !all_ok {
        return Err(io::Error::other("one or more proof checks failed"));
    }
    println!(
        "Proof recorded: .tachoproof/summary.json (tree {})",
        &tree_after[..tree_after.len().min(12)]
    );
    Ok(())
}

fn job_dir(wt: &Path) -> PathBuf {
    wt.join(".tachorun")
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc != 0 {
        return false;
    }
    // Treat zombie ('Z') processes as not alive
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        if let Some(rparen) = stat.rfind(')') {
            let after = stat[rparen + 1..].trim_start();
            if after.starts_with('Z') {
                return false;
            }
        }
    }
    true
}

fn group_alive(pgid: u32) -> bool {
    if pgid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(-(pgid as i32), 0) };
    if rc != 0 {
        return false;
    }
    // Check if any non-zombie process exists in this process group
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Ok(pid) = name.parse::<u32>() {
                    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                        if let Some(rparen) = stat.rfind(')') {
                            let fields: Vec<&str> = stat[rparen + 1..].split_whitespace().collect();
                            if fields.len() >= 3 {
                                let state = fields[0];
                                let p_pgid: u32 = fields[2].parse().unwrap_or(0);
                                if p_pgid == pgid && !state.starts_with('Z') {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        return false;
    }
    true
}

/// Executes `tacho job <name> -- <cmd...>` (spawns a detached process-group job under `.tachorun/<name>`).
pub fn execute_job(args: JobArgs) -> io::Result<()> {
    if args.cmd.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: tacho job <name> -- <command> [args...]",
        ));
    }
    if let Some(prob) = job_name_problem(&args.name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }
    let cwd = std::env::current_dir()?;
    let root = repo_root(&cwd).unwrap_or(cwd);
    let dir = job_dir(&root);
    fs::create_dir_all(&dir)?;
    add_git_excludes(&root);

    let pid_file = dir.join(format!("{}.pid", args.name));
    if pid_file.exists() {
        if let Ok(s) = fs::read_to_string(&pid_file) {
            if let Ok(old_pid) = s.trim().parse::<u32>() {
                if pid_alive(old_pid) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("job '{}' is already running (pid {old_pid})", args.name),
                    ));
                }
            }
        }
    }

    let log_file = dir.join(format!("{}.log", args.name));
    let exit_file = dir.join(format!("{}.exit", args.name));
    let cwd_file = dir.join(format!("{}.cwd", args.name));
    let _ = fs::remove_file(&exit_file);
    fs::write(&cwd_file, root.to_string_lossy().as_bytes())?;

    let quoted_cmd = args
        .cmd
        .iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-./=:".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    let exit_tmp_file = dir.join(format!("{}.exit.tmp", args.name));
    let wrapper = format!(
        "({quoted_cmd}); code=$?; printf '%d\\n' \"$code\" > '{}' && mv -f '{}' '{}'; exit \"$code\"",
        exit_tmp_file.display(),
        exit_tmp_file.display(),
        exit_file.display()
    );

    let log_out = File::create(&log_file)?;
    let log_err = log_out.try_clone()?;

    let mut command = Command::new("bash");
    command
        .args(["-c", &wrapper])
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .process_group(0);

    let child = command.spawn()?;
    let pid = child.id();
    let st = crate::fleet::proc_pid_starttime(pid).unwrap_or(0);
    fs::write(&pid_file, format!("{pid} {st}\n"))?;
    println!(
        "Started job '{}' (pid {pid}) -> {}",
        args.name,
        log_file.display()
    );
    Ok(())
}

/// Executes `tacho wait <name> [--timeout <secs>]`.
/// Streams new log lines and returns:
/// - `Ok(0)` when the job exits with code 0
/// - `Ok(code)` when the job exits with non-zero code
/// - `Ok(3)` when the job is still running after `min(timeout, MAX_WAIT_SECS)` (9 minutes)
pub fn execute_wait(args: WaitArgs) -> io::Result<i32> {
    if let Some(prob) = job_name_problem(&args.name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }
    let cwd = std::env::current_dir()?;
    let root = repo_root(&cwd).unwrap_or(cwd);
    let dir = job_dir(&root);
    let pid_file = dir.join(format!("{}.pid", args.name));
    let log_file = dir.join(format!("{}.log", args.name));
    let exit_file = dir.join(format!("{}.exit", args.name));

    let pid_str = fs::read_to_string(&pid_file).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no job '{}' found in {}", args.name, dir.display()),
        )
    })?;
    let mut parts = pid_str.split_whitespace();
    let pid: u32 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid pid file"))?;
    let _starttime: Option<u64> = parts.next().and_then(|s| s.parse().ok());

    let max_wait = args.timeout.unwrap_or(MAX_WAIT_SECS).min(MAX_WAIT_SECS);
    let start = Instant::now();
    let mut offset = 0u64;

    let drain_log = |offset: &mut u64| {
        if let Ok(mut f) = File::open(&log_file) {
            if f.seek(SeekFrom::Start(*offset)).is_ok() {
                let mut buf = Vec::new();
                if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    *offset += buf.len() as u64;
                    let _ = io::stdout().write_all(&buf);
                    let _ = io::stdout().flush();
                }
            }
        }
    };

    let event_watcher = FdEventWatcher::new_for_dir_and_pid(Some(&dir), pid);

    loop {
        drain_log(&mut offset);

        if let Ok(exit_str) = fs::read_to_string(&exit_file) {
            if let Ok(code) = exit_str.trim().parse::<i32>() {
                drain_log(&mut offset);
                return Ok(code);
            }
        }

        if !pid_alive(pid) && !group_alive(pid) {
            drain_log(&mut offset);
            if let Ok(exit_str) = fs::read_to_string(&exit_file) {
                if let Ok(code) = exit_str.trim().parse::<i32>() {
                    return Ok(code);
                }
            }
            return Ok(1);
        }

        if start.elapsed() >= Duration::from_secs(max_wait) {
            eprintln!(
                "job '{}' (pid {pid}) is still running after {}s (re-run `tacho wait {}` to continue)",
                args.name, max_wait, args.name
            );
            return Ok(3);
        }

        let rem_ms = Duration::from_secs(max_wait)
            .saturating_sub(start.elapsed())
            .as_millis()
            .min(100) as i32;
        event_watcher.wait_ms(rem_ms);
    }
}

/// Event-driven waiter backed by Linux `inotify` and `pidfd_open` + `poll(2)`.
struct FdEventWatcher {
    inotify_fd: i32,
    pid_fd: i32,
}

impl FdEventWatcher {
    fn new_for_dir_and_pid(dir: Option<&Path>, pid: u32) -> Self {
        let inotify_fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify_fd >= 0 {
            if let Some(d) = dir {
                if let Ok(c_path) = std::ffi::CString::new(d.as_os_str().as_bytes()) {
                    let mask = libc::IN_MODIFY
                        | libc::IN_CLOSE_WRITE
                        | libc::IN_CREATE
                        | libc::IN_MOVED_TO;
                    unsafe {
                        let _ = libc::inotify_add_watch(inotify_fd, c_path.as_ptr(), mask);
                    }
                }
            }
        }
        let pid_fd = if pid > 0 {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0) as i32 };
            if fd >= 0 {
                fd
            } else {
                -1
            }
        } else {
            -1
        };
        Self { inotify_fd, pid_fd }
    }

    fn wait_ms(&self, timeout_ms: i32) {
        let mut pfds = [
            libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let mut nfds = 0usize;
        if self.inotify_fd >= 0 {
            pfds[nfds].fd = self.inotify_fd;
            nfds += 1;
        }
        if self.pid_fd >= 0 {
            pfds[nfds].fd = self.pid_fd;
            nfds += 1;
        }
        unsafe {
            let ptr = if nfds > 0 {
                pfds.as_mut_ptr()
            } else {
                std::ptr::null_mut()
            };
            let _ = libc::poll(ptr, nfds as libc::nfds_t, timeout_ms.max(1));
        }
        if self.inotify_fd >= 0 {
            let mut buf = [0u8; 1024];
            unsafe {
                while libc::read(
                    self.inotify_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                ) > 0
                {}
            }
        }
    }
}

impl Drop for FdEventWatcher {
    fn drop(&mut self) {
        if self.inotify_fd >= 0 {
            unsafe {
                let _ = libc::close(self.inotify_fd);
            }
        }
        if self.pid_fd >= 0 {
            unsafe {
                let _ = libc::close(self.pid_fd);
            }
        }
    }
}

/// Executes `tacho kill <name> [--now]`.
/// Verifies worktree ownership (`owns(job_cwd, wt_root)`) and process-group leadership (`pgid == pid`)
/// before signaling `-pgid`, and waits for the entire process group to exit.
pub fn execute_kill(args: KillArgs) -> io::Result<()> {
    if let Some(prob) = job_name_problem(&args.name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }
    let cwd = std::env::current_dir()?;
    let root = repo_root(&cwd).unwrap_or(cwd);
    let dir = job_dir(&root);
    let pid_file = dir.join(format!("{}.pid", args.name));
    let cwd_file = dir.join(format!("{}.cwd", args.name));

    let pid_str = fs::read_to_string(&pid_file).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no job '{}' found in {}", args.name, dir.display()),
        )
    })?;
    let mut parts = pid_str.split_whitespace();
    let pid: u32 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid pid file"))?;
    let starttime: Option<u64> = parts.next().and_then(|s| s.parse().ok());

    if let Ok(recorded_cwd) = fs::read_to_string(&cwd_file) {
        let job_path = Path::new(recorded_cwd.trim());
        if !owns(job_path, &root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to kill job '{}' owned by {} from worktree {}",
                    args.name,
                    job_path.display(),
                    root.display()
                ),
            ));
        }
    }

    if !pid_alive(pid) && !group_alive(pid) {
        println!("job '{}' (pid {pid}) is already stopped", args.name);
        return Ok(());
    }

    let pgid = unsafe { libc::getpgid(pid as i32) };
    if pgid > 0 && pgid != pid as i32 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to kill pid {pid}: process group leader is {pgid} (expected {pid})"),
        ));
    }
    if !crate::fleet::verify_agent_pid(pid, &args.name, None, starttime) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to kill pid {pid}: process does not belong to agent {} (possible PID reuse)", args.name),
        ));
    }
    let target = -(pid as i32);

    let pid_watcher = FdEventWatcher::new_for_dir_and_pid(Some(&dir), pid);

    if args.now {
        unsafe {
            let _ = libc::kill(target, libc::SIGKILL);
        }
    } else {
        unsafe {
            let _ = libc::kill(target, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !pid_alive(pid) && !group_alive(pid) {
                break;
            }
            pid_watcher.wait_ms(50);
        }
        if pid_alive(pid) || group_alive(pid) {
            unsafe {
                let _ = libc::kill(target, libc::SIGKILL);
            }
        }
    }

    // Reap/wait up to 2s for process group to clear
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && (pid_alive(pid) || group_alive(pid)) {
        pid_watcher.wait_ms(50);
    }

    println!("Stopped job '{}' (pid {pid})", args.name);
    Ok(())
}

/// Executes `tacho pr "<title>" -m "<description>" [--dry-run]`.
pub fn execute_pr(args: PrArgs) -> io::Result<()> {
    let cwd = std::env::current_dir()?;
    let root = repo_root(&cwd)?;
    let env_map = fs::read_to_string(root.join(".tachoenv"))
        .map(|s| parse_env(&s))
        .unwrap_or_default();

    let base_sha = env_map
        .get("BASE_SHA")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            git_output(&root, &["merge-base", "HEAD", "origin/main"])
                .or_else(|_| git_output(&root, &["rev-parse", "HEAD~1"]))
                .unwrap_or_default()
        });

    let head_sha = git_output(&root, &["rev-parse", "HEAD"])?;
    let head_tree = git_output(&root, &["rev-parse", "HEAD^{tree}"])?;
    let branch = git_output(&root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PR preflight refused:\n  - HEAD is detached; checkout a branch before opening a PR",
        ));
    }
    let commits_raw = git_output(
        &root,
        &["log", "--format=%h%x20%B%x1e", &format!("{base_sha}..HEAD")],
    )
    .unwrap_or_default();
    let commits: Vec<String> = commits_raw
        .split('\x1e')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let merges_raw = git_output(
        &root,
        &["log", "--merges", "--oneline", &format!("{base_sha}..HEAD")],
    )
    .unwrap_or_default();
    let merge_commits: Vec<String> = merges_raw
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    let changed = changed_files_since(&root, &base_sha)?;
    let status_raw = git_output(&root, &["status", "--porcelain"]).unwrap_or_default();
    let dirty_files: Vec<String> = status_raw
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    let proof: Option<ProofSummary> = fs::read_to_string(root.join(".tachoproof/summary.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    let store = open_fleet_store()?;
    let state = store.load()?;
    let refusals = pr_refusals(
        &args.title,
        &args.message,
        &commits,
        &merge_commits,
        &changed,
        &dirty_files,
        &head_sha,
        &head_tree,
        proof.as_ref(),
        &state.area_rules,
    );

    if !refusals.is_empty() {
        let mut msg = String::from("PR preflight refused:\n");
        for r in &refusals {
            msg.push_str(&format!("  - {r}\n"));
        }
        return Err(io::Error::new(io::ErrorKind::InvalidInput, msg.trim_end()));
    }

    let proof_ref = proof.as_ref().unwrap();
    let body = build_body(&args.message, proof_ref);

    if args.dry_run {
        println!("Branch: {branch}");
        println!("Title:  {}", args.title);
        println!("--- Body ---\n{body}");
        return Ok(());
    }

    // Push branch with --force-with-lease
    let push_status = Command::new("git")
        .args(["push", "--force-with-lease", "-u", "origin", &branch])
        .current_dir(&root)
        .status()?;
    if !push_status.success() {
        return Err(io::Error::other(format!(
            "git push --force-with-lease origin {branch} failed"
        )));
    }

    // Create or update draft PR via gh
    let base_branch = env_map
        .get("BASE_PR")
        .filter(|s| !s.is_empty())
        .and_then(|pr_num| {
            Command::new("gh")
                .args([
                    "pr",
                    "view",
                    pr_num,
                    "--json",
                    "headRefName",
                    "-q",
                    ".headRefName",
                ])
                .current_dir(&root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());

    let pr_out = Command::new("gh")
        .args([
            "pr",
            "create",
            "--draft",
            "--base",
            &base_branch,
            "--head",
            &branch,
            "--title",
            &args.title,
            "--body",
            &body,
        ])
        .current_dir(&root)
        .output()?;

    if pr_out.status.success() {
        let url = String::from_utf8_lossy(&pr_out.stdout).trim().to_string();
        println!("{url}");
        if let Some(num_str) = url.rsplit('/').next() {
            if let Ok(n) = num_str.parse::<u64>() {
                println!("PR: #{n}");
            }
        }
    } else {
        // Fallback: update existing PR
        let edit_out = Command::new("gh")
            .args([
                "pr",
                "edit",
                &branch,
                "--title",
                &args.title,
                "--body",
                &body,
            ])
            .current_dir(&root)
            .output()?;
        if !edit_out.status.success() {
            return Err(io::Error::other(format!(
                "gh pr create/edit failed: {}",
                String::from_utf8_lossy(&pr_out.stderr).trim()
            )));
        }
        println!("{}", String::from_utf8_lossy(&edit_out.stdout).trim());
    }

    Ok(())
}

/// Executes `tacho note <key> "<phase>: <line>"`.
pub fn execute_note(args: NoteArgs) -> io::Result<()> {
    let now = now_secs();
    let parsed =
        parse_note(&args.note, now).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let mut store = open_fleet_store()?;
    let because = format!("{}: {}", parsed.phase, parsed.text);
    store.mutate(Some(("watch", "note", &args.key, &because)), |st| {
        let entry = st.notes.entry(args.key.clone()).or_default();
        *entry = with_note(entry, parsed.clone());
        if let Some(agent) = st.agents.get_mut(&args.key) {
            agent.status_note = because.clone();
        }
    })?;

    emit_decision_event(
        &format!("agent:{}", args.key),
        &format!("phase:{}", parsed.phase),
        &parsed.text,
    );
    println!(
        "Recorded note for {}: [{}] {}",
        args.key, parsed.phase, parsed.text
    );
    Ok(())
}

/// Executes `tacho starve [--slice N] [--cpu-set 0-3] [--timeout 300] -- <cmd...>`.
pub fn execute_starve(args: StarveArgs) -> io::Result<i32> {
    if args.cmd.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: tacho starve [--slice N] [--cpu-set 0-3] -- <command> [args...]",
        ));
    }

    let store = open_fleet_store()?;
    let state = store.load()?;
    let batch_running = state
        .batches
        .iter()
        .any(|b| b.status == "running" || b.status == "bisecting");

    let total_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let plan = plan_starve(
        total_cpus,
        args.slice,
        args.cpu_set.as_deref(),
        batch_running,
        args.timeout.unwrap_or(300),
    )
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    if args.dry_run {
        println!("CPUs:    {}", plan.cpu_list);
        println!("Burners: {}", plan.burners);
        println!("Timeout: {}s", plan.timeout_secs);
        println!("Command: {}", args.cmd.join(" "));
        return Ok(0);
    }

    let because = format!(
        "starve slice cpus={} burners={} cmd={}",
        plan.cpu_list,
        plan.burners,
        args.cmd.first().map(String::as_str).unwrap_or("")
    );
    let _ = store.record_decision("watch", "starve", &plan.cpu_list, &because);
    emit_decision_event("system", "kit:starve", &because);

    // Spawn bounded `yes` burners pinned to `plan.cpu_list` in their own process groups with `TACHO_STARVE=1`
    let mut burner_children = Vec::new();
    for _ in 0..plan.burners {
        let mut cmd = Command::new("taskset");
        cmd.args(["-c", &plan.cpu_list, "yes"])
            .env("TACHO_STARVE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if let Ok(child) = cmd.spawn() {
            burner_children.push(child);
        }
    }

    // Spawn target command pinned to `plan.cpu_list`
    let mut target_cmd = Command::new("taskset");
    target_cmd
        .arg("-c")
        .arg(&plan.cpu_list)
        .args(&args.cmd)
        .env("TACHO_STARVE", "1")
        .process_group(0);

    let mut child = match target_cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            for mut b in burner_children {
                unsafe {
                    let _ = libc::kill(-(b.id() as i32), libc::SIGKILL);
                }
                let _ = b.wait();
            }
            return Err(e);
        }
    };

    let start = Instant::now();
    let timeout = Duration::from_secs(plan.timeout_secs);
    let exit_code: i32;
    let home_dir = crate::paths::resolve_home();
    let starve_watcher = FdEventWatcher::new_for_dir_and_pid(Some(&home_dir), child.id());

    loop {
        match child.try_wait()? {
            Some(status) => {
                exit_code = status.code().unwrap_or(1);
                break;
            }
            None => {
                if start.elapsed() >= timeout {
                    eprintln!(
                        "tacho starve: command timed out after {}s",
                        plan.timeout_secs
                    );
                    unsafe {
                        let _ = libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.wait();
                    exit_code = 124;
                    break;
                }
                // Preempt immediately if a verification batch starts
                if let Ok(st) = store.load() {
                    if st
                        .batches
                        .iter()
                        .any(|b| b.status == "running" || b.status == "bisecting")
                    {
                        eprintln!("tacho starve: preempted by starting verification batch");
                        unsafe {
                            let _ = libc::kill(-(child.id() as i32), libc::SIGTERM);
                        }
                        let _ = child.wait();
                        exit_code = 75;
                        break;
                    }
                }
                let rem_ms = timeout.saturating_sub(start.elapsed()).as_millis().min(50) as i32;
                starve_watcher.wait_ms(rem_ms);
            }
        }
    }

    for mut b in burner_children {
        unsafe {
            let _ = libc::kill(-(b.id() as i32), libc::SIGKILL);
        }
        let _ = b.wait();
    }

    Ok(exit_code)
}
