//! Fleet orchestrator, work queue, batch verification, merge trains, and machine care commands.
//!
//! All decisions are computed purely via `tacho_core::decide::{dispatch, verify, land, watch}`
//! and persisted with their `because` reason into `$TACHO_HOME/events.sqlite` via [`FleetStore`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tacho_core::decide::dispatch::{
    area_report, base_key, compare_items, find_pr, hold_problem, may_start, needs_resume,
    next_hands, next_work, release_problem, resolve, resume_task, retire, retire_problem,
    with_hold, without_hold, AgentRun, TodoItem,
};
use tacho_core::decide::land::{
    drop_breakers, eligible, finished, make_train, mismatch, open_for, order_train_prs, queue,
    settle, train_refusal, waiting_for, with_train, TrainPrState,
};
use tacho_core::decide::verify::{
    batches, classify, counterexample, decide_fixes, failed_tests, flake_rows, flake_tasks,
    new_batch_id, next_probe, parents, record_flakes, reruns_on_red, reusable_batch, slug_problem,
    summary_line, BatchRun, Bisect, FailedRecord, PassedRecord, ProbeStep,
};
use tacho_core::decide::watch::{
    agent_row, burners, chain_rows, decide_tidy, down_alert, down_reason, duplicate_batches,
    idle_chains, needs_you, stale_runs, supervise_step, take_sample, verdict, zombie_lines,
    ProcEntry, ProgressSample, SuperviseAction, TidyDecision, TidySnapshot, WorktreeCandidate,
};
use tacho_core::TaskStatus;
use tacho_daemon::{FleetState, FleetStore};

use crate::cli::{
    AttachArgs, BatchArgs, EventsArgs, FlakesArgs, HoldArgs, LandArgs, NextArgs, ProgressArgs,
    QueueArgs, ReleaseArgs, ReleaseTrainArgs, ReplayArgs, RetireArgs, ServeArgs, SessionArgs,
    StopArgs, TellArgs, TickArgs, TidyArgs, TodoArgs, TopArgs, TrainArgs, WatchdogArgs,
};
use crate::client::DaemonClient;

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

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc != 0 {
        return false;
    }
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        if let Some(rparen) = stat.rfind(')') {
            if stat[rparen + 1..].trim_start().starts_with('Z') {
                return false;
            }
        }
    }
    true
}

fn live_agent_keys(state: &FleetState) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for a in state.agents.values() {
        if a.finished_at.is_none() {
            if let Some(pid) = a.pid {
                if pid_alive(pid) {
                    out.insert(a.key.clone());
                }
            }
        }
    }
    out
}

/// Executes `tacho todo [<key> "<brief>" ...]`.
pub fn execute_todo(args: TodoArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;

    let Some(key) = args.key else {
        // List current queue
        let live = live_agent_keys(&state);
        let passed_prs: BTreeSet<u64> = state.passed.keys().copied().collect();
        let failed_prs: BTreeSet<u64> = state.failed.keys().copied().collect();
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&state.todo).unwrap_or_default()
            );
            return Ok(());
        }
        if state.todo.is_empty() {
            println!("Todo queue is empty.");
            return Ok(());
        }
        println!(
            "{:<22} {:<12} {:<8} {:<10} {:<24} BRIEF",
            "KEY", "AREA", "PRIO", "PROVIDER", "STATUS"
        );
        for item in &state.todo {
            let (ok, reason) = may_start(
                item,
                &state.agents,
                &state.retired,
                &state.holds,
                &live,
                &passed_prs,
                &failed_prs,
                &state.merged_prs,
                &state.closed_prs,
            );
            let status_str = if ok {
                format!("ready ({reason})")
            } else {
                format!("waiting ({reason})")
            };
            let prov = item
                .compare
                .as_deref()
                .or(item.model.as_deref())
                .or(item.provider.as_deref())
                .unwrap_or(&state.default_provider);
            let brief_short: String = item
                .brief
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(48)
                .collect();
            println!(
                "{:<22} {:<12} {:<8} {:<10} {:<24} {}",
                item.key,
                if item.area.is_empty() {
                    "-"
                } else {
                    &item.area
                },
                item.priority,
                prov,
                status_str.chars().take(24).collect::<String>(),
                brief_short
            );
        }
        return Ok(());
    };

    if let Some(prob) = slug_problem(&key) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }

    let brief = match (args.brief, args.brief_file) {
        (Some(b), _) if !b.trim().is_empty() => b,
        (_, Some(path)) => fs::read_to_string(&path)?,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provide a brief string or --brief-file <path>",
            ));
        }
    };

    let after: Vec<String> = args
        .after
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    // Validate provider/model/compare before queueing
    if args.compare.is_none() {
        resolve(
            args.provider.as_deref(),
            args.model.as_deref(),
            args.effort.as_deref(),
            &state.default_provider,
            state.default_model.as_deref(),
            state.default_effort.as_deref(),
        )
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }

    let raw_item = TodoItem {
        key: key.clone(),
        brief,
        area: args.area.unwrap_or_default(),
        base: args.base,
        after,
        priority: if args.priority { 1 } else { 0 },
        created_at: now_secs(),
        provider: args.provider,
        model: args.model,
        effort: args.effort,
        compare: args.compare,
    };

    let items = compare_items(&raw_item, state.default_effort.as_deref())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let added_keys: Vec<String> = items.iter().map(|i| i.key.clone()).collect();
    let because = format!("queued {:?}", added_keys);
    store.mutate(Some(("dispatch", "todo", &key, &because)), |st| {
        for item in items {
            st.retired.remove(&item.key);
            st.retired.remove(base_key(&item.key));
            st.todo.retain(|t| t.key != item.key);
            st.todo.push(item);
        }
    })?;

    let ts = now_ns();
    for k in &added_keys {
        let _ = store.emit_state_band(
            &format!("task:{k}/state"),
            "Queued",
            TaskStatus::Queued,
            &because,
            ts,
        );
    }
    emit_decision_event(&format!("task:{key}"), "dispatch:todo", &because);
    println!("Queued: {}", added_keys.join(", "));
    Ok(())
}

