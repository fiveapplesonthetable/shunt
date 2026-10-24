//! Process tree sampling (`scan_procs`), progress verdicts (`tacho progress`),
//! orchestrator tick (`tacho tick`), and `tacho watchdog` `OPEN`/`UPDATE`/`CLOSE` issue lifecycle.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use tacho_core::decide::dispatch::{base_key, resume_task, TodoItem};
use tacho_core::decide::land::open_for;
use tacho_core::decide::verify::flake_tasks;
use tacho_core::decide::watch::{
    agent_row, burners, chain_rows, down_alert, down_reason, duplicate_batches, idle_chains,
    BehindCommit, DeployRunObs, ProcEntry, ProgressSample, SuperviseAction,
};
use tacho_core::decide::{decide, BatchOutcomeObs, BurnerProcess, DecisionState, WorldObservation};
use tacho_core::{Scrubber, TaskStatus};

use crate::cli::{ProgressArgs, TickArgs, WatchdogArgs};

use super::dispatch::{deliver_tell_action, spawn_worker_for_item};
use super::ops::write_scrubbed_snapshot;
use super::store::{
    emit_decision_event, live_agent_keys, load_fleet_meta, mutate_fleet_meta, now_ns, now_secs,
    open_fleet_store, proc_pid_starttime, resolve_live_main_sha, verify_agent_pid,
    WatchdogIssueState,
};
use super::tidy::{apply_tidy_decisions, build_tidy_snapshot};
use tacho_daemon::FleetStore;

/// Executes `tacho progress`.
pub fn execute_progress(args: ProgressArgs) -> io::Result<()> {
    let store = open_fleet_store()?;
    let state = store.load()?;
    let now = now_secs();

    let mut agent_rows = Vec::new();
    for a in state.agents.values() {
        if state.retired.contains(&a.key) {
            continue;
        }
        let samples = state.samples.get(&a.key).map(Vec::as_slice).unwrap_or(&[]);
        let notes = state.notes.get(&a.key).map(Vec::as_slice).unwrap_or(&[]);
        agent_rows.push(agent_row(a, samples, notes, now));
    }
    let chains = chain_rows(&state.agents, &state.batches, &state.retired, now);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "agents": agent_rows,
                "chains": chains,
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    println!("=== Agent Progress ===");
    if agent_rows.is_empty() {
        println!("  (no active or recent agents)");
    } else {
        println!(
            "{:<20} {:<12} {:<10} {:<8} {:>8}  NOTE",
            "KEY", "AREA", "VERDICT", "PHASE", "IDLE"
        );
        for r in &agent_rows {
            let phase_str = if r.phase_long {
                format!("{}!", r.phase)
            } else {
                r.phase.clone()
            };
            println!(
                "{:<20} {:<12} {:<10} {:<8} {:>7}s  {}",
                r.key,
                if r.area.is_empty() { "-" } else { &r.area },
                r.verdict,
                phase_str,
                r.idle_secs,
                r.note
            );
        }
    }

    if !chains.is_empty() {
        println!("\n=== Area Chains ===");
        println!(
            "{:<14} {:>6} {:>6} {:>10} {:>10} {:>10} {:>10}",
            "AREA", "1H", "24H", "MED_RUN", "MED_BATCH", "OWNER_W", "MACH_W"
        );
        for c in &chains {
            println!(
                "{:<14} {:>6} {:>6} {:>9}s {:>9}s {:>9}s {:>9}s",
                c.area,
                c.steps_last_hour,
                c.steps_last_day,
                c.median_run_secs,
                c.median_batch_secs,
                c.owner_wait_secs,
                c.machine_wait_secs
            );
        }
    }
    Ok(())
}

