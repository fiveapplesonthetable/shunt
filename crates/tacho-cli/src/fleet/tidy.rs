//! Worktree/container janitor (`tacho tidy`), rescue commits (`refs/heads/rescue/<key>-<ts>`
//! and `refs/tacho/rescue/<key>-<ts>` without `git stash`), live-process `cwd`/`argv`/`fd` keep
//! checks, and `--stale-runs DAYS`.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use tacho_core::decide::dispatch::retire;
use tacho_core::decide::watch::{
    decide_tidy, stale_runs, zombie_lines, ProcEntry, TidyDecision, TidySnapshot, WorktreeCandidate,
};
use tacho_core::decide::{decide, DecisionState, WorldObservation};
use tacho_daemon::{FleetState, FleetStore};

use crate::cli::TidyArgs;

use super::store::{
    git_stdout, live_agent_keys, mutate_fleet_meta, now_secs, open_fleet_store, pid_alive_matching,
    RetiredMeta,
};
use super::watch::scan_procs;

/// Paths and ports held open by live processes on the host (`cwd`, `argv`, `/proc/<pid>/fd`).
#[derive(Debug, Clone, Default)]
pub struct LiveProcessHolds {
    pub paths: BTreeSet<String>,
    pub ports: BTreeSet<u16>,
}

/// Scans `/proc` and `procs` for any worktree paths referenced by live process `cwd`, `argv`,
/// or open file descriptors (`/proc/<pid>/fd/*`).
pub fn scan_live_process_holds(
    procs: &[ProcEntry],
    candidate_paths: &[String],
) -> LiveProcessHolds {
    let mut holds = LiveProcessHolds::default();
    if candidate_paths.is_empty() {
        return holds;
    }

    for p in procs {
        for cand in candidate_paths {
            if cand.is_empty() {
                continue;
            }
            if (!p.cwd.is_empty() && (p.cwd == *cand || p.cwd.starts_with(&format!("{cand}/"))))
                || p.args.contains(cand)
            {
                holds.paths.insert(cand.clone());
            }
        }
        let fd_dir = format!("/proc/{}/fd", p.pid);
        if let Ok(entries) = fs::read_dir(&fd_dir) {
            for entry in entries.flatten() {
                if let Ok(target) = fs::read_link(entry.path()) {
                    let target_str = target.to_string_lossy();
                    for cand in candidate_paths {
                        if !cand.is_empty()
                            && (target_str == *cand || target_str.starts_with(&format!("{cand}/")))
                        {
                            holds.paths.insert(cand.clone());
                        }
                    }
                }
            }
        }
    }

    holds
}

/// Builds a [`TidySnapshot`] from [`FleetState`] and live process holds.
pub fn build_tidy_snapshot(
    state: &FleetState,
    live_keys: &BTreeSet<String>,
    now: i64,
) -> TidySnapshot {
    let mut live_wt = BTreeSet::new();
    let mut held_ports = BTreeSet::new();
    for a in state.agents.values() {
        if live_keys.contains(&a.key) && !a.worktree.is_empty() {
            live_wt.insert(a.worktree.clone());
        }
    }
    for wt in state.worktrees.values() {
        if live_wt.contains(&wt.path) {
            for p in wt.port..=wt.port_last {
                held_ports.insert(p);
            }
        }
    }

    let mut wt_candidates = Vec::new();
    for wt in state.worktrees.values() {
        let p = Path::new(&wt.path);
        if !p.exists() {
            continue;
        }
        let dirty = Command::new("git")
            .args(["-C", &wt.path, "status", "--porcelain"])
            .output()
            .ok()
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false);
        let unpushed = Command::new("git")
            .args([
                "-C",
                &wt.path,
                "rev-list",
                "--count",
                &format!("{}..HEAD", wt.base_sha),
            ])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
            })
            .unwrap_or(0);
        let pr_merged_or_closed = state
            .agents
            .get(&wt.slug)
            .and_then(|a| a.pr)
            .is_some_and(|pr| state.merged_prs.contains(&pr) || state.closed_prs.contains(&pr));
        wt_candidates.push(WorktreeCandidate {
            path: wt.path.clone(),
            branch: wt.branch.clone(),
            age_secs: (now - wt.created_at).max(0),
            dirty,
            unpushed_commits: unpushed,
            pr_merged_or_closed,
        });
    }

    TidySnapshot {
        now,
        worktrees: wt_candidates,
        checkouts: Vec::new(),
        containers: Vec::new(),
        networks: Vec::new(),
        live_agent_worktrees: live_wt,
        live_batch_worktrees: BTreeSet::new(),
        held_paths: BTreeSet::new(),
        held_ports,
    }
}