/// Spawns a provider agent process for a `TodoItem` in its worktree.
pub fn spawn_worker_for_item(store: &mut FleetStore, item: &TodoItem) -> io::Result<u32> {
    let state = store.load()?;
    let choice = resolve(
        item.provider.as_deref(),
        item.model.as_deref(),
        item.effort.as_deref(),
        &state.default_provider,
        state.default_model.as_deref(),
        state.default_effort.as_deref(),
    )
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let home = crate::paths::resolve_home();
    let logs_dir = home.join("logs");
    fs::create_dir_all(&logs_dir)?;
    let log_path = logs_dir.join(format!("{}.log", item.key));

    let cwd = std::env::current_dir()?;
    let root_key = base_key(&item.key);
    let wt_path = state
        .worktrees
        .get(&item.key)
        .or_else(|| state.worktrees.get(root_key))
        .map(|w| PathBuf::from(&w.path))
        .unwrap_or_else(|| cwd.clone());

    let log_file = File::create(&log_path)?;
    let log_err = log_file.try_clone()?;

    let prompt = format!(
        "Task key: {}\nArea: {}\nBase: {}\n\n{}\n\nWhen finished, run `tacho prove` and `tacho pr`, and end your final message with `PR: #<n>` (or `PR: none`).",
        item.key,
        if item.area.is_empty() { "unscoped" } else { &item.area },
        item.base.as_deref().unwrap_or("main"),
        item.brief
    );

    let cmd_str = if let Ok(custom) = std::env::var("TACHO_AGENT_CMD") {
        custom
    } else {
        match choice.provider.as_str() {
            "codex" => {
                let m = choice.model.as_deref().unwrap_or("gpt-5.4");
                format!("codex exec --model {m} --full-auto")
            }
            "gemini" => {
                let m = choice.model.as_deref().unwrap_or("gemini-3.1-pro-preview");
                format!("gemini --model {m} --yolo")
            }
            _ => {
                let m = choice.model.as_deref().unwrap_or("sonnet");
                format!("claude -p --model {m} --dangerously-skip-permissions")
            }
        }
    };

    let mut command = Command::new("bash");
    command
        .args(["-c", &cmd_str])
        .current_dir(&wt_path)
        .env("TACHO_AGENT", &item.key)
        .env("TACHO_TASK", &item.key)
        .env("TACHO_MANAGED_WORKER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err))
        .process_group(0);

    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let pid = child.id();
    let now = now_secs();
    let base_pr = item.base.as_deref().and_then(|s| s.parse::<u64>().ok());

    let run = AgentRun {
        key: item.key.clone(),
        pid: Some(pid),
        area: item.area.clone(),
        brief: item.brief.clone(),
        worktree: wt_path.to_string_lossy().to_string(),
        log: log_path.to_string_lossy().to_string(),
        status_note: "started".to_string(),
        started_at: now,
        finished_at: None,
        exit_code: None,
        pr: None,
        pr_none: false,
        pr_source: None,
        base_pr,
        resume_at: None,
        provider: choice.provider.clone(),
        model: choice.model.clone(),
        effort: choice.effort.clone(),
        session_id: None,
        compare: item.compare.clone(),
    };

    let because = format!(
        "dispatched {} on provider={} model={:?} (pid={})",
        item.key, choice.provider, choice.model, pid
    );
    store.mutate(Some(("dispatch", "start", &item.key, &because)), |st| {
        st.agents.insert(item.key.clone(), run);
        st.todo.retain(|t| t.key != item.key);
    })?;
    let _ = store.emit_state_band(
        &format!("task:{}/state", root_key),
        "InProgress",
        TaskStatus::InProgress,
        &because,
        now_ns(),
    );
    emit_decision_event(&format!("agent:{}", item.key), "dispatch:start", &because);
    Ok(pid)
}

/// Executes `tacho session <key> ["<brief>" | --resume]`.
pub fn execute_session(args: SessionArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;

    if args.resume {
        let root = base_key(&args.key);
        let Some(last) = state
            .agents
            .get(&args.key)
            .or_else(|| tacho_core::decide::dispatch::latest(&state.agents, &state.retired, root))
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no previous run found for '{}'", args.key),
            ));
        };
        let item = resume_task(last, "", "", now_secs());
        let pid = spawn_worker_for_item(&mut store, &item)?;
        println!("Resumed {} as {} (pid {pid})", args.key, item.key);
        return Ok(());
    }

    let brief = args
        .brief
        .unwrap_or_else(|| format!("Interactive session for {}", args.key));
    let item = TodoItem {
        key: args.key.clone(),
        brief,
        area: args.area.unwrap_or_default(),
        base: args.base,
        after: Vec::new(),
        priority: 1,
        created_at: now_secs(),
        provider: args.provider,
        model: args.model,
        effort: args.effort,
        compare: None,
    };
    let pid = spawn_worker_for_item(&mut store, &item)?;
    println!("Started session {} (pid {pid})", args.key);
    Ok(())
}

/// Executes `tacho hold <target> [--reason "..."]`.
pub fn execute_hold(args: HoldArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;

    if args.target.is_none() {
        if state.holds.is_empty() {
            println!("No active holds.");
            return Ok(());
        }
        let now = now_secs();
        for (target, h) in &state.holds {
            println!(
                "{:<20} {:>6}s  {}",
                target,
                (now - h.since).max(0),
                h.reason
            );
        }
        return Ok(());
    }

    let target = args.target.unwrap();
    let known_areas: BTreeSet<String> = state.area_rules.iter().map(|r| r.area.clone()).collect();
    if let Some(prob) = hold_problem(
        &target,
        &state.todo,
        &state.agents,
        &state.retired,
        &known_areas,
    ) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }

    let reason = args.reason.unwrap_or_else(|| "manual hold".to_string());
    let now = now_secs();
    store.mutate(Some(("dispatch", "hold", &target, &reason)), |st| {
        st.holds = with_hold(&st.holds, &target, &reason, now);
    })?;
    emit_decision_event("system", "dispatch:hold", &format!("{target}: {reason}"));
    println!("Held {target}: {reason}");
    Ok(())
}

/// Executes `tacho release <target>`.
pub fn execute_release(args: ReleaseArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    if let Some(prob) = release_problem(&args.target, &state.holds) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }
    store.mutate(
        Some(("dispatch", "release", &args.target, "released hold")),
        |st| {
            st.holds = without_hold(&st.holds, &args.target);
        },
    )?;
    emit_decision_event("system", "dispatch:release", &args.target);
    println!("Released hold on {}", args.target);
    Ok(())
}

/// Executes `tacho retire <key>`.
pub fn execute_retire(args: RetireArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let live = live_agent_keys(&state);
    if let Some(prob) = retire_problem(&args.key, &state.todo, &state.agents, &state.retired, &live)
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }
    store.mutate(
        Some(("dispatch", "retire", &args.key, "retired by user")),
        |st| {
            let (new_todo, new_retired, new_holds) =
                retire(&args.key, &st.todo, &st.agents, &st.retired, &st.holds);
            st.todo = new_todo;
            st.retired = new_retired;
            st.holds = new_holds;
        },
    )?;
    let _ = store.emit_state_band(
        &format!("task:{}/state", base_key(&args.key)),
        "Cancelled",
        TaskStatus::Cancelled,
        "retired by user",
        now_ns(),
    );
    emit_decision_event(
        &format!("task:{}", args.key),
        "dispatch:retire",
        "retired by user",
    );
    println!("Retired {}", args.key);
    Ok(())
}

/// Executes `tacho attach <key> <pr|none>`.
pub fn execute_attach(args: AttachArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    if !state.agents.contains_key(&args.key) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("unknown agent run '{}'", args.key),
        ));
    }
    let target_str = args
        .pr
        .trim()
        .trim_start_matches('#')
        .trim_start_matches("pr:");
    let (pr_num, is_none) = if target_str.eq_ignore_ascii_case("none") {
        (None, true)
    } else {
        let n: u64 = target_str.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("expected PR number or 'none', got '{}'", args.pr),
            )
        })?;
        (Some(n), false)
    };

    let because = if is_none {
        format!("attached PR: none to {}", args.key)
    } else {
        format!("attached PR #{} to {}", pr_num.unwrap(), args.key)
    };

    store.mutate(Some(("dispatch", "attach", &args.key, &because)), |st| {
        if let Some(a) = st.agents.get_mut(&args.key) {
            a.pr = pr_num;
            a.pr_none = is_none;
            a.pr_source = Some("attached".to_string());
            if a.finished_at.is_none() {
                a.finished_at = Some(now_secs());
            }
            if a.exit_code.is_none() {
                a.exit_code = Some(0);
            }
        }
    })?;
    let (label, status) = if is_none {
        ("Done", TaskStatus::Done)
    } else {
        ("Review", TaskStatus::Review)
    };
    let _ = store.emit_state_band(
        &format!("task:{}/state", base_key(&args.key)),
        label,
        status,
        &because,
        now_ns(),
    );
    emit_decision_event(&format!("agent:{}", args.key), "dispatch:attach", &because);
    println!("{because}");
    Ok(())
}

