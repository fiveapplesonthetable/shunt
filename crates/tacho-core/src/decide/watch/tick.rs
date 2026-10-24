//! Pure watch evaluation loop (`decide_watch`, `decide_owner_mail`, `resume_ready_agents`, `next_deadline`).
//!
//! Requirements R18–R26, R39–R48.

use std::collections::BTreeSet;

use super::activity::Activity;
use super::chains::{
    attempt, base_key, chain_rows, hourly_health_line, latest, recorded_pr, rows, run_key,
    run_state, table, RunRecord, RunStatus,
};
use super::config::WatchConfig;
use super::procs::{
    burners, duplicate_batches, inside, leads_group, next_burner_due, owner, tree_cpu, BatchObs,
    Proc, ProcId,
};
use super::sample::{take_sample, Sample};
use super::supervise::{
    burner_notice, idle_chains, needs_resume, nudged_before, resolve_worktree, resume_brief,
    stalled_sentence, step, Step, WatchAction, WatchObs, WatchPlan, WatchState,
};
use super::verdict::{agent_next_deadline, verdict};

/// Pure watch decision engine:
/// 1. Judges every live `running` agent (`watch_agents`, R18–R26).
/// 2. Selects and stops rogue CPU burners that are ours (`stop_burners`, R39–R43).
/// 3. Runs the guarded hourly pass when `obs.run_hourly` is true (`hourly`, R44–R48).
pub fn decide_watch(st: &WatchState, obs: &WatchObs, c: &WatchConfig) -> WatchPlan {
    let mut next = st.clone();
    let mut actions = Vec::new();
    let mut discovered_wts = Vec::new();

    // 1. Judge running agents whose pid is alive (R20).
    let agent_keys: Vec<String> = next.agents.keys().cloned().collect();
    for key in agent_keys {
        let agent = &next.agents[&key];
        let Some(pid) = agent.pid else { continue };
        if agent.status != RunStatus::Running || !obs.live_pids.contains(&pid) {
            continue;
        }
        let wt = resolve_worktree(&next, &key, agent);
        if !wt.is_empty() && agent.worktree.is_empty() {
            discovered_wts.push((key.clone(), wt.clone()));
        }
        let cpu = tree_cpu(&obs.procs, pid);
        let prev = next.samples.get(&key);
        let sample = take_sample(prev, cpu, obs.now, c);
        next.samples.insert(key.clone(), sample);

        let act = obs
            .activities
            .get(&wt)
            .or_else(|| obs.activities.get(&key))
            .copied()
            .unwrap_or_else(Activity::quiet);
        let nudged = nudged_before(&key, &next.agents);
        let v = verdict(&act, &sample, agent.started, obs.now, c);
        let s = step(v, nudged);

        let stall_key = format!("stalled-{key}");
        if next.needs_person.remove(&stall_key).is_some() {
            actions.push(WatchAction::ClearNeed {
                key: key.clone(),
                need_key: stall_key.clone(),
                because: format!("cleared stale need for {key} before re-judging"),
            });
        }

        match s {
            Step::None => {}
            Step::Email => {
                let task_notes = next
                    .notes
                    .get(base_key(&key))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let sentence = stalled_sentence(&key, &wt, task_notes, c);
                next.needs_person
                    .insert(stall_key.clone(), sentence.clone());
                actions.push(WatchAction::Escalate {
                    key: key.clone(),
                    need_key: stall_key,
                    need: sentence,
                    because: format!(
                        "{key} remained STALLED after already being nudged; escalating to owner"
                    ),
                });
            }
            Step::Nudge | Step::Resume => {
                let proc_id = obs
                    .procs
                    .iter()
                    .find(|p| p.pid() == pid)
                    .map(|p| p.id)
                    .unwrap_or(ProcId {
                        pid,
                        start_ticks: 0,
                    });
                if leads_group(&obs.procs, proc_id) {
                    let is_nudge = s == Step::Nudge;
                    let stop_reason = if is_nudge {
                        "stopped: nothing written for 45 minutes with the CPU idle".to_string()
                    } else {
                        "stopped: no file change and no CPU for 30 minutes".to_string()
                    };
                    let tell_line = if is_nudge {
                        format!("{key} stalled for 45 minutes; nudged and resuming in its worktree")
                    } else {
                        format!("{key} hung for 30 minutes; stopped, to be resumed in its worktree")
                    };
                    if let Some(a) = next.agents.get_mut(&key) {
                        a.status = RunStatus::Unfinished;
                        a.finished = Some(obs.now);
                        a.nudge = is_nudge;
                        a.stop_reason = stop_reason.clone();
                    }
                    actions.push(WatchAction::StopAndResume {
                        key: key.clone(),
                        id: proc_id,
                        backoff: !is_nudge,
                        question: is_nudge,
                        stop_reason: stop_reason.clone(),
                        tell_line,
                        because: stop_reason,
                    });
                }
            }
        }
    }

    for (key, wt) in discovered_wts {
        if let Some(a) = next.agents.get_mut(&key) {
            if a.worktree.is_empty() {
                a.worktree = wt;
            }
        }
    }

    // 2. Stop rogue burners past the time box that belong to us (R39–R43).
    let running_roots: BTreeSet<u32> = next
        .agents
        .values()
        .filter(|a| a.status == RunStatus::Running)
        .filter_map(|a| a.pid)
        .collect();
    let running_pairs: Vec<(String, u32)> = next
        .agents
        .iter()
        .filter(|(_, a)| a.status == RunStatus::Running)
        .filter_map(|(k, a)| a.pid.map(|pid| (k.clone(), pid)))
        .collect();
    let known_wts: BTreeSet<String> = next
        .worktrees
        .values()
        .chain(next.agents.values().map(|a| &a.worktree))
        .filter(|p| !p.is_empty())
        .cloned()
        .collect();

    for b in burners(&obs.procs, &running_roots, &obs.work_root, c) {
        let owner_key = owner(b, &obs.procs, &running_pairs);
        let who = match &owner_key {
            Some(k) => format!("agent {k}"),
            None => format!("no agent {} knows", c.tool),
        };
        let cg = if b.cgroup.is_empty() {
            "no cgroup"
        } else {
            &b.cgroup
        };
        let mins = b.age.div_euclid(60);
        let tell_line = format!(
            "stopped a {} burner (pid {}, {cg}) owned by {who} that ran {mins} minutes in {}; `{} starve` is the way to starve a test",
            b.name,
            b.pid(),
            b.cwd,
            c.tool
        );
        let worktree_notice = known_wts
            .iter()
            .find(|wt| inside(&b.cwd, wt))
            .map(|wt| (wt.clone(), burner_notice(1, c)));
        let pr_comment = owner_key
            .as_deref()
            .and_then(|k| recorded_pr(base_key(k), &next.agents).map(|pr| (k, pr.number)))
            .map(|(k, num)| {
                (
                    num,
                    format!(
                        "{} stopped a `{}` burner that {k} left running for {mins} minutes outside `{} starve`. A starvation test runs under `{} starve -- <command>`.",
                        c.tool, b.name, c.tool, c.tool
                    ),
                )
            });
        actions.push(WatchAction::KillBurner {
            id: b.id,
            name: b.name.clone(),
            owner: owner_key,
            worktree_notice,
            tell_line: tell_line.clone(),
            pr_comment,
            because: tell_line,
        });
    }

    // 3. Guarded hourly pass (R44–R48).
    let mut hourly_lines = Vec::new();
    if obs.run_hourly {
        if let Some(err) = obs.step_faults.get("duplicate batches") {
            let line = format!("supervision: duplicate batches failed: {err}");
            hourly_lines.push(line.clone());
            actions.push(WatchAction::Report {
                line: line.clone(),
                because: line,
            });
        } else {
            let alive_pids: BTreeSet<u32> = obs.procs.iter().map(Proc::pid).collect();
            let batch_vec: Vec<BatchObs> = next.batches.values().cloned().collect();
            for (bid, pid) in duplicate_batches(&batch_vec, &alive_pids) {
                if let Some(b) = next.batches.get_mut(&bid) {
                    b.running = false;
                }
                if let Some(cb) = next.chain_batches.get_mut(&bid) {
                    cb.status = "superseded".to_string();
                }
                let proc_id = obs
                    .procs
                    .iter()
                    .find(|p| p.pid() == pid)
                    .map(|p| p.id)
                    .unwrap_or(ProcId {
                        pid,
                        start_ticks: 0,
                    });
                let stop_group = leads_group(&obs.procs, proc_id);
                let tell_line = stop_group
                    .then(|| format!("batch {bid} stopped: an older batch checks the same PRs"));
                actions.push(WatchAction::SupersedeBatch {
                    batch: bid.clone(),
                    id: proc_id,
                    stop_group,
                    tell_line,
                    because: format!(
                        "batch {bid} superseded by an older running batch over the same PRs"
                    ),
                });
            }
        }

        if let Some(err) = obs.step_faults.get("idle chains") {
            let line = format!("supervision: idle chains failed: {err}");
            hourly_lines.push(line.clone());
            actions.push(WatchAction::Report {
                line: line.clone(),
                because: line,
            });
        } else {
            let running_count = next
                .agents
                .values()
                .filter(|a| a.status == RunStatus::Running)
                .count();
            let idles = idle_chains(
                &next.todo,
                &next.agents,
                &obs.standing,
                &next.holds,
                running_count,
                c.agent_slots,
                obs.busy,
                &next.area_heads,
                c,
            );
            for idle in idles {
                let mut lifted = false;
                if !idle.no_pr_of.is_empty() {
                    if let Some((latest_key, rec)) = latest(&idle.no_pr_of, &next.agents) {
                        let wt = resolve_worktree(&next, &latest_key, rec);
                        if !wt.is_empty() && obs.activities.get(&wt).is_some_and(Activity::is_empty)
                        {
                            if let Some(mut_rec) = next.agents.get_mut(&latest_key) {
                                mut_rec.no_change = true;
                            }
                            let tell_line = format!(
                                "chain {} was held by {} stating `PR: none`; its worktree holds nothing, so the chain goes on",
                                idle.area, idle.no_pr_of
                            );
                            actions.push(WatchAction::LiftPrNone {
                                key: idle.no_pr_of.clone(),
                                area: idle.area.clone(),
                                tell_line: tell_line.clone(),
                                because: tell_line,
                            });
                            lifted = true;
                        }
                    }
                }
                if !lifted {
                    let line = format!("chain {} is idle at {}: {}", idle.area, idle.key, idle.why);
                    hourly_lines.push(line.clone());
                    actions.push(WatchAction::Report {
                        line: line.clone(),
                        because: line,
                    });
                }
            }
        }

        if let Some(err) = obs.step_faults.get("green batches") {
            let line = format!("supervision: green batches failed: {err}");
            hourly_lines.push(line.clone());
            actions.push(WatchAction::Report {
                line: line.clone(),
                because: line,
            });
        } else {
            for (pr_num, title) in &next.unmailed_green_prs {
                let dedup = format!("green-pr:{pr_num}");
                if next.told_keys.insert(dedup.clone()) {
                    actions.push(WatchAction::MailOwner {
                        dedup_key: dedup,
                        subject: format!("[{}] #{pr_num} ready: {title}", c.tool),
                        body: format!(
                            "PR #{pr_num} ({title}) passed its batch and is ready to land."
                        ),
                        because: format!("green PR #{pr_num} was never emailed"),
                    });
                }
            }
        }

        if let Some(err) = obs.step_faults.get("health") {
            let line = format!("supervision: health failed: {err}");
            hourly_lines.push(line.clone());
            actions.push(WatchAction::Report {
                line: line.clone(),
                because: line,
            });
        } else {
            let a_rows = rows(
                &next.agents,
                &next.samples,
                &next.notes,
                &obs.activities,
                obs.now,
                c,
            );
            let c_rows = chain_rows(
                &next.todo,
                &next.agents,
                &next.chain_batches,
                &next.passed_batch_of_pr,
                &next.merged_at,
                &obs.standing,
                obs.now,
            );
            let running_count = next
                .agents
                .values()
                .filter(|a| a.status == RunStatus::Running)
                .count();
            let mut seen_bases = BTreeSet::new();
            let mut stuck_count = 0;
            for k in next.agents.keys() {
                let b = base_key(k);
                if seen_bases.insert(b.to_string()) && run_state(b, &next.agents, c) == "stuck" {
                    stuck_count += 1;
                }
            }
            let h_line = hourly_health_line(
                obs.cpu_percent,
                obs.load,
                obs.free_gb,
                running_count,
                stuck_count,
                &a_rows,
                &c_rows,
                c,
            );
            hourly_lines.push(h_line.clone());
            actions.push(WatchAction::Report {
                line: h_line,
                because: "hourly supervision health summary".to_string(),
            });
        }

        next.supervised = Some(obs.now);
    }

    WatchPlan {
        actions,
        next_state: next,
        hourly_lines,
    }
}