/// Executes [`TidyDecision`] items against the worktree filesystem and [`FleetStore`].
pub fn apply_tidy_decisions(
    store: &mut FleetStore,
    decisions: &[TidyDecision],
    now: i64,
    dry_run: bool,
) -> io::Result<()> {
    for d in decisions {
        match d {
            TidyDecision::RemoveTree {
                path,
                rescue,
                because,
            } => {
                println!(
                    "{}RemoveTree {path} (rescue={rescue}): {because}",
                    if dry_run { "[dry-run] " } else { "" }
                );
                if !dry_run {
                    if *rescue {
                        if let Err(e) = rescue_worktree(store, Path::new(path), now) {
                            let msg = format!("keeping worktree {path}: rescue failed: {e}");
                            eprintln!("[tacho-fleet] {msg}");
                            store.record_decision("watch", "rescue_failed", path, &msg)?;
                            continue;
                        }
                    }
                    remove_worktree(store, path, because)?;
                }
            }
            other => {
                println!("{}{:?}", if dry_run { "[dry-run] " } else { "" }, other);
            }
        }
    }
    Ok(())
}

/// Removes a worktree and forgets it, reporting (not swallowing) failures. The store entry is
/// kept when git refuses, so a later tidy retries instead of leaking an untracked directory.
fn remove_worktree(store: &mut FleetStore, path: &str, because: &str) -> io::Result<()> {
    let out = Command::new("git")
        .args(["worktree", "remove", "--force", path])
        .output()?;
    if !out.status.success() && Path::new(path).exists() {
        return Err(io::Error::other(format!(
            "git worktree remove {path} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    store.mutate(Some(("watch", "remove_worktree", path, because)), |st| {
        st.worktrees.retain(|_, w| w.path != path);
    })
}

/// Archives a worktree's full state (tracked, untracked, unpushed) as a commit under
/// `refs/heads/rescue/<slug>-<now>` (and `refs/tacho/rescue/<slug>-<now>`) without touching
/// `git stash` or the real git index.
pub fn rescue_worktree(store: &mut FleetStore, wt_p: &Path, now: i64) -> io::Result<String> {
    let slug = wt_p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("worktree");
    let tree_sha = crate::kit::tree_of_worktree(wt_p)?;
    // An unborn branch has no HEAD, so the snapshot becomes a root commit; any other
    // rev-parse failure would silently drop history from the rescue, so it is reported.
    let has_head = git_stdout(wt_p, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).is_ok();
    let parent_sha = if has_head {
        git_stdout(wt_p, &["rev-parse", "HEAD"])?
    } else {
        String::new()
    };
    let message = format!("tacho rescue snapshot for {slug}");
    let mut commit_args = vec!["commit-tree", tree_sha.as_str()];
    if !parent_sha.is_empty() {
        commit_args.extend(["-p", parent_sha.as_str()]);
    }
    commit_args.extend(["-m", message.as_str()]);
    let commit_sha = git_stdout(wt_p, &commit_args)?;
    let rescue_branch_ref = format!("refs/heads/rescue/{slug}-{now}");
    let rescue_tacho_ref = format!("refs/tacho/rescue/{slug}-{now}");
    git_stdout(wt_p, &["update-ref", &rescue_branch_ref, &commit_sha])?;
    git_stdout(wt_p, &["update-ref", &rescue_tacho_ref, &commit_sha])?;
    let because = format!("archived dirty/unpushed worktree to {rescue_branch_ref} ({commit_sha})");
    store.record_decision("watch", "rescue_worktree", slug, &because)?;
    Ok(rescue_branch_ref)
}

/// Retires unfinished/stopped agent runs older than `days` (`>= 1`) that have no open PR.
pub fn retire_stale_runs(
    store: &mut FleetStore,
    days: i64,
    now: i64,
    dry_run: bool,
) -> io::Result<Vec<String>> {
    if days < 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--stale-runs must be at least 1 day",
        ));
    }
    let state = store.load()?;
    let live = live_agent_keys(&state);
    let cutoff = now - days * 86_400;
    let open_prs: BTreeSet<u64> = state.open_prs_cache.iter().map(|p| p.number).collect();

    let mut stale_keys: Vec<String> = state
        .agents
        .values()
        .filter(|a| {
            let alive = live.contains(&a.key);
            let has_open_pr = a.pr.is_some_and(|p| open_prs.contains(&p));
            let age_ts = a.finished_at.unwrap_or(a.started_at);
            !alive && !has_open_pr && !state.retired.contains(&a.key) && age_ts <= cutoff
        })
        .map(|a| a.key.clone())
        .collect();
    stale_keys.sort();

    if stale_keys.is_empty() {
        return Ok(Vec::new());
    }
    let why = format!("stale >{days}d");
    if !dry_run {
        let because = format!(
            "retired {} stale run(s) older than {}d: {:?}",
            stale_keys.len(),
            days,
            stale_keys
        );
        store.mutate(
            Some(("watch", "tidy_stale_runs", "agents", &because)),
            |st| {
                for k in &stale_keys {
                    let (new_todo, new_retired, new_holds) =
                        retire(k, &st.todo, &st.agents, &st.retired, &st.holds);
                    st.todo = new_todo;
                    st.retired = new_retired;
                    st.holds = new_holds;
                }
            },
        )?;
        mutate_fleet_meta(|m| {
            for k in &stale_keys {
                m.retired.insert(
                    k.clone(),
                    RetiredMeta {
                        key: k.clone(),
                        why: why.clone(),
                        at: now,
                        task: String::new(),
                        held_for: String::new(),
                    },
                );
            }
        })?;
    }
    Ok(stale_keys)
}