/// Executes `tacho stop <key>`.
pub fn execute_stop(args: StopArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let Some(agent) = state.agents.get(&args.key) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("unknown agent '{}'", args.key),
        ));
    };
    if let Some(pid) = agent.pid {
        if pid_alive(pid) {
            unsafe {
                let _ = libc::kill(-(pid as i32), libc::SIGTERM);
            }
            thread::sleep(Duration::from_millis(200));
            if pid_alive(pid) {
                unsafe {
                    let _ = libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
    let now = now_secs();
    store.mutate(
        Some(("dispatch", "stop", &args.key, "stopped by user")),
        |st| {
            if let Some(a) = st.agents.get_mut(&args.key) {
                a.finished_at = Some(now);
                a.exit_code = Some(143);
                a.status_note = "stopped".to_string();
            }
            // Place a hold so automatic resume doesn't immediately restart it unless asked
            st.holds = with_hold(&st.holds, base_key(&args.key), "stopped by user", now);
        },
    )?;
    emit_decision_event(
        &format!("agent:{}", args.key),
        "dispatch:stop",
        "stopped by user",
    );
    println!(
        "Stopped {} and placed hold on {}",
        args.key,
        base_key(&args.key)
    );
    Ok(())
}

/// Executes `tacho tell <key> "<message>"`.
pub fn execute_tell(args: TellArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let root = base_key(&args.key);
    let Some(last) = state
        .agents
        .get(&args.key)
        .or_else(|| tacho_core::decide::dispatch::latest(&state.agents, &state.retired, root))
        .cloned()
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("unknown agent '{}'", args.key),
        ));
    };

    let now = now_secs();
    let because = format!("nudge {}: {}", args.key, args.message);
    store.mutate(Some(("watch", "tell", &args.key, &because)), |st| {
        st.nudged.insert(args.key.clone());
        if let Some(a) = st.agents.get_mut(&last.key) {
            a.resume_at = Some(now);
            a.status_note = format!("nudged: {}", args.message);
            a.brief = format!("{}\n\nGuidance from operator:\n{}", a.brief, args.message);
        }
    })?;
    emit_decision_event(&format!("agent:{}", args.key), "watch:tell", &because);
    println!(
        "Sent message to {} (scheduled immediate resume if stopped)",
        args.key
    );
    Ok(())
}

/// Executes `tacho next` (dry-run preview of what the next orchestrator tick will dispatch/verify/land).
pub fn execute_next(args: NextArgs) -> io::Result<()> {
    let store = open_fleet_store()?;
    let state = store.load()?;
    let live = live_agent_keys(&state);
    let free_slots = state.max_workers.saturating_sub(live.len());
    let passed_prs: BTreeSet<u64> = state.passed.keys().copied().collect();
    let failed_prs: BTreeSet<u64> = state.failed.keys().copied().collect();

    let (chosen, _) = next_work(
        &state.todo,
        &state.agents,
        &state.retired,
        &state.holds,
        &live,
        &passed_prs,
        &failed_prs,
        &state.merged_prs,
        &state.closed_prs,
        free_slots,
    );
    let planned_batches = batches(
        &state.open_prs_cache,
        &state.holds,
        &state.passed,
        &state.failed,
        &state.batches,
        &state.area_rules,
        6,
    );
    let areas = area_report(
        &state.agents,
        &state.todo,
        &state.retired,
        &state.holds,
        &live,
        &passed_prs,
        &failed_prs,
        &state.merged_prs,
        &state.closed_prs,
    );

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "free_slots": free_slots,
                "next_work": chosen,
                "planned_batches": planned_batches,
                "areas": areas,
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    println!(
        "Workers: {}/{} active ({} free slots)",
        live.len(),
        state.max_workers,
        free_slots
    );
    if chosen.is_empty() {
        println!("Next work to dispatch: (none)");
    } else {
        println!("Next work to dispatch:");
        for t in &chosen {
            println!(
                "  - {} (area={}, base={})",
                t.key,
                if t.area.is_empty() { "-" } else { &t.area },
                t.base.as_deref().unwrap_or("main")
            );
        }
    }
    if !planned_batches.is_empty() {
        println!("Planned batches:");
        for b in &planned_batches {
            println!("  - {} [{}] PRs {:?} ({})", b.id, b.group, b.prs, b.because);
        }
    }
    if !areas.is_empty() {
        println!("Area gates:");
        for ar in &areas {
            println!(
                "  - {:<14} {:<10} base={:<6} ({})",
                ar.area,
                ar.state,
                ar.base_pr
                    .map(|p| format!("#{p}"))
                    .unwrap_or_else(|| "main".to_string()),
                ar.detail
            );
        }
    }
    Ok(())
}

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

/// Executes `tacho flakes`.
pub fn execute_flakes(args: FlakesArgs) -> io::Result<()> {
    let store = open_fleet_store()?;
    let state = store.load()?;
    let rows = flake_rows(&state.flakes);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("No flaky tests recorded.");
        return Ok(());
    }
    println!(
        "{:<42} {:<12} {:>6} {:<20} BATCHES",
        "TEST", "AREA", "COUNT", "TASK"
    );
    for r in &rows {
        println!(
            "{:<42} {:<12} {:>6} {:<20} {:?}",
            r.test,
            if r.area.is_empty() { "-" } else { &r.area },
            r.count,
            r.task_key.as_deref().unwrap_or("-"),
            r.batches
        );
    }
    Ok(())
}

