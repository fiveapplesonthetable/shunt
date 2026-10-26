//! Decision log query (`tacho events` with `--since`/`-n`/`-f`), `tacho top`, `tacho serve`,
//! `tacho setup`, `tacho version`, self-update with test & rollback, and owner [`Notifier`]
//! with [`Scrubber`] enabled on all outgoing payloads.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tacho_core::decide::watch::needs_you;
use tacho_core::Scrubber;
use tacho_daemon::{DecisionRecord, FleetState, FleetStore};

use crate::cli::{EventsArgs, ServeArgs, SetupArgs, TopArgs, VersionArgs};

use super::store::{
    git_stdout, live_agent_keys, load_fleet_meta, mutate_fleet_meta, now_secs, open_fleet_store,
    write_atomic, CodeMoveRecord,
};
use crate::kit::use_private_tmp;

// =============================================================================
// Owner Notifier & Outgoing Payload Scrubber
// =============================================================================

/// Outgoing owner notifier that passes every title, body, and snapshot payload through [`Scrubber`].
#[derive(Debug, Default)]
pub struct Notifier {
    scrubber: Scrubber,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            scrubber: Scrubber::new(),
        }
    }

    /// Scrubs arbitrary outgoing text so secrets/tokens never leak to external channels.
    pub fn scrub_text(&self, input: &str) -> String {
        self.scrubber.scrub(input)
    }

    /// Scrubs a JSON value recursively by serializing, scrubbing with [`Scrubber`], and re-parsing.
    pub fn scrub_json(&self, val: &serde_json::Value) -> serde_json::Value {
        let raw = serde_json::to_string_pretty(val).unwrap_or_else(|_| "{}".to_string());
        let clean = self.scrubber.scrub(&raw);
        serde_json::from_str(&clean).unwrap_or_else(|_| val.clone())
    }

    /// Sends a scrubbed notification to the fleet owner (`notify_fn`) and records it in `FleetStore`.
    pub fn notify_owner<F>(
        &self,
        store: &mut FleetStore,
        key: &str,
        subject: &str,
        body: &str,
        notify_fn: F,
    ) -> io::Result<bool>
    where
        F: FnOnce(&str, &str) -> Result<(), String>,
    {
        let st = store.load()?;
        if st.notified_failures.contains(key) {
            return Ok(false);
        }
        let clean_subject = self.scrub_text(subject);
        let clean_body = self.scrub_text(body);
        notify_fn(&clean_subject, &clean_body).map_err(io::Error::other)?;
        store.mutate(
            Some(("watch", "notify_owner", key, &clean_subject)),
            |state| {
                state.notified_failures.insert(key.to_string());
            },
        )?;
        Ok(true)
    }
}

/// Atomically writes a scrubbed JSON snapshot to `path`.
pub fn write_scrubbed_snapshot(path: &Path, payload: &serde_json::Value) -> io::Result<()> {
    let raw = serde_json::to_string_pretty(payload).map_err(io::Error::other)?;
    let scrubbed = Scrubber::new().scrub(&raw);
    write_atomic(path, scrubbed.as_bytes())
}

// =============================================================================
// Events (`tacho events` with `--since`, `-n`, `-f`)
// =============================================================================

/// Options for `tacho events`.
#[derive(Debug, Clone, Default)]
pub struct EventsOptions {
    pub limit: Option<usize>,
    pub since: Option<String>,
    pub follow: bool,
    pub category: Option<String>,
    pub target: Option<String>,
    pub json: bool,
}

impl From<EventsArgs> for EventsOptions {
    fn from(args: EventsArgs) -> Self {
        Self {
            limit: args.limit,
            since: args.since,
            follow: args.follow,
            category: args.category,
            target: args.target,
            json: args.json,
        }
    }
}

/// Parses a `--since` duration (`30s`, `15m`, `2h`, `7d`) or unix timestamp into a cutoff epoch second.
pub fn parse_since_cutoff(raw: &str, now: i64) -> Result<i64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty --since value".to_string());
    }
    if let Ok(ts) = s.parse::<i64>() {
        if ts > 0 {
            return Ok(ts);
        }
        return Err(format!("invalid --since timestamp '{raw}'"));
    }
    let (num_str, mult) = if let Some(n) = s.strip_suffix('s') {
        (n, 1i64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60i64)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600i64)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400i64)
    } else {
        return Err(format!(
            "invalid --since '{raw}': expected <N>s, <N>m, <N>h, <N>d, or unix timestamp"
        ));
    };
    let n: i64 = num_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid --since '{raw}'"))?;
    if n < 0 {
        return Err(format!("--since duration cannot be negative: '{raw}'"));
    }
    Ok(now - n * mult)
}

