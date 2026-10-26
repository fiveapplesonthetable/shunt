//! The single plan executor: observe → `decide` → record → execute.
//!
//! Both the daemon (autonomous ticks, the `tick` API request) and the `tacho tick` CLI run
//! [`run_tick`]. There is one set of identity checks ([`identity`]), one observation path
//! ([`observe`]), one janitor ([`tidy`]), and one error policy:
//!
//! - Every action is recorded in the `decisions` journal with its reason (`because`), on
//!   success and on refusal or failure (`<action>_refused` / `<action>_failed`).
//! - A failed action never aborts the tick: it is recorded, an attention event is emitted, and
//!   the error is collected. After the plan's state is persisted the tick returns `Err` listing
//!   every failed action, so callers still learn about it. Only store (SQLite) errors abort.
//!
//! What differs between callers is only what the process can physically do, behind [`Hands`]:
//! starting provider sessions and delivering tells through a provider (CLI) versus queueing
//! for a spawning executor and writing the inbox (daemon).
//!
//! Ticks are serialized in-process ([`TICK_LOCK`]) and across processes (an `flock` on
//! `$TACHO_HOME/decide.lock`), so a CLI tick and a daemon tick never execute two plans
//! computed from the same state.

pub mod identity;
pub mod observe;
pub mod tidy;

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tacho_core::decide::dispatch::{base_key, resume_task, AgentRun, TodoItem};
use tacho_core::decide::watch::{idle_chains, ProcEntry, SuperviseAction};
use tacho_core::decide::{DecisionPlan, DecisionState, WorldObservation};
use tacho_core::TaskStatus;

use crate::fleet::{FleetState, FleetStore};
use identity::{AgentProcess, Identity, Signalled};

/// Error kind a [`Hands::deliver_tell`] returns when delivering would resume a session whose
/// old process may still own the worktree. Recorded as `tell_refused`, not as a failure.
pub const RESUME_REFUSED: io::ErrorKind = io::ErrorKind::ResourceBusy;

/// What a caller of [`run_tick`] can physically do.
pub trait Hands {
    /// Starts a worker for a dispatched item and records it (`dispatch/start`).
    /// `Ok(None)`: this process cannot start workers; the item stays queued.
    fn spawn_worker(&mut self, store: &mut FleetStore, item: &TodoItem) -> io::Result<Option<u32>>;

    /// Delivers a decided message to an agent and records the delivery.
    fn deliver_tell(
        &mut self,
        store: &mut FleetStore,
        agent_key: &str,
        msg: &str,
    ) -> io::Result<()>;

    /// Emits an instant trace event (e.g. `watch:attention`).
    fn emit(&mut self, track: &str, name: &str, because: &str);

    /// Emits a duration slice on `track_key`.
    fn emit_duration_slice(
        &mut self,
        store: &FleetStore,
        track_key: &str,
        name: &str,
        start_ts_ns: u64,
        end_ts_ns: u64,
        because: &str,
    ) -> io::Result<()> {
        store.emit_duration_slice(track_key, name, start_ts_ns, end_ts_ns, because)
    }

    /// Called once after a non-dry-run tick persisted its state.
    fn publish(&mut self, _store: &FleetStore) -> io::Result<()> {
        Ok(())
    }

    /// Delivers an owner notification (`subject`, `body`).
    fn notify_owner(&mut self, subject: &str, body: &str) -> Result<bool, String> {
        tacho_core::decide::plugin::notify_owner_command(subject, body)
    }
}

/// Inputs of one tick besides fleet state.
pub struct TickOptions<'a> {
    pub dry_run: bool,
    /// Caller-supplied main sha (wins over everything else).
    pub explicit_main_sha: Option<String>,
    /// Checkout the tick runs against; `None` = unknown.
    pub repo: Option<PathBuf>,
    pub live_state: &'a tacho_core::LiveState,
    /// Root pid of each recorder-tracked run, by agent key.
    pub run_pids: BTreeMap<String, u32>,
    /// Process table to use; `None` scans `/proc`.
    pub procs: Option<Vec<ProcEntry>>,
    /// Optional forge plugin to refresh `open_prs`, `main_ci`, and remote `main_sha` before deciding.
    pub forge: Option<&'a dyn tacho_core::decide::plugin::ForgePlugin>,
    /// Timeout for each bounded forge/git refresh command (default 10s).
    pub refresh_timeout: std::time::Duration,
    /// Optional explicit epoch timestamp (seconds) for deterministic testing without sleeping.
    pub now_override: Option<i64>,
    /// Optional explicit `(total_bytes, avail_bytes)` of `/` for root-disk checks.
    pub root_disk_bytes: Option<(u64, u64)>,
}

impl<'a> TickOptions<'a> {
    pub fn new(live_state: &'a tacho_core::LiveState) -> Self {
        Self {
            dry_run: false,
            explicit_main_sha: None,
            repo: None,
            live_state,
            run_pids: BTreeMap::new(),
            procs: None,
            forge: None,
            refresh_timeout: std::time::Duration::from_secs(10),
            now_override: None,
            root_disk_bytes: None,
        }
    }
}

/// Result of one tick.
#[derive(Debug, Clone)]
pub struct TickOutcome {
    /// Recorded tick id; the previous id when this tick duplicated it; `None` on dry run.
    pub tick_id: Option<i64>,
    /// `true` when inputs and plan equalled the last recorded tick and nothing was executed.
    pub deduped: bool,
    pub state: DecisionState,
    pub obs: WorldObservation,
    pub plan: DecisionPlan,
}

/// The single owner of tick execution inside a process.
static TICK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Test-only overlap detector: counts ticks in flight and records the most ever seen at once.
#[cfg(test)]
pub(crate) mod tick_probe {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static MAX_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

    pub(crate) struct InFlight;

    pub(crate) fn enter() -> InFlight {
        let now = IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_IN_FLIGHT.fetch_max(now, Ordering::SeqCst);
        InFlight
    }