/// Executes `tacho batch [<pr...>]` — runs speculative batch verification + automatic binary bisection on failure.
pub fn execute_batch(args: BatchArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let cwd = std::env::current_dir()?;

    let main_sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "HEAD".to_string());

    let pr_nums: Vec<u64> = if !args.prs.is_empty() {
        args.prs.clone()
    } else {
        let planned = batches(
            &state.open_prs_cache,
            &state.holds,
            &state.passed,
            &state.failed,
            &state.batches,
            &state.area_rules,
            6,
        );
        if let Some(first) = planned.first() {
            first.prs.clone()
        } else {
            println!("No PRs need batch verification right now.");
            return Ok(());
        }
    };

    let mut shas = BTreeMap::new();
    let mut areas = BTreeSet::new();
    for &n in &pr_nums {
        if let Some(p) = state.open_prs_cache.iter().find(|p| p.number == n) {
            shas.insert(n, p.head.clone());
            for a in tacho_core::decide::verify::areas_for_files(&p.files, &state.area_rules) {
                areas.insert(a);
            }
        } else {
            shas.insert(n, main_sha.clone());
            areas.insert("workspace".to_string());
        }
    }
    let area_list: Vec<String> = areas.into_iter().collect();

    if let Some(reused) = reusable_batch(&state.batches, &pr_nums, &shas, &main_sha, &area_list) {
        println!(
            "Reusing green batch {} for PRs {:?} on main {}",
            reused.id,
            pr_nums,
            &main_sha[..main_sha.len().min(12)]
        );
        return Ok(());
    }

    let batch_id = new_batch_id(&state.batches);
    let tree_sha = crate::kit::tree_of_worktree(&cwd).unwrap_or_else(|_| main_sha.clone());

    if args.dry_run {
        println!("Batch ID: {batch_id}");
        println!("PRs:      {:?}", pr_nums);
        println!("Main SHA: {main_sha}");
        println!("Areas:    {:?}", area_list);
        return Ok(());
    }

    let now = now_secs();
    let batch_start_ns = now_ns();
    let check_cmd = args
        .cmd
        .clone()
        .unwrap_or_else(|| "cargo test --workspace".to_string());
    println!(
        "Running batch {batch_id} on PRs {:?}: `{check_cmd}`",
        pr_nums
    );

    let run_probe = |k: usize, label: &str| -> io::Result<(bool, String, u64, u64)> {
        let p_start = now_ns();
        let prefix_prs: Vec<String> = pr_nums.iter().take(k).map(|n| n.to_string()).collect();
        let out = Command::new("bash")
            .args(["-c", &check_cmd])
            .current_dir(&cwd)
            .env("TACHO_BATCH_ID", &batch_id)
            .env("TACHO_BATCH_PROBE_K", k.to_string())
            .env("TACHO_BATCH_PROBE_PRS", prefix_prs.join(","))
            .env("TACHO_BATCH_LABEL", label)
            .output()?;
        let p_end = now_ns();
        let txt = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok((out.status.success(), txt, p_start, p_end))
    };

    let (mut ok, mut log_text, p0_start, p0_end) = run_probe(pr_nums.len(), "r0")?;
    let _ = store.emit_duration_slice(
        &format!("batch:{batch_id}"),
        &format!("check PRs {:?}", pr_nums),
        p0_start,
        p0_end,
        &check_cmd,
    );
    let mut recorded_flake_names = Vec::new();

    // If the check failed without a deterministic counterexample, retry once (`r0-again`) to detect flakes.
    if !ok && counterexample(&log_text).is_none() {
        let f_tests_initial = failed_tests(&log_text);
        let (retry_ok, retry_log, r_start, r_end) = run_probe(pr_nums.len(), "r0-again")?;
        let _ = store.emit_duration_slice(
            &format!("batch:{batch_id}"),
            "retry r0-again",
            r_start,
            r_end,
            "flake retry on failure without counterexample",
        );
        if retry_ok {
            ok = true;
            recorded_flake_names = if f_tests_initial.is_empty() {
                vec![summary_line(&log_text)]
            } else {
                f_tests_initial
            };
        } else {
            log_text = retry_log;
        }
    }

    let finished = now_secs();

    if ok {
        let b_run = BatchRun {
            id: batch_id.clone(),
            group: area_list
                .first()
                .cloned()
                .unwrap_or_else(|| "default".to_string()),
            prs: pr_nums.clone(),
            shas: shas.clone(),
            main_sha: main_sha.clone(),
            areas: area_list.clone(),
            status: "passed".to_string(),
            started_at: now,
            finished_at: Some(finished),
            tree: Some(tree_sha.clone()),
            pid: None,
            log: String::new(),
            culprit: None,
            skipped: Vec::new(),
            flakes: recorded_flake_names.clone(),
        };
        let because = format!("batch {batch_id} passed for PRs {:?}", pr_nums);
        store.mutate(Some(("verify", "batch_pass", &batch_id, &because)), |st| {
            if !recorded_flake_names.is_empty() {
                let area_hint = area_list.first().map(String::as_str).unwrap_or("");
                let updated = record_flakes(
                    &st.flakes,
                    &recorded_flake_names,
                    &batch_id,
                    area_hint,
                    &log_text,
                    finished,
                );
                let (with_tasks, new_items) =
                    flake_tasks(&updated, &st.todo, &st.agents, &st.retired, finished);
                st.flakes = with_tasks;
                st.todo.extend(new_items);
            }
            for &pr in &pr_nums {
                let sha = shas.get(&pr).cloned().unwrap_or_else(|| main_sha.clone());
                st.failed.remove(&pr);
                st.passed.insert(
                    pr,
                    PassedRecord {
                        pr,
                        sha,
                        main_sha: main_sha.clone(),
                        batch_id: batch_id.clone(),
                        passed_at: finished,
                        tree: tree_sha.clone(),
                        areas: area_list.clone(),
                    },
                );
            }
            st.batches.push(b_run);
        })?;
        for &pr in &pr_nums {
            if let Some(a) = state.agents.values().find(|a| a.pr == Some(pr)) {
                let _ = store.emit_state_band(
                    &format!("task:{}/state", base_key(&a.key)),
                    "Done",
                    TaskStatus::Done,
                    &because,
                    now_ns(),
                );
            }
        }
        emit_decision_event(&format!("batch:{batch_id}"), "verify:pass", &because);
        println!(
            "Batch {batch_id} PASSED (tree {})",
            &tree_sha[..tree_sha.len().min(12)]
        );
    } else {
        let mut kind = classify(&log_text).to_string();
        let mut sum = summary_line(&log_text);
        let mut f_tests = failed_tests(&log_text);
        let mut ce = counterexample(&log_text);

        // Run real binary bisection probe loop (`next_probe(&bisect)`), probing k=0 (`main`)
        // before blaming a single or first PR.
        let members: Vec<(u64, String)> = pr_nums
            .iter()
            .map(|&p| (p, shas.get(&p).cloned().unwrap_or_default()))
            .collect();
        let mut bisect = Bisect {
            members,
            results: BTreeMap::from([(pr_nums.len(), false)]),
        };
        while let ProbeStep::Probe(k) = next_probe(&bisect) {
            let (probe_ok, probe_log, ps, pe) = run_probe(k, &format!("bisect-k{k}"))?;
            let _ = store.emit_duration_slice(
                &format!("batch:{batch_id}"),
                &format!("bisect k={k}"),
                ps,
                pe,
                &format!("probe k={k} ok={probe_ok}"),
            );
            if !probe_ok && k > 0 {
                log_text = probe_log;
                kind = classify(&log_text).to_string();
                sum = summary_line(&log_text);
                f_tests = failed_tests(&log_text);
                ce = counterexample(&log_text);
            }
            bisect.results.insert(k, probe_ok);
        }

        let terminal_step = next_probe(&bisect);
        if terminal_step == ProbeStep::MainBroken {
            let b_run = BatchRun {
                id: batch_id.clone(),
                group: area_list
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "default".to_string()),
                prs: pr_nums.clone(),
                shas: shas.clone(),
                main_sha: main_sha.clone(),
                areas: area_list.clone(),
                status: "main-broken".to_string(),
                started_at: now,
                finished_at: Some(finished),
                tree: Some(tree_sha),
                pid: None,
                log: log_text.clone(),
                culprit: None,
                skipped: Vec::new(),
                flakes: Vec::new(),
            };
            let because = format!("batch {batch_id} aborted: base main is red ({sum})");
            store.mutate(Some(("verify", "main_broken", &batch_id, &because)), |st| {
                st.batches.push(b_run);
            })?;
            let _ = store.emit_duration_slice(
                &format!("batch:{batch_id}"),
                "main-broken",
                batch_start_ns,
                now_ns(),
                &because,
            );
            eprintln!("Batch {batch_id} MAIN BROKEN: {sum}");
            return Err(io::Error::other(because));
        }

        let culprit = match terminal_step {
            ProbeStep::Culprit((pr, _)) => Some(pr),
            _ => pr_nums.first().copied(),
        };

        let b_run = BatchRun {
            id: batch_id.clone(),
            group: area_list
                .first()
                .cloned()
                .unwrap_or_else(|| "default".to_string()),
            prs: pr_nums.clone(),
            shas: shas.clone(),
            main_sha: main_sha.clone(),
            areas: area_list.clone(),
            status: "failed".to_string(),
            started_at: now,
            finished_at: Some(finished),
            tree: Some(tree_sha),
            pid: None,
            log: log_text.clone(),
            culprit,
            skipped: Vec::new(),
            flakes: Vec::new(),
        };
        let because = format!("batch {batch_id} failed ({sum}), culprit={culprit:?}");
        store.mutate(Some(("verify", "batch_fail", &batch_id, &because)), |st| {
            if let Some(c_pr) = culprit {
                if kind == "code" {
                    let sha = shas.get(&c_pr).cloned().unwrap_or_default();
                    st.failed.insert(
                        c_pr,
                        FailedRecord {
                            pr: c_pr,
                            sha,
                            batch_id: batch_id.clone(),
                            failed_at: finished,
                            kind: kind.clone(),
                            summary: sum.clone(),
                            failed_tests: f_tests.clone(),
                            counterexample: ce.clone(),
                            log: log_text.clone(),
                        },
                    );
                }
            }
            st.batches.push(b_run);
        })?;
        emit_decision_event(&format!("batch:{batch_id}"), "verify:fail", &because);
        let parent_map = parents(&state.open_prs_cache);
        if let Some(c_pr) = culprit {
            if let Some(a) = state.agents.values().find(|a| a.pr == Some(c_pr)) {
                let _ = store.emit_state_band(
                    &format!("task:{}/state", base_key(&a.key)),
                    "Failed",
                    TaskStatus::Failed,
                    &because,
                    now_ns(),
                );
            }
            let reruns = reruns_on_red(&pr_nums, c_pr, &parent_map);
            eprintln!("Batch {batch_id} FAILED: {sum} (culprit PR #{c_pr}, reruns={reruns:?})");
            if !reruns.is_empty() && kind == "code" {
                let _ = execute_batch(BatchArgs {
                    prs: reruns,
                    cmd: args.cmd,
                    dry_run: false,
                });
            }
        } else {
            eprintln!("Batch {batch_id} FAILED: {sum}");
        }
        return Err(io::Error::other(sum));
    }
    Ok(())
}