/// Filters [`DecisionRecord`] items by `since_cutoff`, `category`, `target`, and `limit` (keeping the most recent `limit` items).
pub fn filter_decision_events(
    records: &[DecisionRecord],
    since_cutoff: Option<i64>,
    category: Option<&str>,
    target: Option<&str>,
    limit: usize,
) -> Vec<DecisionRecord> {
    let mut filtered: Vec<DecisionRecord> = records
        .iter()
        .filter(|r| {
            since_cutoff.map(|c| r.ts >= c).unwrap_or(true)
                && category.map(|cat| r.category == cat).unwrap_or(true)
                && target.map(|tgt| r.target == tgt).unwrap_or(true)
        })
        .cloned()
        .collect();
    if filtered.len() > limit {
        let drop_n = filtered.len() - limit;
        filtered.drain(0..drop_n);
    }
    filtered
}

/// Executes `tacho events [--limit 50] [--since ...] [--follow] [--category ...] [--target ...]`.
pub fn execute_events(args: EventsArgs) -> io::Result<()> {
    execute_events_with(EventsOptions::from(args))
}

/// Executes `tacho events` with [`EventsOptions`].
pub fn execute_events_with(args: EventsOptions) -> io::Result<()> {
    let store = open_fleet_store()?;
    let now = now_secs();
    let since_cutoff = match args.since.as_deref() {
        Some(raw) => Some(
            parse_since_cutoff(raw, now)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
        ),
        None => None,
    };
    let limit = args.limit.unwrap_or(50);
    let fetch_n = if since_cutoff.is_some() {
        limit.max(500)
    } else {
        limit
    };
    let raw_records = store
        .recent_decisions(fetch_n, args.category.as_deref(), args.target.as_deref())
        .map_err(|e| io::Error::other(e.to_string()))?;
    let records = filter_decision_events(
        &raw_records,
        since_cutoff,
        args.category.as_deref(),
        args.target.as_deref(),
        limit,
    );

    let scrubber = Scrubber::new();
    if args.json {
        let raw = serde_json::to_string_pretty(&records).unwrap_or_default();
        println!("{}", scrubber.scrub(&raw));
        return Ok(());
    }
    if records.is_empty() && !args.follow {
        if let Some(ref s) = args.since {
            println!("No decisions recorded since {s}.");
        } else {
            println!("No decisions recorded yet.");
        }
        return Ok(());
    }
    let mut last_id = 0i64;
    for r in &records {
        last_id = last_id.max(r.id);
        let line = format!(
            "[{}] {:<10} {:<14} {:<18} because: {}",
            r.ts, r.category, r.action, r.target, r.because
        );
        println!("{}", scrubber.scrub(&line));
    }

    if args.follow {
        let max_follow = std::env::var("TACHO_EVENTS_FOLLOW_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(2);
        let watcher = crate::kit::FdEventWatcher::new().ok();
        if let Some(ref w) = watcher {
            w.watch_path(store.home());
            w.watch_path(&store.home().join("events.sqlite"));
            w.watch_path(&store.home().join("events.sqlite-wal"));
        }
        let deadline = Instant::now() + Duration::from_secs(max_follow);
        while Instant::now() < deadline {
            if let Ok(latest) =
                store.recent_decisions(50, args.category.as_deref(), args.target.as_deref())
            {
                for r in latest {
                    if r.id > last_id && since_cutoff.map(|c| r.ts >= c).unwrap_or(true) {
                        last_id = r.id;
                        let line = format!(
                            "[{}] {:<10} {:<14} {:<18} because: {}",
                            r.ts, r.category, r.action, r.target, r.because
                        );
                        println!("{}", scrubber.scrub(&line));
                    }
                }
            }
            let rem_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(250) as i32;
            if rem_ms <= 0 {
                break;
            }
            if let Some(ref w) = watcher {
                w.wait_ms(rem_ms);
            } else {
                break;
            }
        }
    }
    Ok(())
}

// =============================================================================
// Top & Serve (`tacho top`, `tacho serve`)
// =============================================================================

/// Executes `tacho top` (compact live dashboard).
pub fn execute_top(args: TopArgs) -> io::Result<()> {
    let store = open_fleet_store()?;
    let state = store.load()?;
    let now = now_secs();
    let live = live_agent_keys(&state);
    let attention = needs_you(
        &state.agents,
        &state.todo,
        &state.retired,
        &state.holds,
        &live,
        &state.open_prs_cache,
        &state.passed,
        &state.failed,
        now,
    );

    let scrubber = Scrubber::new();
    if args.json {
        let raw = serde_json::to_string_pretty(&serde_json::json!({
            "needs_you": attention,
            "active_workers": live,
            "todo_count": state.todo.len(),
            "holds_count": state.holds.len(),
            "batches_count": state.batches.len(),
        }))
        .unwrap_or_default();
        println!("{}", scrubber.scrub(&raw));
        return Ok(());
    }

    println!(
        "=== TACHO TOP (workers: {}/{}, todo: {}, holds: {}) ===",
        live.len(),
        state.max_workers,
        state.todo.len(),
        state.holds.len()
    );
    if !attention.is_empty() {
        println!("\nNEEDS YOU ({}):", attention.len());
        for item in &attention {
            let line = format!(
                "  [{}] {}: {}  -> `{}`",
                item.kind, item.target, item.summary, item.command_hint
            );
            println!("{}", scrubber.scrub(&line));
        }
    }
    if !live.is_empty() {
        println!("\nACTIVE WORKERS:");
        for k in &live {
            if let Some(a) = state.agents.get(k) {
                let line = format!(
                    "  {:<20} area={:<12} provider={:<8} note={}",
                    a.key,
                    if a.area.is_empty() { "-" } else { &a.area },
                    a.provider,
                    a.status_note
                );
                println!("{}", scrubber.scrub(&line));
            }
        }
    }
    Ok(())
}

/// Builds a scrubbed JSON string payload for `tacho serve`.
pub fn build_serve_payload(state: &FleetState) -> String {
    let raw = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
    Scrubber::new().scrub(&raw)
}

/// Executes `tacho serve [--addr 127.0.0.1:7420]` (local read-only status HTTP server).
pub fn execute_serve(args: ServeArgs) -> io::Result<()> {
    let addr = args.addr.unwrap_or_else(|| "127.0.0.1:7420".to_string());
    let listener = TcpListener::bind(&addr)?;
    println!("Serving read-only fleet status on http://{addr}");
    for stream in listener.incoming() {
        let Ok(mut s) = stream else {
            continue;
        };
        let mut req_buf = [0u8; 1024];
        let _ = s.read(&mut req_buf);
        let store = open_fleet_store()?;
        let state = store.load()?;
        let body = build_serve_payload(&state);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(resp.as_bytes());
        if args.once {
            break;
        }
    }
    Ok(())
}

// =============================================================================
// Self-Update (`run_self_update`) & Version (`execute_version`)
// =============================================================================

/// Outcome of a self-update attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfUpdateOutcome {
    /// Already at `target_sha` or `target_sha` was previously refused.
    Skipped(String),
    /// Fast-forwarded to `target_sha` and self-test succeeded.
    Updated { from_sha: String, to_sha: String },
    /// Self-test failed at `target_sha`; rolled back to `from_sha` and recorded `refused_self_update_sha`.
    RolledBack {
        from_sha: String,
        refused_sha: String,
        reason: String,
    },
}