/// Emit tell-once owner emails for any `needs_person` entries and optional due daily digest (parity cases 32, 59).
pub fn decide_owner_mail(
    st: &mut WatchState,
    obs: &WatchObs,
    send_digest_if_due: bool,
    c: &WatchConfig,
) -> Vec<WatchAction> {
    let mut out = Vec::new();
    for (need_key, body) in &st.needs_person {
        let dedup = format!("need:{need_key}:{body}");
        if st.told_keys.insert(dedup.clone()) {
            out.push(WatchAction::MailOwner {
                dedup_key: dedup,
                subject: format!("[{}] {need_key} needs attention", c.tool),
                body: body.clone(),
                because: format!("{need_key} requires human intervention"),
            });
        }
    }
    if send_digest_if_due && !st.digest_entries.is_empty() {
        let due_digest = st
            .digest_sent
            .is_none_or(|last| obs.now - last >= c.row_window);
        if due_digest {
            let a_rows = rows(
                &st.agents,
                &st.samples,
                &st.notes,
                &obs.activities,
                obs.now,
                c,
            );
            let c_rows = chain_rows(
                &st.todo,
                &st.agents,
                &st.chain_batches,
                &st.passed_batch_of_pr,
                &st.merged_at,
                &obs.standing,
                obs.now,
            );
            let prog = table(&a_rows, &c_rows);
            let events_text: Vec<String> = st
                .digest_entries
                .iter()
                .map(|(ts, msg)| format!("{ts}  {msg}"))
                .collect();
            let body = format!("{}\n\nPROGRESS\n{prog}", events_text.join("\n"));
            st.digest_sent = Some(obs.now);
            st.digest_entries.clear();
            out.push(WatchAction::MailOwner {
                dedup_key: format!("digest:{}", obs.now),
                subject: format!("[{}] daily digest", c.tool),
                body,
                because: "daily digest is due".to_string(),
            });
        }
    }
    out
}