/// Executes `tacho replay <batch-id>`.
pub fn execute_replay(args: ReplayArgs) -> io::Result<()> {
    let store = open_fleet_store()?;
    let state = store.load()?;
    let Some(b) = state.batches.iter().find(|b| b.id == args.batch_id) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("batch '{}' not found", args.batch_id),
        ));
    };
    println!(
        "Replaying batch {} (PRs {:?} on main {})",
        b.id, b.prs, b.main_sha
    );
    if !b.log.is_empty() {
        println!("--- Recorded Log ---\n{}", b.log);
    }
    execute_batch(BatchArgs {
        prs: b.prs.clone(),
        cmd: args.cmd,
        dry_run: args.dry_run,
    })
}

/// Executes `tacho queue`.
pub fn execute_queue(args: QueueArgs) -> io::Result<()> {
    let store = open_fleet_store()?;
    let state = store.load()?;
    let cwd = std::env::current_dir()?;
    let current_main = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let ready = queue(
        &state.open_prs_cache,
        &state.passed,
        &state.batches,
        &state.holds,
        &current_main,
        &HashMap::new(),
    );
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ready).unwrap_or_default()
        );
        return Ok(());
    }
    if ready.is_empty() {
        println!("Ready queue is empty.");
        return Ok(());
    }
    println!("{:<8} {:<14} {:<10} TITLE", "PR", "SHA", "BATCH");
    for item in &ready {
        println!(
            "#{:<7} {:<14} {:<10} {}",
            item.pr,
            &item.sha[..item.sha.len().min(12)],
            item.batch_id,
            item.title
        );
    }
    Ok(())
}

/// Executes `tacho train`.
pub fn execute_train(args: TrainArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;

    if let Some(ref batch_id) = args.from_batch {
        let Some(b) = state.batches.iter().find(|b| &b.id == batch_id).cloned() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("batch '{batch_id}' not found"),
            ));
        };
        let tree = b.tree.clone().unwrap_or_else(|| b.main_sha.clone());
        let train = make_train(&state.trains, &b, &b.main_sha, &tree, now_secs())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let tid = train.id.clone();
        let because = format!(
            "opened train {tid} from batch {batch_id} (PRs {:?})",
            train.prs
        );
        store.mutate(Some(("land", "train_open", &tid, &because)), |st| {
            st.trains = with_train(&st.trains, train);
        })?;
        let _ = store.emit_state_band(
            &format!("train:{tid}"),
            "open",
            TaskStatus::InProgress,
            &because,
            now_ns(),
        );
        emit_decision_event(&format!("train:{tid}"), "land:train_open", &because);
        println!("{because}");
        return Ok(());
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&state.trains).unwrap_or_default()
        );
        return Ok(());
    }

    if let Some(open) = open_for(&state.trains) {
        println!(
            "Open train {}: PRs {:?} on base {} (tree {})",
            open.id,
            open.prs,
            &open.base[..open.base.len().min(12)],
            &open.tree[..open.tree.len().min(12)]
        );
    } else {
        println!("No open merge train.");
    }
    Ok(())
}