/// Runs a self-update to `target_sha` in `repo_dir`, executing `test_cmd` to verify the new build
/// and automatically rolling back (`git checkout --detach <old_sha>`) and recording
/// `refused_self_update_sha` if the test command fails.
pub fn run_self_update(
    store: &mut FleetStore,
    repo_dir: &Path,
    target_sha: &str,
    test_cmd: &str,
) -> io::Result<SelfUpdateOutcome> {
    let state = store.load()?;
    if state.refused_self_update_sha.as_deref() == Some(target_sha) {
        return Ok(SelfUpdateOutcome::Skipped(format!(
            "commit {target_sha} is in refused_self_update_sha"
        )));
    }
    let from_sha = git_stdout(repo_dir, &["rev-parse", "HEAD"])?;
    if from_sha == target_sha {
        return Ok(SelfUpdateOutcome::Skipped(
            "already at target_sha".to_string(),
        ));
    }

    git_stdout(repo_dir, &["checkout", "--detach", target_sha])?;
    let mut test = Command::new("bash");
    test.args(["-c", test_cmd]).current_dir(repo_dir);
    use_private_tmp(&mut test, "self-update")?;
    let test_out = test.output()?;

    let now = now_secs();
    if test_out.status.success() {
        let because = format!("self-updated from {from_sha} to {target_sha}");
        store.mutate(Some(("watch", "self_update", target_sha, &because)), |st| {
            st.refused_self_update_sha = None;
        })?;
        mutate_fleet_meta(|m| {
            m.last_code_moved = Some(CodeMoveRecord {
                from_sha: from_sha.clone(),
                to_sha: target_sha.to_string(),
                at: now,
            });
        })?;
        Ok(SelfUpdateOutcome::Updated {
            from_sha,
            to_sha: target_sha.to_string(),
        })
    } else {
        if let Err(e) = git_stdout(repo_dir, &["checkout", "--detach", &from_sha]) {
            eprintln!("[tacho-self-update] warning: rollback checkout to {from_sha} failed: {e}");
        }
        let err_msg = format!(
            "{}{}",
            String::from_utf8_lossy(&test_out.stdout),
            String::from_utf8_lossy(&test_out.stderr)
        )
        .trim()
        .to_string();
        let because = format!(
            "self-update to {target_sha} failed self-test ({err_msg}); rolled back to {from_sha}"
        );
        store.mutate(
            Some(("watch", "self_update_rollback", target_sha, &because)),
            |st| {
                st.refused_self_update_sha = Some(target_sha.to_string());
            },
        )?;
        Ok(SelfUpdateOutcome::RolledBack {
            from_sha,
            refused_sha: target_sha.to_string(),
            reason: err_msg,
        })
    }
}