/// Resume any unfinished runs in `st.agents` whose backoff has elapsed (or immediately if `nudge == true`).
pub fn resume_ready_agents(
    st: &mut WatchState,
    now: i64,
    c: &WatchConfig,
) -> Vec<(String, RunRecord)> {
    let mut bases = Vec::new();
    let mut seen = BTreeSet::new();
    for k in st.agents.keys() {
        let b = base_key(k).to_string();
        if seen.insert(b.clone()) {
            bases.push(b);
        }
    }
    let mut resumed = Vec::new();
    for base in bases {
        let Some((last_key, last_rec)) = latest(&base, &st.agents) else {
            continue;
        };
        if !needs_resume(last_rec, &st.agents, &last_key, now, c) {
            continue;
        }
        let next_attempt = attempt(&last_key) + 1;
        let next_key = run_key(&base, next_attempt);
        let wt = resolve_worktree(st, &last_key, last_rec);
        let brief = resume_brief(&last_rec.task, &wt, &[], &[], last_rec.nudge, c);
        let next_rec = RunRecord {
            status: RunStatus::Running,
            started: now,
            finished: None,
            pid: Some(last_rec.pid.unwrap_or(100) + next_attempt),
            worktree: wt,
            base_sha: last_rec.base_sha.clone(),
            task: brief,
            model: last_rec.model.clone(),
            pr: None,
            pr_checked: false,
            stated_no_pr: false,
            no_change: false,
            nudge: false,
            stop_reason: String::new(),
            summary: String::new(),
        };
        st.agents.insert(next_key.clone(), next_rec.clone());
        resumed.push((next_key, next_rec));
    }
    resumed
}