/// Collects host `/proc` entries for supervision and tidy checks.
pub fn scan_procs() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let p_dir = entry.path();
        let Ok(stat) = fs::read_to_string(p_dir.join("stat")) else {
            continue;
        };
        let (Some(lparen), Some(rparen)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let comm = stat[lparen + 1..rparen].to_string();
        let fields: Vec<&str> = stat[rparen + 1..].split_whitespace().collect();
        if fields.len() < 20 {
            continue;
        }
        let state = fields[0].to_string();
        let ppid: u32 = fields[1].parse().unwrap_or(0);
        let pgid: u32 = fields[2].parse().unwrap_or(0);
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        let args = fs::read(p_dir.join("cmdline"))
            .map(|b| {
                b.split(|&c| c == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_else(|_| comm.clone());
        let cwd = fs::read_link(p_dir.join("cwd"))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let env_starve = fs::read(p_dir.join("environ"))
            .map(|b| String::from_utf8_lossy(&b).contains("TACHO_STARVE=1"))
            .unwrap_or(false);
        let elapsed_secs = fs::metadata(&p_dir)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        out.push(ProcEntry {
            pid,
            ppid,
            pgid,
            state,
            cpu_ticks: utime + stime,
            elapsed_secs,
            comm,
            args,
            cwd,
            env_starve,
        });
    }
    out
}

fn observe_git_and_deploys(
    cwd: &Path,
    now: i64,
) -> (
    String,
    bool,
    Vec<BehindCommit>,
    BTreeMap<String, String>,
    Vec<DeployRunObs>,
) {
    let main_sha = resolve_live_main_sha(cwd);
    let detached = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(cwd)
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(false);

    let mut behind_commits = Vec::new();
    if let Ok(out) = Command::new("git")
        .args(["log", "--format=%H:%ct", "HEAD..origin/main"])
        .current_dir(cwd)
        .output()
    {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some((sha, ts_str)) = line.trim().split_once(':') {
                    let ts = ts_str.parse::<i64>().unwrap_or(now);
                    behind_commits.push(BehindCommit {
                        sha: sha.to_string(),
                        ts,
                        ci_green: true,
                    });
                }
            }
        }
    }

    let mut failed_main_jobs = BTreeMap::new();
    if let Ok(raw) = std::env::var("TACHO_MAIN_FAILED_JOBS_JSON") {
        if let Ok(parsed) = serde_json::from_str::<BTreeMap<String, String>>(&raw) {
            failed_main_jobs.extend(parsed);
        }
    }

    let mut deploy_runs = Vec::new();
    if let Ok(raw) = std::env::var("TACHO_DEPLOY_RUNS_JSON") {
        if let Ok(parsed) = serde_json::from_str::<Vec<DeployRunObs>>(&raw) {
            deploy_runs.extend(parsed);
        }
    }

    (
        main_sha,
        detached,
        behind_commits,
        failed_main_jobs,
        deploy_runs,
    )
}

/// Kills a duplicate batch PID only when its recorded `starttime` or `cgroup` identity is verified.
pub(crate) fn kill_duplicate_batch(
    pid: u32,
    dup_id: &str,
    starttime: Option<u64>,
    cgroup: Option<&str>,
) -> bool {
    if starttime.filter(|&st| st > 0).is_none() && cgroup.filter(|s| !s.is_empty()).is_none() {
        return false;
    }
    if verify_agent_pid(pid, dup_id, cgroup, starttime) {
        unsafe {
            let _ = libc::kill(pid as i32, libc::SIGTERM);
        }
        true
    } else {
        false
    }
}

/// Executes a single orchestrator & supervisor pass (`tacho tick`) driven entirely by `tacho_core::decide::decide`.
pub fn execute_tick(args: TickArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    execute_tick_with(&mut store, args)
}