/// Executes `tacho version [--json]`.
pub fn execute_version(args: VersionArgs) -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let git_out = |git_args: &[&str]| -> Option<String> {
        Command::new("git")
            .args(git_args)
            .current_dir(&cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let running_commit = git_out(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let main_commit = git_out(&["rev-parse", "origin/main"])
        .or_else(|| git_out(&["rev-parse", "main"]))
        .unwrap_or_else(|| running_commit.clone());
    let commits_behind: u64 = git_out(&["rev-list", "--count", "HEAD..origin/main"])
        .or_else(|| git_out(&["rev-list", "--count", "HEAD..main"]))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let meta = load_fleet_meta().unwrap_or_default();
    let last_move = if let Some(mv) = meta.last_code_moved {
        format!("{} -> {} at {}", mv.from_sha, mv.to_sha, mv.at)
    } else {
        open_fleet_store()
            .ok()
            .and_then(|st| st.recent_decisions(50, Some("watch"), None).ok())
            .and_then(|recs| {
                recs.into_iter()
                    .find(|r| r.action == "self_update")
                    .map(|r| format!("{} at {} ({})", r.target, r.ts, r.because))
            })
            .unwrap_or_else(|| "none".to_string())
    };

    let rule =
        "Auto-updates when detached and origin/main has a green commit (skips refused commits).";

    if args.json {
        let payload = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "running_commit": running_commit,
            "main_commit": main_commit,
            "commits_behind": commits_behind,
            "last_move": last_move,
            "rule": rule,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        println!("tacho {}", env!("CARGO_PKG_VERSION"));
        println!("running commit: {running_commit}");
        println!("main commit:    {main_commit} ({commits_behind} behind)");
        println!("last move:      {last_move}");
        println!("rule:           {rule}");
    }
    Ok(())
}

// =============================================================================
// Setup (`tacho setup`, symlink inspection, idempotent unit/env installation)
// =============================================================================

/// Observed filesystem state at a target symlink location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Missing,
    SymlinkTo(PathBuf),
    RegularFile,
}

/// Planned action to ensure a symlink points to `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAction {
    Noop,
    CreateSymlink { link: PathBuf, target: PathBuf },
    Relink { link: PathBuf, target: PathBuf },
    RefuseRegularFile { link: PathBuf },
}

/// Inspects `link` (without following symlinks) and determines its [`LinkState`].
pub fn observe_link_state(link: &Path) -> LinkState {
    let Ok(meta) = fs::symlink_metadata(link) else {
        return LinkState::Missing;
    };
    if meta.file_type().is_symlink() {
        if let Ok(target) = fs::read_link(link) {
            return LinkState::SymlinkTo(target);
        }
    }
    LinkState::RegularFile
}

/// Plans and optionally applies a symlink update from `link -> target`.
pub fn apply_link_action(link: &Path, target: &Path, dry_run: bool) -> io::Result<LinkAction> {
    let action = match observe_link_state(link) {
        LinkState::Missing => LinkAction::CreateSymlink {
            link: link.to_path_buf(),
            target: target.to_path_buf(),
        },
        LinkState::SymlinkTo(curr) if curr == target => LinkAction::Noop,
        LinkState::SymlinkTo(_) => LinkAction::Relink {
            link: link.to_path_buf(),
            target: target.to_path_buf(),
        },
        LinkState::RegularFile => LinkAction::RefuseRegularFile {
            link: link.to_path_buf(),
        },
    };

    if !dry_run {
        match &action {
            LinkAction::Noop | LinkAction::RefuseRegularFile { .. } => {}
            LinkAction::CreateSymlink { link, target } => {
                if let Some(parent) = link.parent() {
                    fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, link)?;
            }
            LinkAction::Relink { link, target } => {
                let _ = fs::remove_file(link);
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, link)?;
            }
        }
    }
    Ok(action)
}