/// Earliest future deadline (`> obs.now`) at which an agent will cross `hung_after`/`stall_after`,
/// a young burner will cross `burner_limit`, a stopped hung run will finish its resume backoff,
/// or the hourly pass will become due.
pub fn next_deadline(st: &WatchState, obs: &WatchObs, c: &WatchConfig) -> Option<i64> {
    let mut deadlines = Vec::new();
    for (key, agent) in &st.agents {
        if agent.status == RunStatus::Running {
            if agent.pid.is_some_and(|pid| obs.live_pids.contains(&pid)) {
                let wt = resolve_worktree(st, key, agent);
                let act = obs
                    .activities
                    .get(&wt)
                    .or_else(|| obs.activities.get(key))
                    .copied()
                    .unwrap_or_else(Activity::quiet);
                let sample = st
                    .samples
                    .get(key)
                    .copied()
                    .unwrap_or_else(|| Sample::first(0.0, obs.now));
                if let Some(d) = agent_next_deadline(&act, &sample, agent.started, obs.now, c) {
                    deadlines.push(d);
                }
            }
        } else if agent.status == RunStatus::Unfinished && !agent.nudge {
            if let Some(fin) = agent.finished {
                let d = fin + c.resume_backoff;
                if d > obs.now {
                    deadlines.push(d);
                }
            }
        }
    }
    let running_roots: BTreeSet<u32> = st
        .agents
        .values()
        .filter(|a| a.status == RunStatus::Running)
        .filter_map(|a| a.pid)
        .collect();
    if let Some(b_due) = next_burner_due(&obs.procs, &running_roots, &obs.work_root, obs.now, c) {
        if b_due > obs.now {
            deadlines.push(b_due);
        }
    }
    if let Some(last_sup) = st.supervised {
        let sup_due = last_sup + c.report_interval;
        if sup_due > obs.now {
            deadlines.push(sup_due);
        }
    }
    deadlines.into_iter().min()
}
