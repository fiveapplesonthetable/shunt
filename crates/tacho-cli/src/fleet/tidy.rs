//! Worktree/container janitor (`tacho tidy`), rescue commits (`refs/heads/rescue/<key>-<ts>`
//! and `refs/tacho/rescue/<key>-<ts>` without `git stash`), live-process `cwd`/`argv`/`fd` keep
//! checks, and `--stale-runs DAYS`.

use std::collections::BTreeSet;
use std::io;

use tacho_core::decide::dispatch::retire;
use tacho_core::decide::watch::{decide_tidy, stale_runs, zombie_lines, TidySnapshot};
use tacho_core::decide::{decide, DecisionState, WorldObservation};
pub use tacho_daemon::exec::tidy::{
    apply_tidy_decisions, rescue_worktree, scan_live_process_holds, LiveProcessHolds,
};
use tacho_daemon::{FleetState, FleetStore};

use crate::cli::TidyArgs;

use super::store::{
    live_agent_keys, mutate_fleet_meta, now_secs, open_fleet_store, pid_alive_matching, RetiredMeta,
};
use super::watch::scan_procs;

/// Builds a [`TidySnapshot`] from [`FleetState`] and live process holds.
pub fn build_tidy_snapshot(
    state: &FleetState,
    live_keys: &BTreeSet<String>,
    now: i64,
) -> TidySnapshot {
    tacho_daemon::exec::observe::build_tidy_snapshot(
        state,
        &tacho_core::LiveState::default(),
        live_keys,
        now,
    )
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