/// Structured report from `run_setup_with`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupReport {
    pub home_dir: PathBuf,
    pub env_file: PathBuf,
    pub env_created: bool,
    pub systemd_dir: PathBuf,
    pub units_installed: Vec<String>,
    pub missing_deps: Vec<String>,
}

/// Runs idempotent fleet setup (environment file, systemd user units, symlinks, and missing CLI dependency check).
pub fn run_setup_with(
    home_dir: &Path,
    systemd_dir: &Path,
    required_bins: &[&str],
    dry_run: bool,
) -> io::Result<SetupReport> {
    let env_file = home_dir.join("fleet.env");
    let units = [
        "tacho.service",
        "tacho.timer",
        "tacho-watchdog.service",
        "tacho-watchdog.timer",
    ];
    let mut env_created = false;
    let mut units_installed = Vec::new();

    if !dry_run {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(home_dir)?;
        for sub in [
            "warm",
            "sccache",
            "cache/npm",
            "cache/cargo-registry",
            "tmp",
            "runner-tmp",
        ] {
            let dir = home_dir.join(sub);
            fs::create_dir_all(&dir)?;
            if sub == "tmp" || sub == "runner-tmp" {
                let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
            }
        }
        let runner_dropin_dir = systemd_dir.join("tacho-runner@.service.d");
        fs::create_dir_all(&runner_dropin_dir)?;
        let runner_tmp = home_dir.join("runner-tmp");
        let dropin_content =
            tacho_core::decide::ops::config::runner_tmp_dropin(&runner_tmp.to_string_lossy());
        write_atomic(
            &runner_dropin_dir.join("50-tmp.conf"),
            dropin_content.as_bytes(),
        )?;
        if !env_file.exists() {
            let default_env = "# Tachograph fleet environment configuration\nTACHO_MAX_WORKERS=4\n";
            fs::write(&env_file, default_env)?;
            env_created = true;
        }
        let unit_src_dir = home_dir.join("units");
        fs::create_dir_all(&unit_src_dir)?;
        fs::create_dir_all(systemd_dir)?;
        for u in &units {
            let src = unit_src_dir.join(u);
            if !src.exists() {
                fs::write(&src, format!("# Generated {u}\n"))?;
            }
            let dst = systemd_dir.join(u);
            let _ = apply_link_action(&dst, &src, false)?;
            units_installed.push((*u).to_string());
        }
    } else {
        env_created = !env_file.exists();
        units_installed = units.iter().map(|s| (*s).to_string()).collect();
    }

    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let path_dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    let mut missing_deps = Vec::new();
    for &bin in required_bins {
        let found = path_dirs.iter().any(|d| d.join(bin).is_file());
        if !found {
            missing_deps.push(bin.to_string());
        }
    }
    let _ = BTreeSet::<String>::new();

    Ok(SetupReport {
        home_dir: home_dir.to_path_buf(),
        env_file,
        env_created,
        systemd_dir: systemd_dir.to_path_buf(),
        units_installed,
        missing_deps,
    })
}

/// Executes `tacho setup [--dry-run]`.
pub fn execute_setup(args: SetupArgs) -> io::Result<()> {
    let home = crate::paths::resolve_home();
    let systemd_dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".config/systemd/user");
    let report = run_setup_with(
        &home,
        &systemd_dir,
        &["git", "gh", "cargo", "timeout"],
        args.dry_run,
    )?;
    if args.dry_run {
        println!(
            "[dry-run] Would ensure TACHO_HOME directory at {}",
            report.home_dir.display()
        );
        println!(
            "[dry-run] Would ensure fleet environment file at {}",
            report.env_file.display()
        );
        println!(
            "[dry-run] Would check/link systemd user units in {}: {}",
            report.systemd_dir.display(),
            report.units_installed.join(", ")
        );
    } else {
        if report.env_created {
            println!("Created {}", report.env_file.display());
        } else {
            println!("Found existing {}", report.env_file.display());
        }
        println!(
            "Systemd user unit directory: {} (units: {})",
            report.systemd_dir.display(),
            report.units_installed.join(", ")
        );
        if !report.missing_deps.is_empty() {
            println!(
                "Missing optional dependencies: {}",
                report.missing_deps.join(", ")
            );
        }
    }
    Ok(())
}