/// Executes `tacho release-train`.
pub fn execute_release_train(args: ReleaseTrainArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;

    let elig = eligible(
        &state.open_prs_cache,
        &state.holds,
        &state.failed,
        &state.area_rules,
    );
    let ordered_nums = order_train_prs(&elig, &state.open_prs_cache);
    let parent_map = parents(&state.open_prs_cache);
    let cwd = std::env::current_dir()?;
    let compile_cmd = std::env::var("TACHO_COMPILE_CMD").ok();
    let (surviving, dropped) = drop_breakers(&ordered_nums, &parent_map, |slice| {
        if let Some(ref cmd) = compile_cmd {
            let prs_str = slice
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let out = Command::new("bash")
                .args(["-c", cmd])
                .current_dir(&cwd)
                .env("TACHO_TRAIN_SLICE_PRS", &prs_str)
                .output()
                .map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
        }
        Ok(())
    });

    let mut changed_union = BTreeSet::new();
    let mut member_shas = BTreeMap::new();
    for &n in &surviving {
        if let Some(pr) = state.open_prs_cache.iter().find(|p| p.number == n) {
            member_shas.insert(n, pr.head.clone());
            for f in &pr.files {
                changed_union.insert(f.clone());
            }
        }
    }
    let changed_vec: Vec<String> = changed_union.into_iter().collect();
    let settled_cmds = settle(&changed_vec, &state.generators);
    for cmd in &settled_cmds {
        let _ = Command::new("bash")
            .args(["-c", cmd])
            .current_dir(&cwd)
            .status();
    }

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let tree = crate::kit::tree_of_worktree(&cwd).unwrap_or_else(|_| head.clone());

    let mut tpr = state.train_pr.clone().unwrap_or_else(|| TrainPrState {
        number: None,
        branch: "release/train".to_string(),
        base: "main".to_string(),
        head: head.clone(),
        tree: tree.clone(),
        members: surviving.clone(),
        member_shas: member_shas.clone(),
        dropped: dropped.clone(),
        reviewed: None,
        proved_batch: None,
        proved_tree: None,
    });
    tpr.head = head;
    tpr.tree = tree;
    tpr.members = surviving;
    tpr.member_shas = member_shas;
    tpr.dropped = dropped;
    if args.approve {
        tpr.reviewed = Some(tpr.head.clone());
    }
    let waiting = waiting_for(
        &tpr,
        &state.batches,
        &state.open_prs_cache,
        &state.area_rules,
        true,
    );

    store.mutate(
        Some((
            "land",
            "release_train",
            &tpr.branch,
            "updated combined train PR state",
        )),
        |st| {
            st.train_pr = Some(tpr.clone());
        },
    )?;

    if !settled_cmds.is_empty() {
        println!("Settled generators: {:?}", settled_cmds);
    }
    println!(
        "Release Train branch '{}' (members {:?}, dropped {:?})",
        tpr.branch, tpr.members, tpr.dropped
    );
    if waiting.is_empty() {
        println!("Status: READY TO LAND");
    } else {
        println!("Waiting for:");
        for w in &waiting {
            println!("  - {w}");
        }
    }
    Ok(())
}

/// Executes `tacho land`.
pub fn execute_land(args: LandArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let cwd = std::env::current_dir()?;
    let current_main = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let current_tree = Command::new("git")
        .args(["rev-parse", "HEAD^{tree}"])
        .current_dir(&cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    if let Some(open) = open_for(&state.trains).cloned() {
        if let Some(refusal_reason) =
            train_refusal(&open, &current_main, &state.open_prs_cache, &state.holds)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, refusal_reason));
        }
        if args.dry_run {
            println!("Would land train {}: PRs {:?}", open.id, open.prs);
            return Ok(());
        }
        let mut landed = finished(&open);
        if let Some(err) = mismatch(&landed, &current_tree) {
            landed.status = "mismatch".to_string();
            store.mutate(Some(("land", "train_mismatch", &open.id, &err)), |st| {
                for t in &mut st.trains {
                    if t.id == open.id {
                        *t = landed.clone();
                    }
                }
            })?;
            let _ = store.emit_state_band(
                &format!("train:{}", open.id),
                "mismatch",
                TaskStatus::Failed,
                &err,
                now_ns(),
            );
            emit_decision_event(&format!("train:{}", open.id), "land:mismatch", &err);
            return Err(io::Error::other(err));
        }
        let because = format!("landed train {} (PRs {:?})", open.id, open.prs);
        store.mutate(Some(("land", "land_train", &open.id, &because)), |st| {
            for &p in &open.prs {
                st.merged_prs.insert(p);
            }
            for t in &mut st.trains {
                if t.id == open.id {
                    *t = landed.clone();
                }
            }
        })?;
        let _ = store.emit_state_band(
            &format!("train:{}", open.id),
            "landed",
            TaskStatus::Done,
            &because,
            now_ns(),
        );
        emit_decision_event(&format!("train:{}", open.id), "land:done", &because);
        println!("{because}");
        return Ok(());
    }

    let ready = queue(
        &state.open_prs_cache,
        &state.passed,
        &state.batches,
        &state.holds,
        &current_main,
        &HashMap::new(),
    );
    if ready.is_empty() {
        println!("Nothing ready to land.");
        return Ok(());
    }
    for item in &ready {
        println!(
            "Ready to land PR #{} ({}) from batch {}",
            item.pr, item.title, item.batch_id
        );
    }
    Ok(())
}

/// Collects host `/proc` entries for supervision and tidy checks.
fn scan_procs() -> Vec<ProcEntry> {
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

/// Executes `tacho tidy [--dry-run] [--min-age 3600]`.
pub fn execute_tidy(args: TidyArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let procs = scan_procs();
    let live_pids: BTreeSet<u32> = procs.iter().map(|p| p.pid).collect();

    // Reap stale runs whose PID exited
    let stale = stale_runs(&state.agents, &live_pids);
    let now = now_secs();
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
        wt_candidates.push(WorktreeCandidate {
            path: wt.path.clone(),
            branch: wt.branch.clone(),
            age_secs: (now - wt.created_at).max(0),
            dirty,
            unpushed_commits: unpushed,
            pr_merged_or_closed: false,
        });
    }

    let snap = TidySnapshot {
        now,
        worktrees: wt_candidates,
        checkouts: Vec::new(),
        containers: Vec::new(),
        networks: Vec::new(),
        live_agent_worktrees: live_wt,
        live_batch_worktrees: BTreeSet::new(),
        held_paths: BTreeSet::new(),
        held_ports,
    };

    let decisions = decide_tidy(&snap, args.min_age.unwrap_or(3_600));
    let zombies = zombie_lines(&procs);

    if decisions.is_empty() && stale.is_empty() && zombies.is_empty() {
        println!("Machine is tidy (nothing to clean up).");
        return Ok(());
    }

    for z in &zombies {
        println!("Zombie warning: {z}");
    }
    for d in &decisions {
        match d {
            TidyDecision::RemoveTree {
                path,
                rescue,
                because,
            } => {
                println!(
                    "{}RemoveTree {} (rescue={}): {}",
                    if args.dry_run { "[dry-run] " } else { "" },
                    path,
                    rescue,
                    because
                );
                if !args.dry_run {
                    if *rescue {
                        let wt_p = Path::new(path);
                        let slug = wt_p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("worktree");
                        if let Ok(tree_sha) = crate::kit::tree_of_worktree(wt_p) {
                            let parent_sha = Command::new("git")
                                .args(["-C", path, "rev-parse", "HEAD"])
                                .output()
                                .ok()
                                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                                .unwrap_or_default();
                            let commit_out = if !parent_sha.is_empty() {
                                Command::new("git")
                                    .args([
                                        "-C",
                                        path,
                                        "commit-tree",
                                        &tree_sha,
                                        "-p",
                                        &parent_sha,
                                        "-m",
                                        &format!("tacho rescue snapshot for {slug}"),
                                    ])
                                    .output()
                            } else {
                                Command::new("git")
                                    .args([
                                        "-C",
                                        path,
                                        "commit-tree",
                                        &tree_sha,
                                        "-m",
                                        &format!("tacho rescue snapshot for {slug}"),
                                    ])
                                    .output()
                            };
                            if let Ok(co) = commit_out {
                                let commit_sha =
                                    String::from_utf8_lossy(&co.stdout).trim().to_string();
                                if !commit_sha.is_empty() {
                                    let rescue_ref = format!("refs/tacho/rescue/{slug}-{now}");
                                    let _ = Command::new("git")
                                        .args(["-C", path, "update-ref", &rescue_ref, &commit_sha])
                                        .status();
                                    let _ = store.record_decision(
                                        "watch",
                                        "rescue_worktree",
                                        slug,
                                        &format!("archived dirty/unpushed worktree to {rescue_ref} ({commit_sha})"),
                                    );
                                }
                            }
                        }
                    }
                    let _ = Command::new("git")
                        .args(["worktree", "remove", "--force", path])
                        .status();
                    let _ = store.mutate(Some(("watch", "remove_worktree", path, because)), |st| {
                        st.worktrees.retain(|_, w| &w.path != path);
                    });
                }
            }
            other => {
                println!(
                    "{}{:?}",
                    if args.dry_run { "[dry-run] " } else { "" },
                    other
                );
            }
        }
    }
    Ok(())
}

