//! `tacho`: Flight recorder and autonomous fleet CLI for coding agents.

pub mod cli;
pub mod client;
pub mod daemon;
pub mod emit;
pub mod fleet;
pub mod hook;
pub mod install;
pub mod kit;
pub mod paths;
pub mod query;
pub mod run;
pub mod status;
pub mod system;
pub mod tail;
pub mod trace;

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::Command;

use clap::Parser;
use cli::{
    BatchCliArgs, Cli, Commands, DbArgs, HookTarget, LandArgs, PrArgs, ReleaseTrainArgs,
    RetireArgs, SessionCliArgs, TodoArgs, TrainArgs,
};
use tacho_core::decide::dispatch::guard_bash_tool;
use tacho_core::decide::land::{make_train, with_train};
use tacho_core::decide::verify::{allocate_ports, parse_env, slug_problem, BatchMode, BatchRun};
use tacho_daemon::WorktreeEntry;

fn exit_io_err(e: io::Error) -> ! {
    let code = if e.kind() == io::ErrorKind::InvalidInput {
        2
    } else {
        1
    };
    eprintln!("Error: {e}");
    std::process::exit(code);
}

/// Checks whether a bash command is an unwrapped CPU burner (`yes`, `stress`, `stress-ng`).
/// Allows `yes | ...` pipelines and `tacho starve` invocations.
fn check_cpu_burner_guard(cmd: &str) -> Option<String> {
    let trimmed = cmd.trim();
    if trimmed.contains("tacho starve") {
        return None;
    }
    for segment in trimmed.split(&[';', '&', '\n'][..]) {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }
        let pipe_parts: Vec<&str> = seg.split('|').map(str::trim).collect();
        if let Some(first) = pipe_parts.first() {
            let first_tok = first.split_whitespace().next().unwrap_or("");
            if first_tok == "yes" && pipe_parts.len() == 1 {
                return Some(
                    "unbounded CPU burner `yes` is forbidden; pipe it (`yes | ...`) or use `tacho starve -- <cmd>`"
                        .to_string(),
                );
            }
            if first_tok == "stress" || first_tok == "stress-ng" {
                return Some(format!(
                    "CPU burner `{first_tok}` is forbidden; use `tacho starve -- <cmd>` for bounded CPU starvation"
                ));
            }
        }
    }
    None
}