/// Executes a single orchestrator & supervisor pass against an open [`FleetStore`].
pub fn execute_tick_with(store: &mut FleetStore, args: TickArgs) -> io::Result<()> {
    let state = store.load()?;
    let now = now_secs();
    let tick_start_ns = now_ns();
    let procs = scan_procs();
    let live = live_agent_keys(&state);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // 1. Gather WorldObservation (agent log tails, progress samples, git/main/deploy state, tidy snapshot)
    let mut agent_log_tails = BTreeMap::new();
    for (k, a) in &state.agents {
        if a.finished_at.is_none() && !live.contains(k) {
            let log_tail = fs::read_to_string(&a.log)
                .map(|s| {
                    let lines: Vec<&str> = s.lines().rev().take(50).collect();
                    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
                })
                .unwrap_or_default();
            agent_log_tails.insert(k.clone(), log_tail);
        }
    }

    let mut agent_samples = BTreeMap::new();
    for key in &live {
        if let Some(a) = state.agents.get(key) {
            let log_bytes = fs::metadata(&a.log).map(|m| m.len()).unwrap_or(0);
            let cpu = a
                .pid
                .map(|pid| tacho_core::decide::watch::tree_cpu(pid, &procs))
                .unwrap_or(0);
            let head_sha = if !a.worktree.is_empty() && Path::new(&a.worktree).is_dir() {
                Command::new("git")
                    .args(["-C", &a.worktree, "rev-parse", "HEAD"])
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let dirty_files = if !a.worktree.is_empty() && Path::new(&a.worktree).is_dir() {
                Command::new("git")
                    .args(["-C", &a.worktree, "status", "--porcelain"])
                    .output()
                    .ok()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .filter(|l| !l.trim().is_empty())
                            .count()
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            agent_samples.insert(
                key.clone(),
                ProgressSample {
                    ts: now,
                    log_bytes,
                    head_sha,
                    dirty_files: dirty_files as u32,
                    cpu_ticks: cpu,
                },
            );
        }
    }

    let (main_sha, detached_checkout, behind_commits, failed_main_jobs, deploy_runs) =
        observe_git_and_deploys(&cwd, now);
    let tidy_snapshot = build_tidy_snapshot(&state, &live, now);
    let mut ready_trains = Vec::new();
    if let Some(open) = open_for(&state.trains) {
        let mut heads = BTreeMap::new();
        for &pr in &open.prs {
            if let Some(p) = state.open_prs_cache.iter().find(|p| p.number == pr) {
                heads.insert(pr, p.head.clone());
            }
        }
        ready_trains.push((open.id.clone(), open.prs.clone(), heads));
    }

    let (updated_flakes, new_flake_tasks) = flake_tasks(
        &state.flakes,
        &state.todo,
        &state.agents,
        &state.retired,
        now,
    );
    let mut initial_todo = state.todo.clone();
    for item in new_flake_tasks {
        if !initial_todo.iter().any(|t| t.key == item.key) {
            initial_todo.push(item);
        }
    }

    let decision_state = DecisionState {
        max_parallel: state.max_workers,
        todo: initial_todo,
        agents: state.agents.clone(),
        retired: state.retired.clone(),
        holds: state.holds.clone(),
        passed: state.passed.clone(),
        failed: state.failed.clone(),
        merged_prs: state.merged_prs.clone(),
        closed_prs: state.closed_prs.clone(),
        batches: state.batches.clone(),
        flakes: updated_flakes,
        samples: state.samples.clone(),
        nudged: state.nudged.clone(),
        rebase_attempts: state.rebase_attempts.clone(),
        fix_notified: state.fix_notified.clone(),
        notified_keys: state.notified_failures.clone(),
        area_rules: state.area_rules.clone(),
        refused_self_update_sha: state.refused_self_update_sha.clone(),
        last_tick_ts: state.last_tick_ts,
        last_watchdog_alert_ts: state.last_watchdog_alert_ts,
        tidy_min_age_secs: 3_600,
        inbox: state.inbox.clone(),
    };

    let mut batch_outcomes = Vec::new();
    let prev_tick_ts = state.last_tick_ts.unwrap_or(0);
    for b in &state.batches {
        if let Some(finished_ts) = b.finished_at {
            if finished_ts > prev_tick_ts && (b.status == "passed" || b.status == "failed") {
                batch_outcomes.push(BatchOutcomeObs {
                    batch_id: b.id.clone(),
                    passed: b.status == "passed",
                    failed_area: b.areas.first().cloned(),
                    log_output: b.log.clone(),
                    tree_sha: b.tree.clone().unwrap_or_default(),
                });
            }
        }
    }

    let mut burner_obs = Vec::new();
    for b in burners(&procs) {
        burner_obs.push(BurnerProcess {
            pid: b.pid,
            comm: b.comm,
            elapsed_secs: b.elapsed_secs,
            starttime: proc_pid_starttime(b.pid),
        });
    }

    let world_obs = WorldObservation {
        now,
        main_sha,
        open_prs: state.open_prs_cache.clone(),
        merged_prs: state.merged_prs.clone(),
        closed_prs: state.closed_prs.clone(),
        live_keys: live.clone(),
        agent_samples,
        agent_log_tails,
        batch_outcomes,
        deploy_runs,
        behind_commits,
        detached_checkout,
        failed_main_jobs,
        ready_trains,
        tidy_snapshot,
        burners: burner_obs,
    };

    // 3. Call the pure decision engine (`decide`) — nothing else decides!
    let plan = decide(&decision_state, &world_obs);

    // Record decision tick inputs and output plan for deterministic replay
    if !args.dry_run {
        store
            .record_decision_tick(now, &decision_state, &world_obs, &plan)
            .map_err(|e| io::Error::other(e.to_string()))?;
    }

    // 4. Execute side-effects prescribed by `plan`
    let dup_batch_ids = duplicate_batches(&state.batches);
    if !dup_batch_ids.is_empty() {
        let meta = load_fleet_meta().unwrap_or_default();
        for dup_id in &dup_batch_ids {
            let because = format!("aborted duplicate batch {dup_id}");
            if !args.dry_run {
                let batch_id_rec = meta.batch_identities.get(dup_id).cloned();
                store.mutate(
                    Some(("watch", "kill_duplicate_batch", dup_id, &because)),
                    |st| {
                        for b in &mut st.batches {
                            if &b.id == dup_id {
                                if let Some(pid) = b.pid {
                                    let (rec_st, rec_cg) = batch_id_rec
                                        .as_ref()
                                        .filter(|id| id.pid == pid)
                                        .map(|id| (id.starttime, id.cgroup.as_deref()))
                                        .unwrap_or((None, None));
                                    let _ = kill_duplicate_batch(pid, dup_id, rec_st, rec_cg);
                                }
                                b.status = "aborted".to_string();
                                b.finished_at = Some(now);
                            }
                        }
                    },
                )?;
            }
            println!("{because}");
        }
    }

    for b in &plan.burner_kills {
        let because = format!(
            "killed runaway burner pid={} ({}) elapsed={}s",
            b.pid, b.comm, b.elapsed_secs
        );
        if !args.dry_run {
            let actual_st = proc_pid_starttime(b.pid);
            if b.starttime.filter(|&st| st > 0).is_some() && actual_st == b.starttime {
                unsafe {
                    let _ = libc::kill(b.pid as i32, libc::SIGKILL);
                }
                store.record_decision("watch", "kill_burner", &b.pid.to_string(), &because)?;
                emit_decision_event("system", "watch:kill_burner", &because);
            } else {
                eprintln!(
                    "[tacho-safety] burner PID {} starttime mismatch; refusing kill",
                    b.pid
                );
            }
        }
        println!("{because}");
    }

    for (agent_key, msg) in &plan.tell_actions {
        if !args.dry_run {
            deliver_tell_action(store, agent_key, msg)?;
        }
    }

    if !plan.reaped_agents.is_empty() && !args.dry_run {
        store.mutate(
            Some((
                "dispatch",
                "reap_agents",
                "agents",
                "reaped exited agents and attributed PRs",
            )),
            |st| {
                st.agents = plan.next_state.agents.clone();
                let wt_clean_map = HashMap::new();
                let idle = idle_chains(&st.agents, &st.retired, &live, &wt_clean_map);
                for root in idle {
                    if let Some(a) = st.agents.get_mut(&root) {
                        a.pr_none = true;
                    }
                }
            },
        )?;
        for (k, found_pr, is_none) in &plan.reaped_agents {
            let (label, status, because) = if let Some(p) = found_pr {
                ("Review", TaskStatus::Review, format!("{k} opened PR #{p}"))
            } else if *is_none {
                (
                    "Done",
                    TaskStatus::Done,
                    format!("{k} completed with PR: none"),
                )
            } else {
                (
                    "Failed",
                    TaskStatus::Failed,
                    format!("{k} exited without PR"),
                )
            };
            store.emit_state_band(
                &format!("task:{}/state", base_key(k)),
                label,
                status,
                &because,
                now_ns(),
            )?;
        }
    }

    for step in &plan.supervision_actions {
        match step {
            SuperviseAction::Ok => {}
            SuperviseAction::Resume { key, because } => {
                println!("Supervise: {because}");
                if !args.dry_run {
                    if let Some(a) = state.agents.get(key) {
                        if let Some(pid) = a.pid {
                            if verify_agent_pid(pid, key, a.cgroup.as_deref(), a.starttime) {
                                unsafe {
                                    let _ = libc::kill(-(pid as i32), libc::SIGTERM);
                                }
                            }
                        }
                        let resumed = resume_task(a, because, "", now);
                        store.mutate(Some(("watch", "supervise_resume", key, because)), |st| {
                            if let Some(ag) = st.agents.get_mut(key) {
                                ag.finished_at = Some(now);
                                ag.exit_code = Some(143);
                            }
                            st.todo.push(resumed);
                        })?;
                    }
                    emit_decision_event(&format!("agent:{key}"), "watch:supervise", because);
                }
            }
            SuperviseAction::Nudge { key, because } => {
                println!("Supervise: {because}");
                if !args.dry_run {
                    store.mutate(Some(("watch", "supervise_nudge", key, because)), |st| {
                        st.nudged.insert(key.clone());
                    })?;
                    emit_decision_event(&format!("agent:{key}"), "watch:supervise", because);
                }
            }
            SuperviseAction::Escalate { key, because } => {
                println!("Supervise: {because}");
                if !args.dry_run {
                    store.record_decision("watch", "supervise", key, because)?;
                    emit_decision_event(&format!("agent:{key}"), "watch:supervise", because);
                }
            }
        }
    }

    for item in &plan.resumed_tasks {
        let because = format!("auto-queued resume {} after backoff", item.key);
        println!("{because}");
        if !args.dry_run {
            store.record_decision("dispatch", "auto_resume", &item.key, &because)?;
        }
    }

    if !args.dry_run {
        let mut next_todo = plan.next_state.todo.clone();
        for (pr, rec) in &plan.fix_decisions.dispatch {
            let fix_key = format!("fix-pr{pr}");
            if !next_todo.iter().any(|t| t.key == fix_key) {
                next_todo.push(TodoItem {
                    key: fix_key,
                    brief: format!(
                        "Fix red PR #{pr} at {} ({}):\n{}",
                        rec.sha, rec.summary, rec.log
                    ),
                    area: String::new(),
                    base: Some(pr.to_string()),
                    after: Vec::new(),
                    priority: 1,
                    created_at: now,
                    ..Default::default()
                });
            }
        }
        for (pr_num, head_sha) in &plan.rebase_handoffs {
            let reb_key = format!("rebase-pr{pr_num}");
            if !next_todo.iter().any(|t| t.key == reb_key) {
                next_todo.push(TodoItem {
                    key: reb_key,
                    brief: format!("Rebase conflicting PR #{pr_num} at {head_sha} onto main"),
                    area: String::new(),
                    base: Some(pr_num.to_string()),
                    after: Vec::new(),
                    priority: 1,
                    created_at: now,
                    ..Default::default()
                });
            }
        }

        if !plan.fix_decisions.dispatch.is_empty() || !plan.fix_decisions.notify.is_empty() {
            store.record_decision(
                "verify",
                "decide_fixes",
                "failed_prs",
                "processed red PR fix actions",
            )?;
        }
        if !plan.rebase_handoffs.is_empty() {
            store.record_decision(
                "dispatch",
                "next_hands",
                "open_prs",
                "processed conflict rebase actions",
            )?;
        }

        // Persist updated state from `decide` (keeping `dispatched_tasks` in `st.todo` until `spawn_worker_for_item` pops them)
        let mut todo_with_dispatched = plan.dispatched_tasks.clone();
        todo_with_dispatched.extend(next_todo);
        store.mutate(None, |st| {
            st.todo = todo_with_dispatched;
            st.samples = plan.next_state.samples.clone();
            st.nudged = plan.next_state.nudged.clone();
            st.flakes = plan.next_state.flakes.clone();
            st.rebase_attempts = plan.next_state.rebase_attempts.clone();
            st.fix_notified = plan.next_state.fix_notified.clone();
            st.notified_failures = plan.next_state.notified_keys.clone();
            st.last_tick_ts = plan.next_state.last_tick_ts;
            st.last_watchdog_alert_ts = plan.next_state.last_watchdog_alert_ts;
        })?;
    }

    for item in &plan.dispatched_tasks {
        if args.dry_run {
            println!("[dry-run] Would dispatch {}", item.key);
        } else {
            let pid = spawn_worker_for_item(store, item)?;
            println!("Dispatched {} (pid {pid})", item.key);
        }
    }

    // 5. Run automatic janitor (`tidy_decisions`), self-update, and owner notifications from `decide`
    if !plan.tidy_decisions.is_empty() {
        apply_tidy_decisions(store, &plan.tidy_decisions, now, args.dry_run)?;
    }

    if let Some(ref target_sha) = plan.self_update.target_sha {
        println!("Self-update: {}", plan.self_update.because);
        if !args.dry_run {
            store.record_decision(
                "watch",
                "self_update",
                target_sha,
                &plan.self_update.because,
            )?;
        }
    }

    if !args.dry_run {
        for n in &plan.owner_notifications {
            store.record_decision("watch", "notify_owner", &n.key, &n.because)?;
        }
        if let Some(ref alert) = plan.watchdog_alert {
            store.record_decision("watch", "watchdog_alert", "tick", alert)?;
        }
    }

    let tick_summary = format!(
        "active={}, dispatched={}, reaped={}",
        live.len(),
        plan.dispatched_tasks.len(),
        plan.reaped_agents.len()
    );

    if !args.dry_run {
        store.emit_duration_slice(
            "fleet/ticks",
            "tick",
            tick_start_ns,
            now_ns(),
            &tick_summary,
        )?;
        // Write atomic scrubbed snapshot.json via .tmp + rename for fast dashboard reads
        let st = store.load()?;
        let home = crate::paths::resolve_home();
        let val = serde_json::to_value(&st).map_err(io::Error::other)?;
        write_scrubbed_snapshot(&home.join("snapshot.json"), &val)?;
    }

    println!("Tick complete at {} ({})", now, tick_summary);
    Ok(())
}

/// Lifecycle action decided by the watchdog for the `"tacho is down"` issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogStep {
    /// Open a new `"tacho is down"` issue.
    Open { title: String, body: String },
    /// Comment on the existing open `"tacho is down"` issue after cooldown.
    Update { number: u64, body: String },
    /// Comment recovery and close the open `"tacho is down"` issue.
    Close { number: u64, body: String },
    /// Nothing to do (healthy with no open issue, or within comment cooldown).
    None,
}

/// Evaluates and transitions the watchdog `"tacho is down"` issue lifecycle (`OPEN`, `UPDATE`, `CLOSE`, `NONE`).
pub fn run_watchdog_lifecycle(
    reason: Option<&str>,
    open_issue: Option<WatchdogIssueState>,
    now: i64,
    cooldown_secs: i64,
) -> (WatchdogStep, Option<WatchdogIssueState>) {
    let scrubber = Scrubber::new();
    match (reason, open_issue) {
        (Some(why), None) => {
            let clean = scrubber.scrub(why);
            (
                WatchdogStep::Open {
                    title: "tacho is down".to_string(),
                    body: format!("Watchdog detected stale orchestrator tick: {clean}"),
                },
                Some(WatchdogIssueState {
                    number: 1,
                    last_comment_at: now,
                }),
            )
        }
        (Some(why), Some(mut issue)) => {
            if now.saturating_sub(issue.last_comment_at) >= cooldown_secs {
                let clean = scrubber.scrub(why);
                issue.last_comment_at = now;
                (
                    WatchdogStep::Update {
                        number: issue.number,
                        body: format!("Still down at {now}: {clean}"),
                    },
                    Some(issue),
                )
            } else {
                (WatchdogStep::None, Some(issue))
            }
        }
        (None, Some(issue)) => (
            WatchdogStep::Close {
                number: issue.number,
                body: format!("Orchestrator recovered at {now}; closing watchdog issue."),
            },
            None,
        ),
        (None, None) => (WatchdogStep::None, None),
    }
}

/// Executes `tacho watchdog` (checks whether the orchestrator tick is stale while work is pending).
pub fn execute_watchdog(args: WatchdogArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let now = now_secs();
    let live = live_agent_keys(&state);
    let has_active_work = !live.is_empty() || !state.todo.is_empty();
    let max_stale = args.max_stale.unwrap_or(600);
    let cooldown = args.cooldown.unwrap_or(3_600);
    let reason = down_reason(state.last_tick_ts, now, max_stale, has_active_work);

    let meta = load_fleet_meta().unwrap_or_default();
    let (step, next_issue) =
        run_watchdog_lifecycle(reason.as_deref(), meta.watchdog_issue, now, cooldown);
    mutate_fleet_meta(|m| {
        m.watchdog_issue = next_issue;
    })?;
    if let WatchdogStep::Close { number, ref body } = step {
        store.record_decision("watch", "watchdog_close", &number.to_string(), body)?;
    }

    let alert = down_alert(
        reason.as_deref(),
        state.last_watchdog_alert_ts,
        now,
        cooldown,
    );
    if let Some(msg) = alert {
        store.mutate(
            Some(("watch", "watchdog_alert", "orchestrator", &msg)),
            |st| {
                st.last_watchdog_alert_ts = Some(now);
            },
        )?;
        eprintln!("WATCHDOG ALERT: {msg}");
        return Err(io::Error::other(msg));
    }
    println!("Watchdog OK.");
    Ok(())
}