/// Executes a single orchestrator & supervisor pass (`tacho tick`).
pub fn execute_tick(args: TickArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let now = now_secs();
    let tick_start_ns = now_ns();
    let procs = scan_procs();
    let live = live_agent_keys(&state);

    // 0. Reap exited agents and attribute PRs (`find_pr`) + clear idle chains
    let mut reaped = Vec::new();
    for (k, a) in &state.agents {
        if a.finished_at.is_none() && !live.contains(k) {
            let log_tail = fs::read_to_string(&a.log)
                .map(|s| {
                    let lines: Vec<&str> = s.lines().rev().take(50).collect();
                    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
                })
                .unwrap_or_default();
            let (found_pr, found_src) = find_pr(k, a.started_at, &state.open_prs_cache, &log_tail);
            let is_none = found_src.as_deref() == Some("none");
            reaped.push((k.clone(), found_pr, is_none, found_src));
        }
    }
    if !reaped.is_empty() && !args.dry_run {
        store.mutate(
            Some((
                "dispatch",
                "reap_agents",
                "agents",
                "reaped exited agents and attributed PRs",
            )),
            |st| {
                for (k, found_pr, is_none, found_src) in &reaped {
                    if let Some(a) = st.agents.get_mut(k) {
                        a.finished_at = Some(now);
                        if a.exit_code.is_none() {
                            a.exit_code = Some(if found_pr.is_some() || *is_none { 0 } else { 1 });
                        }
                        if found_pr.is_some() {
                            a.pr = *found_pr;
                            a.pr_source = found_src.clone();
                        } else if *is_none {
                            a.pr_none = true;
                            a.pr_source = Some("none".to_string());
                        }
                    }
                }
                let wt_clean_map = HashMap::new();
                let idle = idle_chains(&st.agents, &st.retired, &live, &wt_clean_map);
                for root in idle {
                    if let Some(a) = st.agents.get_mut(&root) {
                        a.pr_none = true;
                    }
                }
            },
        )?;
        for (k, found_pr, is_none, _) in &reaped {
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
            let _ = store.emit_state_band(
                &format!("task:{}/state", base_key(k)),
                label,
                status,
                &because,
                now_ns(),
            );
        }
    }

    // 1. Check for unguarded runaway CPU burners (`yes`, `stress`, `stress-ng` >= 10m) and duplicate batches
    for b in burners(&procs) {
        let because = format!(
            "killed runaway burner pid={} ({}) elapsed={}s",
            b.pid, b.comm, b.elapsed_secs
        );
        if !args.dry_run {
            unsafe {
                let _ = libc::kill(b.pid as i32, libc::SIGKILL);
            }
            let _ = store.record_decision("watch", "kill_burner", &b.pid.to_string(), &because);
            emit_decision_event("system", "watch:kill_burner", &because);
        }
        println!("{because}");
    }
    let dup_batch_ids = duplicate_batches(&state.batches);
    if !dup_batch_ids.is_empty() {
        for dup_id in &dup_batch_ids {
            let because = format!("aborted duplicate batch {dup_id}");
            if !args.dry_run {
                store.mutate(
                    Some(("watch", "kill_duplicate_batch", dup_id, &because)),
                    |st| {
                        for b in &mut st.batches {
                            if &b.id == dup_id {
                                if let Some(pid) = b.pid {
                                    unsafe {
                                        let _ = libc::kill(pid as i32, libc::SIGTERM);
                                    }
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

    // 2. Sample progress and supervise live agents
    let state = store.load()?;
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
            let sample = ProgressSample {
                ts: now,
                log_bytes,
                head_sha,
                dirty_files: dirty_files as u32,
                cpu_ticks: cpu,
            };
            let existing = state.samples.get(key).map(Vec::as_slice).unwrap_or(&[]);
            let updated_samples = take_sample(existing, sample, 60);
            let (v, idle) = verdict(&updated_samples, now);
            let step = supervise_step(key, v, idle, state.nudged.contains(key));
            if !args.dry_run {
                store.mutate(None, |st| {
                    st.samples.insert(key.clone(), updated_samples);
                })?;
            }
            match step {
                SuperviseAction::Ok => {}
                SuperviseAction::Resume { key, because } => {
                    println!("Supervise: {because}");
                    if !args.dry_run {
                        if let Some(pid) = a.pid {
                            unsafe {
                                let _ = libc::kill(-(pid as i32), libc::SIGTERM);
                            }
                        }
                        let resumed = resume_task(a, &because, "", now);
                        store.mutate(
                            Some(("watch", "supervise_resume", &key, &because)),
                            |st| {
                                if let Some(ag) = st.agents.get_mut(&key) {
                                    ag.finished_at = Some(now);
                                    ag.exit_code = Some(143);
                                }
                                st.todo.push(resumed);
                            },
                        )?;
                        emit_decision_event(&format!("agent:{key}"), "watch:supervise", &because);
                    }
                }
                SuperviseAction::Nudge { key, because } => {
                    println!("Supervise: {because}");
                    if !args.dry_run {
                        store.mutate(Some(("watch", "supervise_nudge", &key, &because)), |st| {
                            st.nudged.insert(key.clone());
                        })?;
                        emit_decision_event(&format!("agent:{key}"), "watch:supervise", &because);
                    }
                }
                SuperviseAction::Escalate { key, because } => {
                    println!("Supervise: {because}");
                    if !args.dry_run {
                        let _ = store.record_decision("watch", "supervise", &key, &because);
                        emit_decision_event(&format!("agent:{key}"), "watch:supervise", &because);
                    }
                }
            }
        }
    }

    // 3. Check for due automatic resumes (`needs_resume`), red-PR fixes (`decide_fixes`), and rebases (`next_hands`)
    let state = store.load()?;
    for root in tacho_core::decide::dispatch::base_keys(&state.agents, &state.retired) {
        if needs_resume(
            &state.agents,
            &state.todo,
            &state.retired,
            &state.holds,
            &live,
            &root,
            now,
        ) {
            if let Some(last) =
                tacho_core::decide::dispatch::latest(&state.agents, &state.retired, &root).cloned()
            {
                let item = resume_task(&last, "", "", now);
                let item_key = item.key.clone();
                let because = format!("auto-queued resume {item_key} after backoff");
                println!("{because}");
                if !args.dry_run {
                    store.mutate(
                        Some(("dispatch", "auto_resume", &item_key, &because)),
                        |st| {
                            st.todo.push(item);
                        },
                    )?;
                }
            }
        }
    }

    let state = store.load()?;
    let free_for_aux = state.max_workers.saturating_sub(live.len());
    let fix_decisions = decide_fixes(
        &state.failed,
        &state.open_prs_cache,
        &state.agents,
        &state.holds,
        &live,
        &state.fix_notified,
        free_for_aux,
    );
    if (!fix_decisions.dispatch.is_empty() || !fix_decisions.notify.is_empty()) && !args.dry_run {
        store.mutate(
            Some((
                "verify",
                "decide_fixes",
                "failed_prs",
                "processed red PR fix actions",
            )),
            |st| {
                for (pr, _msg) in &fix_decisions.notify {
                    if let Some(rec) = st.failed.get(pr) {
                        st.fix_notified.insert(*pr, rec.sha.clone());
                    }
                }
                for (pr, rec) in fix_decisions.dispatch {
                    let fix_key = format!("fix-pr{pr}");
                    if !st.todo.iter().any(|t| t.key == fix_key) {
                        st.todo.push(TodoItem {
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
            },
        )?;
    }

    let state = store.load()?;
    let (to_rebase, _barred) = next_hands(
        &state.open_prs_cache,
        &state.agents,
        &state.holds,
        &live,
        &state.rebase_attempts,
        free_for_aux,
    );
    let rebase_prs: Vec<(u64, String)> = to_rebase
        .into_iter()
        .map(|p| (p.number, p.head.clone()))
        .collect();
    if !rebase_prs.is_empty() && !args.dry_run {
        store.mutate(
            Some((
                "dispatch",
                "next_hands",
                "open_prs",
                "processed conflict rebase actions",
            )),
            |st| {
                for (pr_num, head_sha) in rebase_prs {
                    st.rebase_attempts.insert(pr_num, head_sha.clone());
                    let reb_key = format!("rebase-pr{pr_num}");
                    if !st.todo.iter().any(|t| t.key == reb_key) {
                        st.todo.push(TodoItem {
                            key: reb_key,
                            brief: format!(
                                "Rebase conflicting PR #{pr_num} at {head_sha} onto main"
                            ),
                            area: String::new(),
                            base: Some(pr_num.to_string()),
                            after: Vec::new(),
                            priority: 1,
                            created_at: now,
                            ..Default::default()
                        });
                    }
                }
            },
        )?;
    }

    // 4. Auto-queue flake fix tasks if any test crossed FLAKE_TASK_THRESHOLD
    let state = store.load()?;
    let (updated_flakes, new_flake_tasks) = flake_tasks(
        &state.flakes,
        &state.todo,
        &state.agents,
        &state.retired,
        now,
    );
    if !new_flake_tasks.is_empty() && !args.dry_run {
        store.mutate(
            Some(("verify", "flake_tasks", "flakes", "queued flake fix tasks")),
            |st| {
                st.flakes = updated_flakes;
                st.todo.extend(new_flake_tasks);
            },
        )?;
    }

    // 5. Dispatch ready tasks up to free worker slots
    let state = store.load()?;
    let free_slots = state.max_workers.saturating_sub(live.len());
    let passed_prs: BTreeSet<u64> = state.passed.keys().copied().collect();
    let failed_prs: BTreeSet<u64> = state.failed.keys().copied().collect();
    let (chosen, _) = next_work(
        &state.todo,
        &state.agents,
        &state.retired,
        &state.holds,
        &live,
        &passed_prs,
        &failed_prs,
        &state.merged_prs,
        &state.closed_prs,
        free_slots,
    );

    for item in &chosen {
        if args.dry_run {
            println!("[dry-run] Would dispatch {}", item.key);
        } else {
            let pid = spawn_worker_for_item(&mut store, item)?;
            println!("Dispatched {} (pid {pid})", item.key);
        }
    }

    let tick_summary = format!(
        "active={}, dispatched={}, reaped={}",
        live.len(),
        chosen.len(),
        reaped.len()
    );

    if !args.dry_run {
        store.mutate(None, |st| {
            st.last_tick_ts = Some(now);
        })?;
        let _ = store.emit_duration_slice(
            "fleet/ticks",
            "tick",
            tick_start_ns,
            now_ns(),
            &tick_summary,
        );
        // Write atomic snapshot.json via .tmp + rename for fast dashboard reads
        let st = store.load()?;
        let home = crate::paths::resolve_home();
        let snap_tmp = home.join("snapshot.json.tmp");
        let snap_path = home.join("snapshot.json");
        if let Ok(json) = serde_json::to_string_pretty(&st) {
            if fs::write(&snap_tmp, json).is_ok() {
                let _ = fs::rename(&snap_tmp, &snap_path);
            }
        }
    }

    println!("Tick complete at {} ({})", now, tick_summary);
    Ok(())
}

/// Executes `tacho events [--limit 50] [--category ...] [--target ...]`.
pub fn execute_events(args: EventsArgs) -> io::Result<()> {
    let store = open_fleet_store()?;
    let records = store
        .recent_decisions(
            args.limit.unwrap_or(50),
            args.category.as_deref(),
            args.target.as_deref(),
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&records).unwrap_or_default()
        );
        return Ok(());
    }
    if records.is_empty() {
        println!("No decisions recorded yet.");
        return Ok(());
    }
    for r in &records {
        println!(
            "[{}] {:<10} {:<14} {:<18} because: {}",
            r.ts, r.category, r.action, r.target, r.because
        );
    }
    Ok(())
}

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

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "needs_you": attention,
                "active_workers": live,
                "todo_count": state.todo.len(),
                "holds_count": state.holds.len(),
                "batches_count": state.batches.len(),
            }))
            .unwrap_or_default()
        );
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
            println!(
                "  [{}] {}: {}  -> `{}`",
                item.kind, item.target, item.summary, item.command_hint
            );
        }
    }
    if !live.is_empty() {
        println!("\nACTIVE WORKERS:");
        for k in &live {
            if let Some(a) = state.agents.get(k) {
                println!(
                    "  {:<20} area={:<12} provider={:<8} note={}",
                    a.key,
                    if a.area.is_empty() { "-" } else { &a.area },
                    a.provider,
                    a.status_note
                );
            }
        }
    }
    Ok(())
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
        let body = serde_json::to_string_pretty(&state).unwrap_or_else(|_| "{}".to_string());
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

/// Executes `tacho watchdog` (checks whether the orchestrator tick is stale while work is pending).
pub fn execute_watchdog(args: WatchdogArgs) -> io::Result<()> {
    let mut store = open_fleet_store()?;
    let state = store.load()?;
    let now = now_secs();
    let live = live_agent_keys(&state);
    let has_active_work = !live.is_empty() || !state.todo.is_empty();
    let max_stale = args.max_stale.unwrap_or(600);
    let reason = down_reason(state.last_tick_ts, now, max_stale, has_active_work);
    let alert = down_alert(
        reason.as_deref(),
        state.last_watchdog_alert_ts,
        now,
        args.cooldown.unwrap_or(3_600),
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