/// Executes `tacho hook guard` (`PreToolUse` guardrail check on stdin JSON).
fn execute_hook_guard() -> i32 {
    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() || input.trim().is_empty() {
        return 0;
    }
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&input) else {
        return 0;
    };
    let Some(obj) = val.as_object() else {
        return 0;
    };
    let tool_name = obj
        .get("tool_name")
        .or_else(|| obj.get("tool"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let under_starve_env = std::env::var("TACHO_STARVE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if let Err(reason) = hook::check_pre_tool_use_guardrails(&val, under_starve_env) {
        eprintln!("Refused by tacho guard: {reason}");
        return 2;
    }
    if !tool_name.eq_ignore_ascii_case("bash")
        && !tool_name.eq_ignore_ascii_case("shell")
        && !tool_name.eq_ignore_ascii_case("run_command")
    {
        return 0;
    }

    let tool_input = obj.get("tool_input").and_then(|v| v.as_object());
    let cmd = tool_input
        .and_then(|ti| ti.get("command").or_else(|| ti.get("cmd")))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let run_in_bg = tool_input
        .and_then(|ti| ti.get("run_in_background"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let under_starve = std::env::var("TACHO_STARVE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || cmd.contains("tacho starve");
    if let Some(ti_val) = obj.get("tool_input") {
        if let Err(reason) = hook::no_background_bash_check(ti_val) {
            eprintln!("Refused by tacho guard: {reason}");
            return 2;
        }
    }
    if let Err(reason) = hook::burner_guard_check(cmd, under_starve) {
        eprintln!("Refused by tacho guard: {reason}");
        return 2;
    }
    if !under_starve {
        if let Some(reason) = check_cpu_burner_guard(cmd) {
            eprintln!("Refused by tacho guard: {reason}");
            return 2;
        }
    }
    let sanitized_cmd = cmd.replace("yes |", "true |").replace("yes|", "true|");
    if let Err(reason) = guard_bash_tool(run_in_bg, &sanitized_cmd, under_starve) {
        eprintln!("Refused by tacho guard: {reason}");
        return 2;
    }
    0
}

fn format_env_map(map: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (k, v) in map {
        out.push_str(&format!("{k}={v}\n"));
    }
    out
}

/// Executes `tacho db <name> [checkout] [--rm]`.
fn execute_db(args: DbArgs) -> io::Result<()> {
    if let Some(prob) = slug_problem(&args.name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, prob));
    }
    let checkout = args
        .checkout
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut store = fleet::open_fleet_store()?;
    let state = store.load()?;

    let existing_port = state.worktrees.get(&args.name).map(|w| w.port);
    let (port, port_last) = if let Some(p) = existing_port {
        (p, p.saturating_add(9))
    } else {
        let mut used: BTreeSet<u16> = BTreeSet::new();
        for w in state.worktrees.values() {
            for p in w.port..=w.port_last {
                used.insert(p);
            }
        }
        allocate_ports(&used, |_| true).map_err(io::Error::other)?
    };

    if args.rm {
        if let Ok(down_cmd) = std::env::var("TACHO_DB_DOWN_CMD") {
            if !down_cmd.trim().is_empty() {
                let status = Command::new("bash")
                    .args(["-c", &down_cmd])
                    .current_dir(&checkout)
                    .env("SLUG", &args.name)
                    .env("DB_PORT", port.to_string())
                    .status()?;
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "TACHO_DB_DOWN_CMD failed with {status}"
                    )));
                }
            }
        }
        let env_path = checkout.join(".tachoenv");
        if env_path.is_file() {
            let txt = fs::read_to_string(&env_path)?;
            let mut map = parse_env(&txt);
            map.remove("DATABASE_URL");
            fleet::write_atomic(&env_path, format_env_map(&map).as_bytes())?;
        }
        store.record_decision("kit", "db_rm", &args.name, "removed per-worktree database")?;
        println!("Removed database for {}", args.name);
        return Ok(());
    }

    let mut db_url = format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/test_{}",
        args.name.replace('-', "_")
    );
    if let Ok(up_cmd) = std::env::var("TACHO_DB_UP_CMD") {
        if !up_cmd.trim().is_empty() {
            let out = Command::new("bash")
                .args(["-c", &up_cmd])
                .current_dir(&checkout)
                .env("SLUG", &args.name)
                .env("DB_PORT", port.to_string())
                .output()?;
            if !out.status.success() {
                return Err(io::Error::other(format!(
                    "TACHO_DB_UP_CMD failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let parsed = parse_env(&stdout);
            if let Some(u) = parsed.get("DATABASE_URL") {
                db_url = u.clone();
            }
        }
    }

    if checkout.is_dir() {
        let _ = kit::verify_and_plan_worktree_migrations(&checkout)?;
        let env_path = checkout.join(".tachoenv");
        let mut map = if env_path.is_file() {
            parse_env(&fs::read_to_string(&env_path).unwrap_or_default())
        } else {
            std::collections::BTreeMap::new()
        };
        map.entry("SLUG".to_string())
            .or_insert_with(|| args.name.clone());
        map.entry("PORT".to_string())
            .or_insert_with(|| port.to_string());
        map.insert("DATABASE_URL".to_string(), db_url.clone());
        fleet::write_atomic(&env_path, format_env_map(&map).as_bytes())?;
    }

    let now = fleet::now_secs();
    let checkout_str = checkout.to_string_lossy().to_string();
    store.mutate(Some(("kit", "db_up", &args.name, &db_url)), |st| {
        st.worktrees
            .entry(args.name.clone())
            .or_insert(WorktreeEntry {
                slug: args.name.clone(),
                path: checkout_str,
                branch: format!("agent/{}", args.name),
                base_sha: String::new(),
                base_pr: None,
                port,
                port_last,
                created_at: now,
            });
    })?;
    println!("DATABASE_URL={db_url}");
    Ok(())
}

/// Executes `tacho todo` with support for `--now` and `--retired`.
fn handle_todo(mut args: TodoArgs) -> io::Result<()> {
    if args.retired {
        if let Some(ref key) = args.key {
            if args.brief.is_none() && args.brief_file.is_none() {
                return fleet::execute_retire(RetireArgs {
                    key: key.clone(),
                    reason: Some("retired via todo --retired".to_string()),
                });
            }
        }
    }

    let dispatch_now = args.now;
    if dispatch_now {
        args.priority = true;
    }
    let target_key = args.key.clone();
    fleet::execute_todo(args)?;

    if dispatch_now {
        if let Some(key) = target_key {
            let mut store = fleet::open_fleet_store()?;
            let state = store.load()?;
            let live = fleet::live_agent_keys(&state);
            if live.len() < state.max_workers {
                if let Some(item) = state.todo.iter().find(|t| t.key == key).cloned() {
                    let pid = fleet::spawn_worker_for_item(&mut store, &item)?;
                    println!("Dispatched {} immediately (pid {pid})", item.key);
                }
            }
        }
    }
    Ok(())
}

/// Executes `tacho session` with support for `--clear` and fleet-wide default configuration.
fn handle_session(cli_args: SessionCliArgs) -> io::Result<()> {
    fleet::execute_session_with(fleet::SessionOptions {
        key: cli_args.key,
        brief: cli_args.brief,
        resume: cli_args.resume,
        clear: cli_args.clear,
        area: cli_args.area,
        base: cli_args.base,
        provider: cli_args.provider,
        model: cli_args.model,
        effort: cli_args.effort,
    })
}

/// Executes `tacho retire <key> [--why ...]` while recording custom `--why` reason.
fn handle_retire(args: RetireArgs) -> io::Result<()> {
    let custom_reason = args.reason.clone();
    let key = args.key.clone();
    fleet::execute_retire(args)?;
    if let Some(why) = custom_reason {
        let store = fleet::open_fleet_store()?;
        store.record_decision("dispatch", "retire_why", &key, &why)?;
    }
    Ok(())
}

/// Executes `tacho batch` with `--areas` and `--foreground` support.
fn handle_batch(cli_args: BatchCliArgs) -> io::Result<()> {
    let mut inner = cli_args.inner;
    if let Some(ref areas_csv) = cli_args.areas {
        let requested_areas: BTreeSet<String> = areas_csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if !requested_areas.is_empty() && inner.prs.is_empty() {
            let store = fleet::open_fleet_store()?;
            let state = store.load()?;
            let mut matched_prs = Vec::new();
            for pr in &state.open_prs_cache {
                let pr_areas =
                    tacho_core::decide::verify::areas_for_files(&pr.files, &state.area_rules);
                if pr_areas.iter().any(|a| requested_areas.contains(a)) {
                    matched_prs.push(pr.number);
                }
            }
            if !matched_prs.is_empty() {
                inner.prs = matched_prs;
            }
        }
    }
    fleet::execute_batch(inner)
}

/// Executes `tacho train` with positional `[prs]...` and `--max <N>` support.
fn handle_train(mut args: TrainArgs) -> io::Result<()> {
    if args.from_batch.is_none() && !args.prs.is_empty() {
        let mut store = fleet::open_fleet_store()?;
        let state = store.load()?;

        let mut target_prs = args.prs.clone();
        if let Some(max_n) = args.max {
            target_prs.truncate(max_n);
        }

        let matching_batch = state
            .batches
            .iter()
            .rev()
            .find(|b| b.status == "passed" && b.prs == target_prs)
            .cloned()
            .or_else(|| {
                let mut shas = std::collections::BTreeMap::new();
                let mut main_sha = String::new();
                let mut tree_sha = String::new();
                for &pr in &target_prs {
                    let rec = state.passed.get(&pr)?;
                    shas.insert(pr, rec.sha.clone());
                    if main_sha.is_empty() {
                        main_sha = rec.main_sha.clone();
                    }
                    tree_sha = rec.tree.clone();
                }
                let now = fleet::now_secs();
                Some(BatchRun {
                    id: format!("b{:03}", state.batches.len() + 1),
                    group: "manual".to_string(),
                    prs: target_prs.clone(),
                    shas,
                    main_sha,
                    areas: Vec::new(),
                    checks: Vec::new(),
                    mode: BatchMode::Presubmit,
                    status: "passed".to_string(),
                    started_at: now,
                    finished_at: Some(now),
                    tree: Some(tree_sha),
                    pid: None,
                    log: String::new(),
                    culprit: None,
                    skipped: Vec::new(),
                    flakes: Vec::new(),
                })
            });

        let Some(b) = matching_batch else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("PRs {:?} do not all have passed batch records", target_prs),
            ));
        };
        let tree = b.tree.clone().unwrap_or_else(|| b.main_sha.clone());
        let train = make_train(&state.trains, &b, &b.main_sha, &tree, fleet::now_secs())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let tid = train.id.clone();
        let because = format!("opened train {tid} for PRs {:?}", train.prs);
        store.mutate(Some(("land", "train_open", &tid, &because)), |st| {
            st.trains = with_train(&st.trains, train);
        })?;
        println!("{because}");
        return Ok(());
    }

    let max_cap = args.max.take();
    fleet::execute_train(args)?;
    if let Some(max_n) = max_cap {
        let mut store = fleet::open_fleet_store()?;
        store.mutate(None, |st| {
            if let Some(last) = st.trains.last_mut() {
                if last.status == "open" && last.prs.len() > max_n {
                    last.prs.truncate(max_n);
                }
            }
        })?;
    }
    Ok(())
}