    impl Drop for InFlight {
        fn drop(&mut self) {
            IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Cross-process tick lock: a blocking `flock` on `$TACHO_HOME/decide.lock`.
struct HomeLock(std::fs::File);

impl HomeLock {
    fn acquire(home: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(home)?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(home.join("decide.lock"))?;
        loop {
            // SAFETY: flock on an fd we own; blocks in the kernel until the lock is free.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self(f));
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

impl Drop for HomeLock {
    fn drop(&mut self) {
        // SAFETY: unlocking an fd we own. Closing the fd releases the lock regardless, so a
        // failed unlock is only reported.
        if unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) } != 0 {
            eprintln!(
                "[tacho] failed to unlock decide.lock: {}",
                io::Error::last_os_error()
            );
        }
    }
}

/// Collects failed actions under the shared error policy.
struct Failures<'h> {
    hands: &'h mut dyn Hands,
    errors: Vec<String>,
}

impl Failures<'_> {
    /// Records a refused or failed action with its reason, emits attention, and (for
    /// failures) remembers it for the tick's result.
    fn record(
        &mut self,
        store: &FleetStore,
        category: &str,
        action: &str,
        target: &str,
        because: &str,
        is_failure: bool,
    ) -> io::Result<()> {
        store.record_decision(category, action, target, because)?;
        self.hands
            .emit(&attention_track(target), "watch:attention", because);
        eprintln!("[tacho] {because}");
        if is_failure {
            self.errors.push(format!("{action} {target}: {because}"));
        }
        Ok(())
    }
}

fn attention_track(target: &str) -> String {
    if target.starts_with("pid:") || target.is_empty() {
        "system".to_string()
    } else {
        format!("agent:{target}")
    }
}

/// Current epoch time in seconds.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Main sha for the tick: explicit, else the refreshed remote main, else the checkout's local
/// main, else the newest sha fleet state knows. Empty when nothing is known.
fn resolve_main_sha(state: &FleetState, opts: &TickOptions<'_>, home: &Path) -> String {
    let non_empty = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    opts.explicit_main_sha
        .as_deref()
        .and_then(non_empty)
        .or_else(|| state.main_sha.as_deref().and_then(non_empty))
        .or_else(|| opts.repo.as_deref().and_then(observe::local_main_sha))
        .or_else(|| crate::fleet::known_main_sha(home, state, opts.live_state))
        .unwrap_or_default()
}

/// Observes, decides, records, and (unless `dry_run`) executes one tick. See module docs.
pub fn run_tick(
    home: &Path,
    store: &mut FleetStore,
    hands: &mut dyn Hands,
    opts: TickOptions<'_>,
) -> io::Result<TickOutcome> {
    // A tick that panicked left no lock-protected data behind (the guard protects `()`).
    let _serialized = TICK_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _home_lock = HomeLock::acquire(home)?;
    #[cfg(test)]
    let _in_flight = tick_probe::enter();

    let tick_start_ns = now_ns();
    let mut refresh_errors = Vec::new();
    if !opts.dry_run {
        if let Some(forge) = opts.forge {
            let outcome = observe::refresh_world(
                store,
                forge,
                opts.repo.as_deref(),
                hands,
                opts.refresh_timeout,
            )?;
            refresh_errors = outcome.errors;
        }
    }
    let state = store.load()?;
    let now = opts.now_override.unwrap_or_else(now_secs);
    let procs = match opts.procs {
        Some(ref p) => p.clone(),
        None => observe::scan_procs(),
    };
    let main_sha = resolve_main_sha(&state, &opts, home);
    let root_disk_bytes = opts.root_disk_bytes.or_else(|| {
        if opts.procs.is_none() && !tacho_core::decide::plugin::running_in_test_harness() {
            observe::statvfs_root_disk()
        } else {
            None
        }
    });
    let host = observe::HostObs {
        now,
        live_state: opts.live_state,
        procs: &procs,
        run_pids: opts.run_pids.clone(),
        repo: opts.repo.as_deref(),
        main_sha,
        root_disk_bytes,
    };
    let (decision_state, world_obs) = observe::observe(&state, &host);
    let plan = tacho_core::decide::decide(&decision_state, &world_obs);

    if opts.dry_run {
        print_dry_run(&plan);
        return Ok(TickOutcome {
            tick_id: None,
            deduped: false,
            state: decision_state,
            obs: world_obs,
            plan,
        });
    }

    // Same inputs and same decision as the last recorded tick: nothing new to record or
    // execute. Point at the tick this one would duplicate (unless forge refresh failed, which
    // must surface as an error so the sampler backs off).
    if let Some(prev) = store.unchanged_since_latest_tick(&decision_state, &world_obs, &plan)? {
        if !refresh_errors.is_empty() {
            return Err(io::Error::other(format!(
                "tick {prev}: {} forge refresh error(s): {}",
                refresh_errors.len(),
                refresh_errors.join("; ")
            )));
        }
        return Ok(TickOutcome {
            tick_id: Some(prev),
            deduped: true,
            state: decision_state,
            obs: world_obs,
            plan,
        });
    }
    let tick_id = store.record_decision_tick(now, &decision_state, &world_obs, &plan)?;

    persist_plan_state(store, &plan, now)?;

    let mut f = Failures {
        hands,
        errors: refresh_errors,
    };
    execute_actions(
        home, store, &mut f, &state, &world_obs, &plan, opts.forge, now,
    )?;

    let summary = format!(
        "active={}, dispatched={}, reaped={}",
        world_obs.live_keys.len(),
        plan.dispatched_tasks.len(),
        plan.reaped_agents.len()
    );
    f.hands.emit_duration_slice(
        store,
        "fleet/ticks",
        "tick",
        tick_start_ns,
        now_ns(),
        &summary,
    )?;
    if let Err(e) = f.hands.publish(store) {
        f.errors.push(format!("publish: {e}"));
    }
    println!("Tick complete at {now} ({summary})");

    if !f.errors.is_empty() {
        return Err(io::Error::other(format!(
            "tick {tick_id}: {} action(s) failed: {}",
            f.errors.len(),
            f.errors.join("; ")
        )));
    }
    Ok(TickOutcome {
        tick_id: Some(tick_id),
        deduped: false,
        state: decision_state,
        obs: world_obs,
        plan,
    })
}

fn print_dry_run(plan: &DecisionPlan) {
    for id in &plan.duplicate_batch_aborts {
        println!("would abort duplicate batch {id}");
    }
    for b in &plan.burner_kills {
        println!(
            "[dry-run] would kill runaway burner pid={} ({}) elapsed={}s",
            b.pid, b.comm, b.elapsed_secs
        );
    }
    for step in &plan.supervision_actions {
        match step {
            SuperviseAction::Ok => {}
            SuperviseAction::Resume { because, .. }
            | SuperviseAction::Nudge { because, .. }
            | SuperviseAction::Escalate { because, .. } => println!("Supervise: {because}"),
        }
    }
    for item in &plan.dispatched_tasks {
        println!("[dry-run] Would dispatch {}", item.key);
    }
    for d in &plan.tidy_decisions {
        println!("[dry-run] {d:?}");
    }
    if plan.self_update.target_sha.is_some() {
        println!("Self-update: {}", plan.self_update.because);
    }
}

/// Fix and rebase hand-off tasks the plan asks for, appended to `todo` when not queued yet.
fn handoff_tasks(plan: &DecisionPlan, mut todo: Vec<TodoItem>, now: i64) -> Vec<TodoItem> {
    for (pr, rec) in &plan.fix_decisions.dispatch {
        let key = format!("fix-pr{pr}");
        if !todo.iter().any(|t| t.key == key) {
            todo.push(TodoItem {
                key,
                brief: format!(
                    "Fix red PR #{pr} at {} ({}):\n{}",
                    rec.sha, rec.summary, rec.log
                ),
                base: Some(pr.to_string()),
                priority: 1,
                created_at: now,
                ..Default::default()
            });
        }
    }
    for (pr, head) in &plan.rebase_handoffs {
        let key = format!("rebase-pr{pr}");
        if !todo.iter().any(|t| t.key == key) {
            todo.push(TodoItem {
                key,
                brief: format!("Rebase conflicting PR #{pr} at {head} onto main"),
                base: Some(pr.to_string()),
                priority: 1,
                created_at: now,
                ..Default::default()
            });
        }
    }
    todo
}

/// Persists `decide`'s next state. Runs before any action so actions that mutate the store
/// (spawns, resumes, aborts, worktree removal) are never overwritten by it.
fn persist_plan_state(store: &mut FleetStore, plan: &DecisionPlan, now: i64) -> io::Result<()> {
    let next = &plan.next_state;
    let mut todo = plan.dispatched_tasks.clone();
    for item in handoff_tasks(plan, next.todo.clone(), now) {
        if !todo.iter().any(|t| t.key == item.key) {
            todo.push(item);
        }
    }
    let reaped = !plan.reaped_agents.is_empty();
    store.mutate(None, |st| {
        st.todo = todo;
        st.agents = next.agents.clone();
        if reaped {
            // Exited lineages whose worktrees hold nothing more are finished without a PR.
            let idle = idle_chains(
                &st.agents,
                &st.retired,
                &BTreeSet::new(),
                &Default::default(),
            );
            for root in idle {
                if let Some(a) = st.agents.get_mut(&root) {
                    a.pr_none = true;
                }
            }
        }
        st.retired.extend(next.retired.iter().cloned());
        for (k, v) in &next.holds {
            st.holds.entry(k.clone()).or_insert_with(|| v.clone());
        }
        st.passed = next.passed.clone();
        st.failed = next.failed.clone();
        st.merged_prs = next.merged_prs.clone();
        st.closed_prs = next.closed_prs.clone();
        st.batches = next.batches.clone();
        st.batches.retain(|b| {
            !plan
                .cleared_main_broken
                .iter()
                .any(|c| c.batch_id == b.id && b.status == "main-broken")
        });
        st.flakes = next.flakes.clone();
        st.samples = next.samples.clone();
        st.nudged = next.nudged.clone();
        st.rebase_attempts = next.rebase_attempts.clone();
        st.fix_notified = next.fix_notified.clone();
        st.notified_failures = next.notified_keys.clone();
        st.inbox = next.inbox.clone();
        st.last_tick_ts = next.last_tick_ts;
        st.last_watchdog_alert_ts = next.last_watchdog_alert_ts;
        if let Some(ci) =
            observe::env_json::<tacho_core::decide::ops::snapshot::MainCiRun>("TACHO_MAIN_CI_JSON")
        {
            st.main_ci = Some(ci);
        }
    })
}

#[derive(Deserialize, Default)]
struct BatchIdentities {
    #[serde(default)]
    batch_identities: BTreeMap<String, RecordedIdentity>,
}

#[derive(Deserialize, Default, Clone)]
struct RecordedIdentity {
    pid: u32,
    #[serde(default)]
    starttime: Option<u64>,
    #[serde(default)]
    cgroup: Option<String>,
}

/// Recorded batch process identities (`$TACHO_HOME/fleet_meta.json`, written when a batch
/// is launched).
fn batch_identities(home: &Path) -> BTreeMap<String, RecordedIdentity> {
    std::fs::read_to_string(home.join("fleet_meta.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<BatchIdentities>(&raw).ok())
        .unwrap_or_default()
        .batch_identities
}

#[allow(clippy::too_many_arguments)]
fn execute_actions(
    home: &Path,
    store: &mut FleetStore,
    f: &mut Failures<'_>,
    state: &FleetState,
    obs: &WorldObservation,
    plan: &DecisionPlan,
    forge: Option<&dyn tacho_core::decide::plugin::ForgePlugin>,
    now: i64,
) -> io::Result<()> {
    // Duplicate batches: abort all but the oldest, signalling only verified processes.
    if !plan.duplicate_batch_aborts.is_empty() {
        let identities = batch_identities(home);
        for dup in &plan.duplicate_batch_aborts {
            let pid = state
                .batches
                .iter()
                .find(|b| &b.id == dup)
                .and_then(|b| b.pid);
            let outcome = match pid {
                // No process recorded: nothing runs, so only the record is aborted.
                None => Ok(Signalled::Gone),
                Some(pid) => {
                    let rec = identities.get(dup).filter(|r| r.pid == pid);
                    let id = Identity {
                        pid,
                        starttime: rec.and_then(|r| r.starttime),
                        cgroup: rec.and_then(|r| r.cgroup.as_deref()),
                    };
                    identity::signal_verified(&id, dup, libc::SIGTERM)
                }
            };
            match outcome {
                Ok(Signalled::Delivered | Signalled::Gone) => {
                    let because = format!("aborted duplicate batch {dup}");
                    store.mutate(
                        Some(("watch", "kill_duplicate_batch", dup, &because)),
                        |st| {
                            for b in st.batches.iter_mut().filter(|b| &b.id == dup) {
                                b.status = "aborted".to_string();
                                b.finished_at = Some(now);
                            }
                        },
                    )?;
                    println!("{because}");
                }
                Ok(Signalled::Refused) => f.record(
                    store,
                    "watch",
                    "kill_duplicate_batch_refused",
                    dup,
                    &format!("refused kill: no identity for duplicate batch {dup}"),
                    false,
                )?,
                Err(e) => f.record(
                    store,
                    "watch",
                    "kill_duplicate_batch_failed",
                    dup,
                    &format!("failed to abort duplicate batch {dup}: {e}"),
                    true,
                )?,
            }
        }
    }

    // Runaway burners.
    for b in &plan.burner_kills {
        let target = format!("pid:{}", b.pid);
        let id = Identity {
            pid: b.pid,
            starttime: b.starttime,
            cgroup: None,
        };
        match identity::signal_verified(&id, &b.comm, libc::SIGKILL) {
            Ok(Signalled::Delivered) => {
                let because = format!(
                    "killed runaway burner pid={} ({}) elapsed={}s",
                    b.pid, b.comm, b.elapsed_secs
                );
                store.record_decision("watch", "kill_burner", &target, &because)?;
                f.hands.emit("system", "watch:kill_burner", &because);
                println!("{because}");
            }
            Ok(Signalled::Gone) => store.record_decision(
                "watch",
                "kill_burner_gone",
                &target,
                &format!("burner pid={} ({}) exited before kill", b.pid, b.comm),
            )?,
            Ok(Signalled::Refused) => f.record(
                store,
                "watch",
                "kill_burner_refused",
                &target,
                &format!(
                    "refused kill of burner pid={} ({}): identity not verified",
                    b.pid, b.comm
                ),
                false,
            )?,
            Err(e) => f.record(
                store,
                "watch",
                "kill_burner_failed",
                &target,
                &format!("failed to kill burner pid={} ({}): {e}", b.pid, b.comm),
                true,
            )?,
        }
    }

    // Decided messages.
    for (key, msg) in &plan.tell_actions {
        deliver_tell(store, f, key, msg)?;
    }

    // Reaped agents: task state bands.
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
        store.record_decision("dispatch", "reap_agent", k, &because)?;
        store.emit_state_band(
            &format!("task:{}/state", base_key(k)),
            label,
            status,
            &because,
            now_ns(),
        )?;
    }

    // Supervision.
    for step in &plan.supervision_actions {
        match step {
            SuperviseAction::Ok => {}
            SuperviseAction::Resume { key, because } => {
                println!("Supervise: {because}");
                if let Some(a) = state.agents.get(key) {
                    supervise_resume(store, f, a, because, now)?;
                } else {
                    store.record_decision("watch", "supervise", key, because)?;
                }
                f.hands
                    .emit(&format!("agent:{key}"), "watch:supervise", because);
            }
            SuperviseAction::Nudge { key, because } => {
                println!("Supervise: {because}");
                store.record_decision("watch", "supervise_nudge", key, because)?;
                f.hands
                    .emit(&format!("agent:{key}"), "watch:supervise", because);
            }
            SuperviseAction::Escalate { key, because } => {
                println!("Supervise: {because}");
                if obs.main_checkout_workers.contains_key(key) {
                    if let Some(a) = state.agents.get(key) {
                        if let Some(pid) = a.pid {
                            let id = Identity {
                                pid,
                                starttime: a.starttime,
                                cgroup: a.cgroup.as_deref(),
                            };
                            match identity::signal_verified(&id, key, libc::SIGTERM) {
                                Ok(Signalled::Delivered) => {
                                    // Also signal the verified leader's process group so its children die too.
                                    let _ = identity::send_signal(-(pid as i32), libc::SIGTERM);
                                    store.mutate(
                                        Some(("watch", "supervise", key, because)),
                                        |st| {
                                            if let Some(ag) = st.agents.get_mut(key) {
                                                ag.finished_at.get_or_insert(now);
                                                ag.exit_code.get_or_insert(143);
                                                ag.status_note =
                                                    "stopped: directory resolves to main checkout"
                                                        .to_string();
                                            }
                                            st.holds = tacho_core::decide::dispatch::with_hold(
                                                &st.holds,
                                                base_key(key),
                                                because,
                                                now,
                                            );
                                        },
                                    )?;
                                }
                                Ok(Signalled::Gone) => {
                                    store.mutate(
                                        Some(("watch", "supervise", key, because)),
                                        |st| {
                                            if let Some(ag) = st.agents.get_mut(key) {
                                                ag.finished_at.get_or_insert(now);
                                                ag.exit_code.get_or_insert(143);
                                            }
                                            st.holds = tacho_core::decide::dispatch::with_hold(
                                                &st.holds,
                                                base_key(key),
                                                because,
                                                now,
                                            );
                                        },
                                    )?;
                                }
                                Ok(Signalled::Refused) => {
                                    f.record(
                                        store,
                                        "watch",
                                        "supervise_stop_refused",
                                        key,
                                        &format!(
                                            "refused to stop main-checkout worker {key}: identity not verified"
                                        ),
                                        false,
                                    )?;
                                }
                                Err(e) => {
                                    f.record(
                                        store,
                                        "watch",
                                        "supervise_stop_failed",
                                        key,
                                        &format!("failed to stop main-checkout worker {key}: {e}"),
                                        true,
                                    )?;
                                }
                            }
                        } else {
                            store.record_decision("watch", "supervise", key, because)?;
                        }
                    } else {
                        store.record_decision("watch", "supervise", key, because)?;
                    }
                } else {
                    // Ordinary stall escalation: notify the owner (via plan.owner_notifications)
                    // without killing the worker or holding the task.
                    store.record_decision("watch", "supervise_escalate", key, because)?;
                }
                f.hands
                    .emit(&format!("agent:{key}"), "watch:supervise", because);
            }
        }
    }

    for item in &plan.resumed_tasks {
        let because = format!("auto-queued resume {} after backoff", item.key);
        println!("{because}");
        store.record_decision("dispatch", "auto_resume", &item.key, &because)?;
    }
    for (pr, rec) in &plan.fix_decisions.dispatch {
        store.record_decision(
            "verify",
            "queue_fix",
            &format!("fix-pr{pr}"),
            &format!("PR #{pr} is red at {}: {}", rec.sha, rec.summary),
        )?;
    }
    for (pr, head) in &plan.rebase_handoffs {
        store.record_decision(
            "dispatch",
            "queue_rebase",
            &format!("rebase-pr{pr}"),
            &format!("PR #{pr} conflicts with main at {head}"),
        )?;
    }
    for c in &plan.cleared_main_broken {
        store.record_decision("verify", "clear_main_broken", &c.batch_id, &c.reason)?;
    }
    if !plan.cleared_main_broken.is_empty()
        && !plan
            .next_state
            .batches
            .iter()
            .any(|b| b.status == "main-broken")
    {
        if let Some(forge) = forge {
            let reason = plan
                .cleared_main_broken
                .iter()
                .map(|c| c.reason.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            if let Err(e) = forge.close_issue("main is red", &reason) {
                f.record(
                    store,
                    "watch",
                    "close_issue_failed",
                    "main is red",
                    &format!("failed to close 'main is red' issue: {e}"),
                    true,
                )?;
            }
        }
    }

    // Dispatch.
    for item in &plan.dispatched_tasks {
        match f.hands.spawn_worker(store, item) {
            Ok(Some(pid)) => println!("Dispatched {} (pid {pid})", item.key),
            Ok(None) => store.record_decision(
                "dispatch",
                "dispatch_queued",
                &item.key,
                "decided to dispatch; left queued for an executor that can start workers",
            )?,
            Err(e) => f.record(
                store,
                "dispatch",
                "start_failed",
                &item.key,
                &format!("failed to start {}: {e}", item.key),
                true,
            )?,
        }
    }

    // Janitor.
    if !plan.tidy_decisions.is_empty() {
        tidy::apply_tidy_decisions(store, &plan.tidy_decisions, now, false)?;
    }

    if let Some(ref disk) = plan.root_disk_action {
        if disk.prune_docker_volumes {
            store.record_decision(
                "watch",
                "prune_root_disk_volumes",
                "root-disk",
                &disk.summary,
            )?;
        }
    }

    if let Some(target) = &plan.self_update.target_sha {
        println!("Self-update: {}", plan.self_update.because);
        store.record_decision("watch", "self_update", target, &plan.self_update.because)?;
    }
    for n in &plan.owner_notifications {
        let mut notify_failed = false;
        if let Err(e) = f.hands.notify_owner(&n.subject, &n.body) {
            notify_failed = true;
            f.record(
                store,
                "watch",
                "notify_owner_failed",
                &n.key,
                &format!("failed to notify owner for {}: {e}", n.key),
                true,
            )?;
        }
        if let Some(forge) = forge {
            let issue_title = if n.kind == "main_red" || n.kind == "main-red" {
                "main is red".to_string()
            } else {
                format!("tacho alert: {}", n.key)
            };
            if let Err(e) = forge.set_issue(&issue_title, &n.body) {
                notify_failed = true;
                f.record(
                    store,
                    "watch",
                    "set_issue_failed",
                    &n.key,
                    &format!("failed to set issue '{issue_title}' for {}: {e}", n.key),
                    true,
                )?;
            }
        }
        if notify_failed {
            store.mutate(None, |st| {
                st.notified_failures.remove(&n.key);
            })?;
        } else {
            store.record_decision("watch", "notify_owner", &n.key, &n.because)?;
        }
    }
    if let Some(alert) = &plan.watchdog_alert {
        let mut alert_failed = false;
        if let Err(e) = f.hands.notify_owner("tacho is down", alert) {
            alert_failed = true;
            f.record(
                store,
                "watch",
                "watchdog_alert_failed",
                "tick",
                &format!("failed to notify owner of watchdog alert: {e}"),
                true,
            )?;
        }
        if let Some(forge) = forge {
            if let Err(e) = forge.set_issue("tacho is down", alert) {
                alert_failed = true;
                f.record(
                    store,
                    "watch",
                    "watchdog_alert_failed",
                    "tick",
                    &format!("failed to set 'tacho is down' issue: {e}"),
                    true,
                )?;
            }
        }
        if alert_failed {
            let prev_ts = state.last_watchdog_alert_ts;
            store.mutate(None, |st| {
                st.last_watchdog_alert_ts = prev_ts;
            })?;
        } else {
            store.record_decision("watch", "watchdog_alert", "tick", alert)?;
        }
    }
    Ok(())
}

/// Delivers a tell; a refused resume (old process still owns the worktree) is recorded as
/// `tell_refused`, any other failure as `tell_failed`.
fn deliver_tell(
    store: &mut FleetStore,
    f: &mut Failures<'_>,
    key: &str,
    msg: &str,
) -> io::Result<()> {
    match f.hands.deliver_tell(store, key, msg) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == RESUME_REFUSED => {
            f.record(store, "watch", "tell_refused", key, &e.to_string(), false)
        }
        Err(e) => f.record(
            store,
            "watch",
            "tell_failed",
            key,
            &format!("failed to deliver message to {key}: {e}"),
            true,
        ),
    }
}

/// Carries out a supervisor `Resume` without ever restarting a running agent.
///
/// - Old process verified gone: queue a resume of the session.
/// - Verified and stopped: kill it, wait for its exit event, then queue the resume.
/// - Verified and running: deliver the guidance through the tell path.
/// - Identity unverified: refuse and record why; state is left untouched.
fn supervise_resume(
    store: &mut FleetStore,
    f: &mut Failures<'_>,
    a: &AgentRun,
    because: &str,
    now: i64,
) -> io::Result<()> {
    let key = a.key.as_str();
    let exit_code =
        match identity::classify_agent_process(a.pid, key, a.cgroup.as_deref(), a.starttime) {
            AgentProcess::Gone => a.exit_code,
            AgentProcess::Stopped(pid) => match identity::kill_stopped_and_wait(pid) {
                Ok(()) => Some(128 + libc::SIGKILL),
                Err(e) => {
                    return f.record(
                        store,
                        "watch",
                        "supervise_resume_failed",
                        key,
                        &format!("failed to stop {key} before resume: {e}"),
                        true,
                    )
                }
            },
            AgentProcess::Running => return deliver_tell(store, f, key, because),
            AgentProcess::Unverified => {
                return f.record(
                    store,
                    "watch",
                    "supervise_resume_refused",
                    key,
                    &format!("refused resume of {key}: no identity for pid {:?}", a.pid),
                    false,
                )
            }
        };
    let resumed = resume_task(a, because, "", now);
    store.mutate(Some(("watch", "supervise_resume", key, because)), |st| {
        if let Some(ag) = st.agents.get_mut(key) {
            ag.finished_at.get_or_insert(now);
            ag.exit_code = exit_code;
        }
        st.todo.push(resumed);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::DaemonHands;
    use tacho_core::decide::dispatch::{AgentRun, PullFacts};
    use tacho_core::decide::ops::snapshot::MainCiRun;
    use tacho_core::decide::plugin::MockForge;
    use tacho_core::decide::verify::{BatchMode, BatchRun};
    use tacho_core::decide::watch::{HUNG_SECS, STALLED_SECS};

    #[test]
    fn test_run_tick_persists_real_progress_samples_and_replays_identically() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();

        let my_pid = std::process::id();
        let my_st = identity::proc_pid_starttime(my_pid).expect("own starttime");
        store
            .mutate(None, |st| {
                st.agents.insert(
                    "worker-1".to_string(),
                    AgentRun {
                        key: "worker-1".to_string(),
                        pid: Some(my_pid),
                        starttime: Some(my_st),
                        started_at: 1_000,
                        ..Default::default()
                    },
                );
            })
            .unwrap();

        let live = tacho_core::LiveState::default();
        let mut hands = DaemonHands {
            home: home.to_path_buf(),
            live_state: live.clone(),
            appender: None,
        };

        // Tick 1 at t = 1000: first sample is recorded; agent is Moving (0 supervision actions).
        let mut opts1 = TickOptions::new(&live);
        opts1.now_override = Some(1_000);
        opts1.explicit_main_sha = Some("a".repeat(40));
        opts1.procs = Some(vec![ProcEntry {
            pid: my_pid,
            ppid: 1,
            pgid: my_pid,
            state: "S".to_string(),
            cpu_ticks: 100,
            ..Default::default()
        }]);
        let out1 = run_tick(home, &mut store, &mut hands, opts1).unwrap();
        assert!(out1.plan.supervision_actions.is_empty());
        assert_eq!(store.load().unwrap().samples["worker-1"].len(), 1);

        // Tick 2 at t = 1000 + STALLED_SECS + 100 with a busy child process (10 ticks/s >= 5 ticks/s):
        // must NOT be marked stalled!
        let mut opts2 = TickOptions::new(&live);
        let t2 = 1_000 + STALLED_SECS + 100;
        opts2.now_override = Some(t2);
        opts2.explicit_main_sha = Some("a".repeat(40));
        opts2.procs = Some(vec![
            ProcEntry {
                pid: my_pid,
                ppid: 1,
                pgid: my_pid,
                state: "S".to_string(),
                cpu_ticks: 100,
                ..Default::default()
            },
            ProcEntry {
                pid: my_pid.saturating_add(99_999),
                ppid: my_pid,
                pgid: my_pid,
                state: "R".to_string(),
                cpu_ticks: ((t2 - 1_000) as u64) * 10,
                ..Default::default()
            },
        ]);
        let out2 = run_tick(home, &mut store, &mut hands, opts2).unwrap();
        assert!(
            out2.plan.supervision_actions.is_empty(),
            "busy child process must prevent stall verdict, got: {:?}",
            out2.plan.supervision_actions
        );

        // Tick 3 at t2 + STALLED_SECS + 100 where child is gone and root CPU only grew by 1 tick:
        // now idle for >= STALLED_SECS -> Stalled -> Nudge!
        let t3 = t2 + STALLED_SECS + 100;
        let mut opts3 = TickOptions::new(&live);
        opts3.now_override = Some(t3);
        opts3.explicit_main_sha = Some("a".repeat(40));
        opts3.procs = Some(vec![ProcEntry {
            pid: my_pid,
            ppid: 1,
            pgid: my_pid,
            state: "S".to_string(),
            cpu_ticks: 100 + ((t2 - 1_000) as u64) * 10 + 1,
            ..Default::default()
        }]);
        let out3 = run_tick(home, &mut store, &mut hands, opts3).unwrap();
        assert!(
            out3.plan
                .supervision_actions
                .iter()
                .any(|s| matches!(s, SuperviseAction::Nudge { key, .. } if key == "worker-1")),
            "expected Nudge after STALLED_SECS of idle CPU, got: {:?}",
            out3.plan.supervision_actions
        );

        // Deterministic replay of the recorded decision tick from SQLite must produce the exact plan!
        let tick_id = out3.tick_id.expect("recorded tick_id");
        let (replayed_state, replayed_obs, logged_plan) =
            store.load_decision_tick(tick_id).unwrap();
        let replayed_plan = tacho_core::decide::decide(&replayed_state, &replayed_obs);
        assert_eq!(replayed_plan, logged_plan);
        let _ = HUNG_SECS;
    }

    #[test]
    fn test_run_tick_with_mock_forge_refreshes_world_and_clears_main_broken() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run_git_cmd = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        run_git_cmd(&["init", "-q", "-b", "main"]);
        run_git_cmd(&["config", "user.email", "t@example.com"]);
        run_git_cmd(&["config", "user.name", "Tester"]);
        std::fs::write(repo.join("f.txt"), "v1\n").unwrap();
        run_git_cmd(&["add", "f.txt"]);
        run_git_cmd(&["commit", "-q", "-m", "c1"]);
        let sha1 = observe::local_main_sha(&repo).unwrap();

        std::fs::write(repo.join("f.txt"), "v2\n").unwrap();
        run_git_cmd(&["add", "f.txt"]);
        run_git_cmd(&["commit", "-q", "-m", "c2"]);
        let sha2 = observe::local_main_sha(&repo).unwrap();

        let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
        store
            .mutate(None, |st| {
                st.batches.push(BatchRun {
                    id: "b-main-broken".to_string(),
                    group: "core".to_string(),
                    prs: vec![55],
                    shas: BTreeMap::from([(55, sha1.clone())]),
                    main_sha: sha1.clone(),
                    areas: vec!["core".to_string()],
                    checks: Vec::new(),
                    mode: BatchMode::Presubmit,
                    status: "main-broken".to_string(),
                    started_at: 100,
                    finished_at: Some(120),
                    tree: None,
                    pid: None,
                    log: String::new(),
                    culprit: None,
                    skipped: Vec::new(),
                    flakes: Vec::new(),
                });
            })
            .unwrap();

        let forge = MockForge {
            main_ci: Ok(Some(MainCiRun {
                id: 501,
                status: "completed".to_string(),
                conclusion: "success".to_string(),
                sha: sha2.clone(),
                title: "CI".to_string(),
                age_seconds: None,
                completed_at: "2026-10-25T13:00:00Z".to_string(),
            })),
            open_prs: Ok(vec![PullFacts {
                number: 55,
                title: "Fix core".to_string(),
                branch: "feat/55".to_string(),
                base: "main".to_string(),
                head: sha1,
                state: "OPEN".to_string(),
                mergeable: "MERGEABLE".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let live = tacho_core::LiveState::default();
        let mut hands = DaemonHands {
            home: home.to_path_buf(),
            live_state: live.clone(),
            appender: None,
        };
        let mut opts = TickOptions::new(&live);
        opts.repo = Some(repo);
        opts.forge = Some(&forge);
        opts.procs = Some(Vec::new());

        let out = run_tick(home, &mut store, &mut hands, opts).unwrap();
        assert_eq!(out.plan.cleared_main_broken.len(), 1);
        assert_eq!(out.plan.cleared_main_broken[0].batch_id, "b-main-broken");
        let after = store.load().unwrap();
        assert!(after.batches.is_empty());
        assert_eq!(after.open_prs_cache.len(), 1);
        assert_eq!(after.main_ci.as_ref().unwrap().sha, sha2);
    }

    #[test]
    fn test_run_tick_forwards_owner_notifications_and_watchdog_alerts_to_forge_issues() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
        store
            .mutate(None, |st| {
                st.batches.push(BatchRun {
                    id: "b-red".to_string(),
                    group: "core".to_string(),
                    prs: vec![7],
                    shas: BTreeMap::from([(7, "a".repeat(40))]),
                    main_sha: "b".repeat(40),
                    areas: vec!["core".to_string()],
                    checks: Vec::new(),
                    mode: BatchMode::Presubmit,
                    status: "failed".to_string(),
                    started_at: 100,
                    finished_at: Some(120),
                    tree: None,
                    pid: None,
                    log: "test failure".to_string(),
                    culprit: Some(7),
                    skipped: Vec::new(),
                    flakes: Vec::new(),
                });
                st.max_workers = 1;
                st.todo.push(TodoItem {
                    key: "pending-work-1".to_string(),
                    brief: "Work item 1".to_string(),
                    area: "core".to_string(),
                    ..Default::default()
                });
                st.todo.push(TodoItem {
                    key: "pending-work-2".to_string(),
                    brief: "Work item 2".to_string(),
                    area: "core".to_string(),
                    ..Default::default()
                });
                st.last_tick_ts = Some(1_000);
            })
            .unwrap();

        let forge = MockForge::default();
        let live = tacho_core::LiveState::default();
        let mut hands = DaemonHands {
            home: home.to_path_buf(),
            live_state: live.clone(),
            appender: None,
        };
        let mut opts = TickOptions::new(&live);
        opts.explicit_main_sha = Some("b".repeat(40));
        opts.forge = Some(&forge);
        opts.procs = Some(Vec::new());
        opts.now_override = Some(2_000); // > 600s since last_tick_ts=1_000 with pending todo -> watchdog_alert

        let out = run_tick(home, &mut store, &mut hands, opts).unwrap();
        assert!(!out.plan.owner_notifications.is_empty());
        assert!(out.plan.watchdog_alert.is_some());

        let issues = forge.issues.lock().unwrap();
        assert!(
            issues.iter().any(|(title, _)| title == "tacho is down"),
            "expected 'tacho is down' issue on MockForge, got: {:?}",
            *issues
        );
        assert!(
            issues
                .iter()
                .any(|(title, _)| title == "tacho alert: batch-red:b-red"),
            "expected batch-red alert issue on MockForge, got: {:?}",
            *issues
        );
    }

    #[test]
    fn test_run_tick_retries_failed_owner_notifications_and_watchdog_alerts_on_next_tick() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
        store
            .mutate(None, |st| {
                st.batches.push(BatchRun {
                    id: "b-red".to_string(),
                    group: "core".to_string(),
                    prs: vec![7],
                    shas: BTreeMap::from([(7, "a".repeat(40))]),
                    main_sha: "b".repeat(40),
                    areas: vec!["core".to_string()],
                    checks: Vec::new(),
                    mode: BatchMode::Presubmit,
                    status: "failed".to_string(),
                    started_at: 100,
                    finished_at: Some(120),
                    tree: None,
                    pid: None,
                    log: "test failure".to_string(),
                    culprit: Some(7),
                    skipped: Vec::new(),
                    flakes: Vec::new(),
                });
                st.max_workers = 1;
                let my_pid = std::process::id();
                let my_st = identity::proc_pid_starttime(my_pid).expect("own starttime");
                st.agents.insert(
                    "busy-worker".to_string(),
                    AgentRun {
                        key: "busy-worker".to_string(),
                        pid: Some(my_pid),
                        starttime: Some(my_st),
                        started_at: 1_000,
                        ..Default::default()
                    },
                );
                st.todo.push(TodoItem {
                    key: "pending-work-1".to_string(),
                    brief: "Work item 1".to_string(),
                    area: "core".to_string(),
                    ..Default::default()
                });
                st.last_tick_ts = Some(1_000);
            })
            .unwrap();

        let broken_forge = tacho_core::decide::plugin::CliForgePlugin {
            name: "broken-gh".to_string(),
            cli_bin: "/nonexistent/gh-binary".to_string(),
            repo: Some("owner/repo".to_string()),
            allow_merge: false,
        };
        let live = tacho_core::LiveState::default();
        let mut hands = DaemonHands {
            home: home.to_path_buf(),
            live_state: live.clone(),
            appender: None,
        };
        let mut opts1 = TickOptions::new(&live);
        opts1.explicit_main_sha = Some("b".repeat(40));
        opts1.forge = Some(&broken_forge);
        opts1.procs = Some(Vec::new());
        opts1.now_override = Some(2_000);

        let err1 = run_tick(home, &mut store, &mut hands, opts1).unwrap_err();
        assert!(
            err1.to_string().contains("set_issue_failed")
                && err1.to_string().contains("watchdog_alert_failed"),
            "expected set_issue_failed and watchdog_alert_failed in tick error, got: {err1}"
        );
        let decisions1 = store.recent_decisions(50, None, None).unwrap();
        assert!(
            decisions1.iter().any(|d| d.action == "set_issue_failed"),
            "expected set_issue_failed on broken forge, got: {:?}",
            decisions1
        );
        assert!(
            decisions1
                .iter()
                .any(|d| d.action == "watchdog_alert_failed"),
            "expected watchdog_alert_failed on broken forge, got: {:?}",
            decisions1
        );

        let st_after_fail = store.load().unwrap();
        assert!(
            !st_after_fail.notified_failures.contains("batch-red:b-red"),
            "failed owner notification must be reverted from notified_failures so next tick retries"
        );
        assert_eq!(
            st_after_fail.last_watchdog_alert_ts, None,
            "failed watchdog alert must revert last_watchdog_alert_ts so next tick retries"
        );

        // Next tick with a working forge retries and succeeds.
        store
            .mutate(None, |st| {
                st.last_tick_ts = Some(1_000);
            })
            .unwrap();
        let working_forge = MockForge::default();
        let mut opts2 = TickOptions::new(&live);
        opts2.explicit_main_sha = Some("b".repeat(40));
        opts2.forge = Some(&working_forge);
        opts2.procs = Some(Vec::new());
        opts2.now_override = Some(2_005);

        let _out2 = run_tick(home, &mut store, &mut hands, opts2).unwrap();
        let st_after_ok = store.load().unwrap();
        assert!(st_after_ok.notified_failures.contains("batch-red:b-red"));
        assert_eq!(st_after_ok.last_watchdog_alert_ts, Some(2_005));
    }

    #[test]
    fn test_tidy_refuses_plain_subdir_inside_enclosing_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let parent_repo = dir.path().join("srv-fleet");
        std::fs::create_dir_all(&parent_repo).unwrap();

        let run_git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&parent_repo)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["config", "user.email", "t@example.com"]);
        run_git(&["config", "user.name", "Tester"]);
        std::fs::write(parent_repo.join("root.txt"), "root\n").unwrap();
        run_git(&["add", "root.txt"]);
        run_git(&["commit", "-q", "-m", "initial"]);

        std::fs::write(parent_repo.join("untracked_parent.txt"), "do not stage\n").unwrap();
        let plain_subdir = parent_repo.join("trees").join("plain-dir");
        std::fs::create_dir_all(&plain_subdir).unwrap();
        std::fs::write(plain_subdir.join("work.txt"), "precious\n").unwrap();

        let mut store = FleetStore::open(&parent_repo.join("events.sqlite")).unwrap();
        store
            .mutate(None, |st| {
                st.worktrees.insert(
                    "plain-dir".to_string(),
                    crate::fleet::WorktreeEntry {
                        slug: "plain-dir".to_string(),
                        path: plain_subdir.to_string_lossy().to_string(),
                        branch: "feat/plain".to_string(),
                        base_sha: String::new(),
                        base_pr: None,
                        port: 3100,
                        port_last: 3119,
                        created_at: 100,
                    },
                );
            })
            .unwrap();

        let decisions = vec![
            tacho_core::decide::watch::TidyDecision::RemoveCheckout {
                path: plain_subdir.to_string_lossy().to_string(),
                rescue: true,
                because: "test remove checkout on plain subdir".to_string(),
            },
            tacho_core::decide::watch::TidyDecision::RemoveTree {
                path: plain_subdir.to_string_lossy().to_string(),
                rescue: false,
                because: "test remove tree on plain subdir".to_string(),
            },
            tacho_core::decide::watch::TidyDecision::RemoveTree {
                path: parent_repo.to_string_lossy().to_string(),
                rescue: true,
                because: "test remove tree on main checkout".to_string(),
            },
            tacho_core::decide::watch::TidyDecision::RemoveCheckout {
                path: parent_repo.to_string_lossy().to_string(),
                rescue: false,
                because: "test remove checkout on main checkout".to_string(),
            },
        ];
        assert!(
            tidy::rescue_worktree(&mut store, &plain_subdir, 1_000).is_err(),
            "rescue_worktree must refuse a plain subdir inside an enclosing repo"
        );
        let removed = tidy::apply_tidy_decisions(&mut store, &decisions, 1_000, false).unwrap();
        assert!(
            removed.is_empty(),
            "neither plain subdir nor main checkout may ever be removed"
        );
        assert!(
            plain_subdir.join("work.txt").exists(),
            "plain subdir contents must remain intact"
        );
        assert!(
            parent_repo.join("root.txt").exists(),
            "main checkout must remain intact"
        );

        // Verify the enclosing parent git repo has zero staged changes (git add -A did NOT run in parent_repo).
        let cached = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(&parent_repo)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&cached.stdout).trim(),
            "",
            "enclosing parent repo must not have staged files"
        );
    }