/// Options for `tacho tidy`.
#[derive(Debug, Clone, Default)]
pub struct TidyOptions {
    pub dry_run: bool,
    pub min_age: Option<i64>,
    pub stale_runs_days: Option<i64>,
}

impl From<TidyArgs> for TidyOptions {
    fn from(args: TidyArgs) -> Self {
        Self {
            dry_run: args.dry_run,
            min_age: args.min_age,
            stale_runs_days: args.stale_runs,
        }
    }
}

/// Executes `tacho tidy [--dry-run] [--min-age 3600] [--stale-runs DAYS]`.
pub fn execute_tidy(args: TidyArgs) -> io::Result<()> {
    execute_tidy_with(TidyOptions::from(args))
}

/// Executes `tacho tidy` with [`TidyOptions`].
pub fn execute_tidy_with(args: TidyOptions) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let now = now_secs();

    if let Some(days) = args.stale_runs_days {
        let retired_keys = retire_stale_runs(&mut store, days, now, args.dry_run)?;
        if !retired_keys.is_empty() {
            println!(
                "{}Retired {} stale run(s) older than {days}d: {}",
                if args.dry_run { "[dry-run] " } else { "" },
                retired_keys.len(),
                retired_keys.join(", ")
            );
        }
    }

    let state = store.load()?;
    let procs = scan_procs();
    let mut live_pids: BTreeSet<u32> = procs.iter().map(|p| p.pid).collect();
    for a in state.agents.values() {
        if let Some(pid) = a.pid {
            if !pid_alive_matching(pid, a.starttime) {
                live_pids.remove(&pid);
            }
        }
    }

    // Reap stale runs whose PID exited or whose PID was recycled with a different starttime
    let stale = stale_runs(&state.agents, &live_pids);
    if !stale.is_empty() && !args.dry_run {
        store.mutate(
            Some((
                "watch",
                "reap_stale",
                "agents",
                &format!("marked {:?} finished", stale),
            )),
            |st| {
                for k in &stale {
                    if let Some(a) = st.agents.get_mut(k) {
                        a.finished_at = Some(now);
                        if a.exit_code.is_none() {
                            a.exit_code = Some(1);
                        }
                    }
                }
            },
        )?;
    }

    let live_keys = live_agent_keys(&state);
    let mut snap = build_tidy_snapshot(&state, &live_keys, now);
    let candidate_paths: Vec<String> = snap.worktrees.iter().map(|w| w.path.clone()).collect();
    let live_holds = scan_live_process_holds(&procs, &candidate_paths);
    snap.held_paths.extend(live_holds.paths);
    snap.held_ports.extend(live_holds.ports);

    let min_age_secs = args.min_age.unwrap_or(3_600);
    let decision_state = DecisionState {
        todo: state.todo.clone(),
        agents: state.agents.clone(),
        retired: state.retired.clone(),
        holds: state.holds.clone(),
        tidy_min_age_secs: min_age_secs,
        ..Default::default()
    };
    let world_obs = WorldObservation {
        now,
        live_keys,
        tidy_snapshot: snap,
        ..Default::default()
    };
    let mut plan = decide(&decision_state, &world_obs);
    if min_age_secs <= 0 {
        for d in decide_tidy(&world_obs.tidy_snapshot, min_age_secs) {
            if !plan.tidy_decisions.contains(&d) {
                plan.tidy_decisions.push(d);
            }
        }
    }
    if !args.dry_run {
        store
            .record_decision_tick(now, &decision_state, &world_obs, &plan)
            .map_err(|e| io::Error::other(e.to_string()))?;
    }

    let decisions = plan.tidy_decisions;
    let zombies = zombie_lines(&procs);

    if decisions.is_empty() && stale.is_empty() && zombies.is_empty() {
        println!("Machine is tidy (nothing to clean up).");
        return Ok(());
    }

    for z in &zombies {
        println!("Zombie warning: {z}");
    }
    apply_tidy_decisions(&mut store, &decisions, now, args.dry_run)?;
    Ok(())
}