/// Executes `tacho release-train [pr] [--approve]`.
fn handle_release_train(args: ReleaseTrainArgs) -> io::Result<()> {
    let pr_num = args.pr;
    fleet::execute_release_train(args)?;
    if let Some(num) = pr_num {
        let mut store = fleet::open_fleet_store()?;
        store.mutate(None, |st| {
            if let Some(ref mut tpr) = st.train_pr {
                tpr.number = Some(num);
            }
        })?;
    }
    Ok(())
}

/// Executes `tacho land [train] [--dry-run]`.
fn handle_land(args: LandArgs) -> io::Result<()> {
    if let Some(target_id) = args.train.as_ref().or(args.train_flag.as_ref()) {
        let store = fleet::open_fleet_store()?;
        let state = store.load()?;
        let found = state
            .trains
            .iter()
            .any(|t| t.id == *target_id && t.status == "open");
        if !found {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("open train '{target_id}' not found"),
            ));
        }
    }
    fleet::execute_land(args)
}

/// Executes `tacho pr` with `--shots-anyway` support.
fn handle_pr(args: PrArgs) -> io::Result<()> {
    if args.shots_anyway {
        std::env::set_var("TACHO_SHOTS_ANYWAY", "1");
    }
    kit::execute_pr(args)
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon(args) => {
            if let Err(e) = daemon::run_daemon(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Emit(args) => {
            if let Err(e) = emit::execute_emit(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Hook(args) => match args.provider.as_provider() {
            Some(provider) => {
                hook::execute_hook(provider);
                std::process::exit(0);
            }
            None if args.provider == HookTarget::Guard => {
                let code = execute_hook_guard();
                std::process::exit(code);
            }
            None => std::process::exit(0),
        },
        Commands::Run(args) => match run::execute_run(args.cmd) {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        },
        Commands::Trace(args) => {
            if let Err(e) = trace::execute_trace(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Status(args) => {
            if let Err(e) = status::execute_status(args) {
                exit_io_err(e);
            }
        }
        Commands::Needs(mut args) => {
            if args.section.is_empty() {
                args.section.push("needs".to_string());
            }
            if let Err(e) = status::execute_status(args) {
                exit_io_err(e);
            }
        }
        Commands::Version(args) => {
            if let Err(e) = fleet::execute_version(args) {
                exit_io_err(e);
            }
        }
        Commands::Setup(args) => {
            if let Err(e) = fleet::execute_setup(args) {
                exit_io_err(e);
            }
        }
        Commands::Db(args) => {
            if let Err(e) = execute_db(args) {
                exit_io_err(e);
            }
        }
        Commands::Report(args) => {
            if let Err(e) = status::execute_report(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Watch(args) => {
            if let Err(e) = status::execute_watch(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Query(args) => {
            if let Err(e) = query::execute_query(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Install(args) => {
            if let Err(e) = install::install_hooks(args.provider, args.user) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Uninstall(args) => {
            if let Err(e) = install::uninstall_hooks(args.provider, args.user) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::System(args) => {
            if let Err(e) = system::execute_system(args.command) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Tail(args) => {
            if let Err(e) = tail::execute_tail(args) {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        Commands::Start(args) => {
            if let Err(e) = kit::execute_start(args) {
                exit_io_err(e);
            }
        }
        Commands::Prove(args) => {
            if let Err(e) = kit::execute_prove(args) {
                exit_io_err(e);
            }
        }
        Commands::Job(args) => {
            if let Err(e) = kit::execute_job(args) {
                exit_io_err(e);
            }
        }
        Commands::Wait(args) => match kit::execute_wait(args) {
            Ok(code) => std::process::exit(code),
            Err(e) => exit_io_err(e),
        },
        Commands::Kill(args) => {
            if let Err(e) = kit::execute_kill(args) {
                exit_io_err(e);
            }
        }
        Commands::Pr(args) => {
            if let Err(e) = handle_pr(args) {
                exit_io_err(e);
            }
        }
        Commands::Note(args) => {
            if let Err(e) = kit::execute_note(args) {
                exit_io_err(e);
            }
        }
        Commands::Starve(args) => match kit::execute_starve(args) {
            Ok(code) => std::process::exit(code),
            Err(e) => exit_io_err(e),
        },
        Commands::Todo(args) => {
            if let Err(e) = handle_todo(args) {
                exit_io_err(e);
            }
        }
        Commands::Session(args) => {
            if let Err(e) = handle_session(args) {
                exit_io_err(e);
            }
        }
        Commands::Hold(args) => {
            if let Err(e) = fleet::execute_hold(args) {
                exit_io_err(e);
            }
        }
        Commands::Release(args) => {
            if let Err(e) = fleet::execute_release(args) {
                exit_io_err(e);
            }
        }
        Commands::Retire(args) => {
            if let Err(e) = handle_retire(args) {
                exit_io_err(e);
            }
        }
        Commands::Attach(args) => {
            if let Err(e) = fleet::execute_attach(args) {
                exit_io_err(e);
            }
        }
        Commands::Stop(args) => {
            if let Err(e) = fleet::execute_stop(args) {
                exit_io_err(e);
            }
        }
        Commands::Tell(args) => {
            if let Err(e) = fleet::execute_tell(args) {
                exit_io_err(e);
            }
        }
        Commands::Next(args) => {
            if let Err(e) = fleet::execute_next(args) {
                exit_io_err(e);
            }
        }
        Commands::Progress(args) => {
            if let Err(e) = fleet::execute_progress(args) {
                exit_io_err(e);
            }
        }
        Commands::Flakes(args) => {
            if let Err(e) = fleet::execute_flakes(args) {
                exit_io_err(e);
            }
        }
        Commands::Batch(args) => {
            if let Err(e) = handle_batch(args) {
                exit_io_err(e);
            }
        }
        Commands::Replay(args) | Commands::DecideReplay(args) => {
            if let Err(e) = fleet::execute_replay(args) {
                exit_io_err(e);
            }
        }
        Commands::Train(args) => {
            if let Err(e) = handle_train(args) {
                exit_io_err(e);
            }
        }
        Commands::ReleaseTrain(args) => {
            if let Err(e) = handle_release_train(args) {
                exit_io_err(e);
            }
        }
        Commands::Land(args) => {
            if let Err(e) = handle_land(args) {
                exit_io_err(e);
            }
        }
        Commands::Queue(args) => {
            if let Err(e) = fleet::execute_queue(args) {
                exit_io_err(e);
            }
        }
        Commands::Tidy(args) => {
            if let Err(e) = fleet::execute_tidy(args) {
                exit_io_err(e);
            }
        }
        Commands::Tick(args) => {
            if let Err(e) = fleet::execute_tick(args) {
                exit_io_err(e);
            }
        }
        Commands::Events(args) => {
            if let Err(e) = fleet::execute_events(args) {
                exit_io_err(e);
            }
        }
        Commands::Top(args) => {
            if let Err(e) = fleet::execute_top(args) {
                exit_io_err(e);
            }
        }
        Commands::Serve(args) => {
            if let Err(e) = fleet::execute_serve(args) {
                exit_io_err(e);
            }
        }
        Commands::Watchdog(args) => {
            if let Err(e) = fleet::execute_watchdog(args) {
                exit_io_err(e);
            }
        }
    }
}