    #[test]
    fn test_retired_main_checkout_survives_tidy_with_index_and_untracked_files_intact() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let holder = dir.path().join("holder");
        let main_checkout = holder.join("repo");
        std::fs::create_dir_all(&main_checkout).unwrap();

        let run_git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&main_checkout)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["config", "user.email", "t@example.com"]);
        run_git(&["config", "user.name", "Tester"]);
        std::fs::write(main_checkout.join("README.md"), "hello\n").unwrap();
        run_git(&["add", "README.md"]);
        run_git(&["commit", "-q", "-m", "initial"]);

        // Stage a file in the main checkout's index and create an untracked file.
        std::fs::write(main_checkout.join("staged.txt"), "staged work\n").unwrap();
        run_git(&["add", "staged.txt"]);
        std::fs::write(main_checkout.join("untracked.txt"), "untracked work\n").unwrap();

        let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
        let main_path_str = main_checkout.to_string_lossy().to_string();
        let holder_path_str = holder.to_string_lossy().to_string();
        store
            .mutate(None, |st| {
                st.retired.insert("retired-main".to_string());
                st.retired.insert("retired-holder".to_string());
                st.worktrees.insert(
                    "retired-main".to_string(),
                    crate::fleet::WorktreeEntry {
                        slug: "retired-main".to_string(),
                        path: main_path_str.clone(),
                        branch: "main".to_string(),
                        base_sha: String::new(),
                        base_pr: None,
                        port: 3200,
                        port_last: 3219,
                        created_at: 100,
                    },
                );
                st.worktrees.insert(
                    "retired-holder".to_string(),
                    crate::fleet::WorktreeEntry {
                        slug: "retired-holder".to_string(),
                        path: holder_path_str.clone(),
                        branch: "main".to_string(),
                        base_sha: String::new(),
                        base_pr: None,
                        port: 3220,
                        port_last: 3239,
                        created_at: 100,
                    },
                );
                st.agents.insert(
                    "retired-main".to_string(),
                    AgentRun {
                        key: "retired-main".to_string(),
                        worktree: main_path_str.clone(),
                        started_at: 100,
                        finished_at: Some(200),
                        exit_code: Some(0),
                        ..Default::default()
                    },
                );
            })
            .unwrap();

        // 1. Run a full autonomous tick: tidy must not plan or execute removal of the main checkout.
        let live = tacho_core::LiveState::default();
        let mut hands = DaemonHands {
            home: home.clone(),
            live_state: live.clone(),
            appender: None,
        };
        let mut opts = TickOptions::new(&live);
        opts.explicit_main_sha = Some("a".repeat(40));
        opts.procs = Some(Vec::new());
        opts.now_override = Some(100_000);
        let out = run_tick(&home, &mut store, &mut hands, opts).unwrap();
        assert!(
            out.plan.tidy_decisions.is_empty(),
            "decide must never plan tidy removal for the main checkout or its holder: {:?}",
            out.plan.tidy_decisions
        );

        // 2. Also invoke apply_tidy_decisions directly with RemoveTree and RemoveCheckout (with and without rescue).
        let explicit = vec![
            tacho_core::decide::watch::TidyDecision::RemoveTree {
                path: main_path_str.clone(),
                rescue: true,
                because: "retired worktree".to_string(),
            },
            tacho_core::decide::watch::TidyDecision::RemoveCheckout {
                path: main_path_str.clone(),
                rescue: false,
                because: "stale checkout".to_string(),
            },
            tacho_core::decide::watch::TidyDecision::RemoveTree {
                path: holder_path_str.clone(),
                rescue: false,
                because: "holder of main checkout".to_string(),
            },
            tacho_core::decide::watch::TidyDecision::RemoveCheckout {
                path: holder_path_str.clone(),
                rescue: true,
                because: "holder checkout".to_string(),
            },
        ];
        let removed = tidy::apply_tidy_decisions(&mut store, &explicit, 100_000, false).unwrap();
        assert!(removed.is_empty());

        // Main checkout, its staged index, and its untracked files are all intact.
        assert!(main_checkout.join(".git").is_dir());
        assert_eq!(
            std::fs::read_to_string(main_checkout.join("staged.txt")).unwrap(),
            "staged work\n"
        );
        assert_eq!(
            std::fs::read_to_string(main_checkout.join("untracked.txt")).unwrap(),
            "untracked work\n"
        );
        let cached = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(&main_checkout)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&cached.stdout).trim(),
            "staged.txt",
            "main checkout git index must remain intact"
        );
    }

    #[test]
    fn test_worker_running_in_main_checkout_is_stopped_with_page() {
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let main_checkout = dir.path().join("repo");
        std::fs::create_dir_all(&main_checkout).unwrap();

        let st = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&main_checkout)
            .status()
            .unwrap();
        assert!(st.success());

        // Spawn a background worker process group leader that also spawns a child in its group,
        // whose recorded worktree is the main checkout.
        let mut child = unsafe {
            use std::os::unix::process::CommandExt;
            std::process::Command::new("bash")
                .args([
                    "-c",
                    "{ cat >/dev/null; } <&0 & sub=$!; echo $sub; wait $sub",
                ])
                .current_dir(&main_checkout)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                })
                .spawn()
                .unwrap()
        };
        let _child_stdin = child.stdin.take();
        let mut child_stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut sub_pid_line = String::new();
        child_stdout.read_line(&mut sub_pid_line).unwrap();
        let sub_pid: u32 = sub_pid_line.trim().parse().expect("sub-child pid");
        assert!(identity::pid_alive(sub_pid), "sub-child must be alive");

        let pid = child.id();
        let starttime = identity::proc_pid_starttime(pid).expect("child starttime");

        let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
        let main_str = main_checkout.to_string_lossy().to_string();
        store
            .mutate(None, |s| {
                s.repo_root = Some(main_str.clone());
                s.agents.insert(
                    "rogue-worker".to_string(),
                    AgentRun {
                        key: "rogue-worker".to_string(),
                        pid: Some(pid),
                        starttime: Some(starttime),
                        worktree: main_str.clone(),
                        started_at: 1_000,
                        ..Default::default()
                    },
                );
            })
            .unwrap();

        let forge = MockForge::default();
        let live = tacho_core::LiveState::default();
        let mut hands = DaemonHands {
            home: home.clone(),
            live_state: live.clone(),
            appender: None,
        };
        let mut opts = TickOptions::new(&live);
        opts.explicit_main_sha = Some("a".repeat(40));
        opts.forge = Some(&forge);
        opts.procs = Some(vec![ProcEntry {
            pid,
            ppid: 1,
            pgid: pid,
            state: "S".to_string(),
            ..Default::default()
        }]);
        opts.now_override = Some(1_010);

        let out = run_tick(&home, &mut store, &mut hands, opts).unwrap();
        let wait_status = child.wait().unwrap();
        assert!(
            !wait_status.success(),
            "worker in main checkout must be terminated by supervision"
        );
        identity::wait_for_pid_exit_event(sub_pid).unwrap();
        assert!(
            !identity::pid_alive(sub_pid),
            "child process in main-checkout worker's process group must also be terminated"
        );
        assert!(
            out.plan.supervision_actions.iter().any(
                |a| matches!(a, SuperviseAction::Escalate { key, .. } if key == "rogue-worker")
            ),
            "expected Escalate supervision action for rogue-worker, got: {:?}",
            out.plan.supervision_actions
        );
        assert!(
            !out.plan.owner_notifications.is_empty(),
            "expected owner notification/page when stopping worker in main checkout"
        );
        let final_state = store.load().unwrap();
        assert!(
            final_state.holds.contains_key("rogue-worker"),
            "rogue-worker must be placed on hold"
        );
        assert_eq!(final_state.agents["rogue-worker"].finished_at, Some(1_010));
    }

    #[test]
    fn test_holder_directory_worker_is_not_mistaken_for_main_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let fleet_main = dir.path().join("fleet-repo");
        std::fs::create_dir_all(&fleet_main).unwrap();
        let holder = dir.path().join("holder");
        let child_repo = holder.join("sub-repo");
        std::fs::create_dir_all(&child_repo).unwrap();

        for repo in [&fleet_main, &child_repo] {
            let st = std::process::Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(st.success());
        }
        assert!(
            !observe::resolves_to_main_checkout(&holder),
            "a holder directory containing a child repo must not resolve to main checkout"
        );

        let mut child = std::process::Command::new("cat")
            .current_dir(&holder)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let _stdin = child.stdin.take();
        let pid = child.id();
        let starttime = identity::proc_pid_starttime(pid).expect("child starttime");

        let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
        store
            .mutate(None, |s| {
                s.repo_root = Some(fleet_main.to_string_lossy().to_string());
                s.agents.insert(
                    "holder-worker".to_string(),
                    AgentRun {
                        key: "holder-worker".to_string(),
                        pid: Some(pid),
                        starttime: Some(starttime),
                        worktree: holder.to_string_lossy().to_string(),
                        started_at: 1_000,
                        ..Default::default()
                    },
                );
            })
            .unwrap();

        let live = tacho_core::LiveState::default();
        let mut hands = DaemonHands {
            home: home.clone(),
            live_state: live.clone(),
            appender: None,
        };
        let mut opts = TickOptions::new(&live);
        opts.explicit_main_sha = Some("a".repeat(40));
        opts.procs = Some(vec![ProcEntry {
            pid,
            ppid: 1,
            pgid: pid,
            state: "S".to_string(),
            ..Default::default()
        }]);
        opts.now_override = Some(1_010);

        let out = run_tick(&home, &mut store, &mut hands, opts).unwrap();
        assert!(
            out.obs.main_checkout_workers.is_empty(),
            "holder directory must not be flagged as fleet main checkout"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "worker in holder directory must not be killed"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_ordinary_stall_escalation_notifies_owner_without_killing_worker() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let wt = dir.path().join("wt-1");
        std::fs::create_dir_all(&wt).unwrap();

        let mut child = std::process::Command::new("cat")
            .current_dir(&wt)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let _stdin = child.stdin.take();
        let pid = child.id();
        let starttime = identity::proc_pid_starttime(pid).expect("child starttime");

        let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
        store
            .mutate(None, |s| {
                s.agents.insert(
                    "stalled-worker".to_string(),
                    AgentRun {
                        key: "stalled-worker".to_string(),
                        pid: Some(pid),
                        starttime: Some(starttime),
                        worktree: wt.to_string_lossy().to_string(),
                        started_at: 1_000,
                        ..Default::default()
                    },
                );
                s.nudged.insert("stalled-worker".to_string());
                s.samples.insert(
                    "stalled-worker".to_string(),
                    vec![tacho_core::decide::watch::ProgressSample {
                        ts: 1_000,
                        log_bytes: 0,
                        head_sha: String::new(),
                        dirty_files: 0,
                        cpu_ticks: 100,
                        newest_mtime: None,
                    }],
                );
            })
            .unwrap();

        let live = tacho_core::LiveState::default();
        let mut hands = DaemonHands {
            home: home.clone(),
            live_state: live.clone(),
            appender: None,
        };
        let t_escalate = 1_000 + STALLED_SECS + 120;
        let mut opts = TickOptions::new(&live);
        opts.explicit_main_sha = Some("a".repeat(40));
        opts.now_override = Some(t_escalate);
        opts.procs = Some(vec![ProcEntry {
            pid,
            ppid: 1,
            pgid: pid,
            state: "S".to_string(),
            cpu_ticks: 105, // alive (>100) but idle (< 5 ticks/s)
            ..Default::default()
        }]);

        let out = run_tick(&home, &mut store, &mut hands, opts).unwrap();
        assert!(
            out.plan.supervision_actions.iter().any(
                |a| matches!(a, SuperviseAction::Escalate { key, .. } if key == "stalled-worker")
            ),
            "expected Escalate for already-nudged stalled worker, got: {:?}",
            out.plan.supervision_actions
        );
        assert!(
            out.plan
                .owner_notifications
                .iter()
                .any(|n| n.kind == "stalled" && n.subject.contains("stalled-worker")),
            "expected owner notification for escalated stall, got: {:?}",
            out.plan.owner_notifications
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "ordinary stall escalation must NOT kill the worker"
        );
        let st = store.load().unwrap();
        assert!(
            !st.holds.contains_key("stalled-worker"),
            "ordinary stall escalation must NOT hold the task"
        );
        assert!(
            st.agents["stalled-worker"].finished_at.is_none(),
            "ordinary stall escalation must leave the worker running"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_root_disk_check_in_tick_warns_and_prunes_volumes_and_clears_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut store = FleetStore::open(&home.join("events.sqlite")).unwrap();
        let live = tacho_core::LiveState::default();
        let mut hands = DaemonHands {
            home: home.clone(),
            live_state: live.clone(),
            appender: None,
        };
        let gib = 1024 * 1024 * 1024u64;

        // 1. Critical root disk (85% used, 15 GiB free): warns owner and records prune_root_disk_volumes.
        let mut opts1 = TickOptions::new(&live);
        opts1.explicit_main_sha = Some("a".repeat(40));
        opts1.now_override = Some(2_000);
        opts1.procs = Some(Vec::new());
        opts1.root_disk_bytes = Some((100 * gib, 15 * gib));
        let out1 = run_tick(&home, &mut store, &mut hands, opts1).unwrap();
        assert!(out1.plan.root_disk_action.as_ref().unwrap().warn);
        assert!(
            out1.plan
                .root_disk_action
                .as_ref()
                .unwrap()
                .prune_docker_volumes
        );
        assert!(
            out1.plan
                .owner_notifications
                .iter()
                .any(|n| n.kind == "root-disk" && n.body.contains("The root disk is filling")),
            "expected root-disk owner notification, got: {:?}",
            out1.plan.owner_notifications
        );
        let decisions = store.recent_decisions(10, None, None).unwrap();
        assert!(
            decisions
                .iter()
                .any(|d| d.action == "prune_root_disk_volumes" && d.target == "root-disk"),
            "expected prune_root_disk_volumes decision, got: {:?}",
            decisions
        );

        // 2. Recovery (50% used, 50 GiB free): clears root-disk attention and resets notification latch.
        let mut opts2 = TickOptions::new(&live);
        opts2.explicit_main_sha = Some("a".repeat(40));
        opts2.now_override = Some(2_060);
        opts2.procs = Some(Vec::new());
        opts2.root_disk_bytes = Some((100 * gib, 50 * gib));
        let out2 = run_tick(&home, &mut store, &mut hands, opts2).unwrap();
        assert!(!out2.plan.root_disk_action.as_ref().unwrap().warn);
        assert!(!out2
            .plan
            .attention_items
            .iter()
            .any(|a| a.kind == "root-disk"));
        let st2 = store.load().unwrap();
        assert!(
            !st2.notified_failures
                .iter()
                .any(|k| k.starts_with("attention:root-disk:")),
            "recovery must clear root-disk notification latch"
        );
    }
}
